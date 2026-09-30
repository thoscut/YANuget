//! Upstream mirroring: read-through caching of a public NuGet feed.
//!
//! When a feed has a `[feeds.mirror]` upstream configured and a client asks for
//! a package the feed does not have yet, the server fetches that package's
//! versions from the upstream V3 feed, streams each `.nupkg` to disk and indexes
//! it into the local feed — after which every later request is served locally.
//!
//! Mirrored versions go through the same [`crate::indexing`] pipeline as a push,
//! so the streaming/large-package guarantees and the feed's license policy
//! apply. When the feed `requires_approval`, mirrored versions land **pending**
//! and are withheld from clients until an admin approves them in `/admin` —
//! turning the mirror into a curated, approval-gated cache.

use std::path::Path;

use futures::StreamExt;
use tokio::sync::OnceCell;

use crate::config::{LicensePolicyConfig, MirrorAuthConfig, MirrorConfig};
use crate::database::PackageDatabase;
use crate::error::{Error, Result};
use crate::indexing::{self, IndexOptions};
use crate::storage::PackageStorage;
use crate::streaming;
use crate::version::NuGetVersion;

/// A client for one upstream V3 feed, with its service-index resources resolved
/// lazily on first use and cached thereafter.
pub struct MirrorClient {
    client: reqwest::Client,
    upstream: String,
    /// The service index's origin: the only one the credentials are sent to.
    upstream_origin: Origin,
    /// The configured credentials, attached per request by [`Self::send`] —
    /// never as client-wide defaults, which go wherever a request goes.
    credentials: reqwest::header::HeaderMap,
    /// Permit upstream resource URLs that resolve to private/loopback hosts.
    allow_private_upstream: bool,
    /// Cap on a single mirrored `.nupkg`, in bytes.
    max_package_size_bytes: Option<u64>,
    /// Cap on versions fetched for one read-through miss.
    max_versions_per_package: Option<usize>,
    /// Deadline for one metadata request (service index, search or catalog
    /// page, version list), from `timeout_secs`.
    timeout: std::time::Duration,
    /// Deadline for one whole `.nupkg` download. `None` leaves only the read
    /// timeout, which bounds how long the upstream may go silent.
    download_deadline: Option<std::time::Duration>,
    resources: OnceCell<MirrorResources>,
}

impl std::fmt::Debug for MirrorClient {
    /// The upstream without its userinfo, and the credentials by header name
    /// only: a `{:?}` in a log line must not print the secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorClient")
            .field("upstream", &redact_url(&self.upstream))
            .field("credentials", &self.credentials.keys().collect::<Vec<_>>())
            .field("allow_private_upstream", &self.allow_private_upstream)
            .field("max_package_size_bytes", &self.max_package_size_bytes)
            .field("max_versions_per_package", &self.max_versions_per_package)
            .field("timeout", &self.timeout)
            .field("download_deadline", &self.download_deadline)
            .finish_non_exhaustive()
    }
}

/// `(scheme, host, port)`: what two URLs must share for a credential meant for
/// one to be sent to the other.
type Origin = (String, String, u16);

fn origin(url: &reqwest::Url) -> Option<Origin> {
    Some((
        url.scheme().to_string(),
        url.host_str()?.to_ascii_lowercase(),
        url.port_or_known_default()?,
    ))
}

/// How many redirects one upstream request may follow.
const MAX_REDIRECTS: usize = 5;

/// Cap on one upstream JSON document (service index, version list, search,
/// catalog or registration page). The largest real ones — catalog pages — are
/// well under a megabyte; the cap only stops an upstream from streaming an
/// endless body into memory.
const MAX_JSON_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
struct MirrorResources {
    /// `PackageBaseAddress/3.0.0`, with a trailing slash.
    package_base: String,
    /// `SearchQueryService` (any advertised version), when present. Used to
    /// enumerate the upstream's package ids for a full migration.
    search: Option<String>,
    /// `Catalog/3.0.0`, when present. The enumeration fallback for feeds whose
    /// search service is capped or absent.
    catalog: Option<String>,
}

/// How long one read-through miss may spend fetching before it gives up and
/// answers with what it has.
///
/// `ensure_package` downloads up to `max_versions_per_package` (50 by default)
/// `.nupkg`s one after another, each bounded only by the per-request timeout —
/// so a single `GET /v3/package/{id}/index.json`, which needs no
/// authentication, could hold a connection open for twenty-five minutes. The
/// mirror is a cache: stopping early is not a failure, because the versions
/// already fetched are kept and the next request continues from there.
const MIRROR_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// Default cap on a single mirrored `.nupkg` when neither the feed nor the
/// server configured one.
const DEFAULT_MIRROR_MAX_PACKAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Resolves upstream host names, dropping every private, loopback or
/// link-local address unless the operator allowed a private upstream.
///
/// Checking a URL before requesting it cannot stop a *name* that resolves to
/// such an address (`metadata.evil.example` with an `A` record of
/// `169.254.169.254`, or `localhost.` with its trailing dot), and a check that
/// resolves separately from the connection is undone by a DNS answer that
/// changes in between. The resolver is where the connection's own addresses
/// come from, so filtering here covers every URL the mirror fetches, every
/// redirect hop and every later reconnect at once.
///
/// Literal IP hosts never reach a resolver; [`MirrorClient::check_url`] and
/// the redirect handling classify those.
struct GuardedResolver {
    /// A configured outbound proxy's host, resolved without the filter: the
    /// operator named it, and it is routinely on the private network.
    proxy_host: Option<String>,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let exempt = self
            .proxy_host
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(&host));
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if exempt {
                return Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs);
            }
            let public: Vec<std::net::SocketAddr> = addrs
                .iter()
                .copied()
                .filter(|a| !crate::proxy::is_private_ip_addr(a.ip()))
                .collect();
            if public.is_empty() {
                if let Some(first) = addrs.first() {
                    return Err(format!(
                        "upstream host {host} resolves to the private address {}; \
                         set allow_private_upstream = true to permit it",
                        first.ip()
                    )
                    .into());
                }
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

impl MirrorClient {
    /// Build a client from a feed's [`MirrorConfig`]. Returns `None` when
    /// mirroring is disabled for the feed.
    pub fn from_config(config: &MirrorConfig) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        Self::build(config, false)
    }

    /// Build the client `yanuget migrate` copies a source with.
    ///
    /// Unlike the read-through mirror, a migration is a command an operator
    /// runs by hand, like `curl`, so it uses the shell's `HTTP(S)_PROXY` when
    /// no proxy is configured; and a download may take as long as it needs
    /// (see [`Self::set_download_deadline`]).
    pub fn for_migration(config: &MirrorConfig) -> Option<Self> {
        let mut client = Self::build(config, true)?;
        client.set_download_deadline(None);
        Some(client)
    }

    fn build(config: &MirrorConfig, env_proxy: bool) -> Option<Self> {
        let upstream_origin = reqwest::Url::parse(&config.upstream)
            .ok()
            .as_ref()
            .and_then(origin)?;
        // Connecting and every read are bounded on the client. A read timeout
        // restarts with each chunk received, so it limits how long the upstream
        // may go silent, not how long a transfer may take; each request adds a
        // total deadline of its own on top (`timeout`, `download_deadline`).
        let timeout = std::time::Duration::from_secs(config.timeout_secs.max(1));
        let mut builder = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .user_agent(concat!("yanuget/", env!("CARGO_PKG_VERSION")))
            // Redirects are followed by `send`, which knows which hops may
            // carry the credentials; reqwest's policy cannot be told.
            .redirect(reqwest::redirect::Policy::none());
        // reqwest honours `HTTP(S)_PROXY` from the environment by default. For
        // a server that is a surprise egress path chosen by whoever set up the
        // process environment, and a proxy resolves names itself, past the
        // resolver below. So a proxy is used only when the feed names one.
        let mut proxy_host = None;
        match &config.proxy {
            Some(proxy) => {
                builder = builder.proxy(reqwest::Proxy::all(proxy.as_str()).ok()?);
                proxy_host = reqwest::Url::parse(proxy)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string));
            }
            None if env_proxy => {}
            None => builder = builder.no_proxy(),
        }
        if !config.allow_private_upstream {
            builder = builder.dns_resolver(std::sync::Arc::new(GuardedResolver { proxy_host }));
        }
        Some(Self {
            client: builder.build().ok()?,
            upstream: config.upstream.clone(),
            upstream_origin,
            credentials: auth_headers(&config.auth),
            allow_private_upstream: config.allow_private_upstream,
            max_package_size_bytes: config.max_package_size_bytes,
            max_versions_per_package: config.max_versions_per_package,
            timeout,
            download_deadline: Some(timeout),
            resources: OnceCell::new(),
        })
    }

    /// Replace the deadline on one whole `.nupkg` download. `None` removes it:
    /// the upstream may then take as long as it needs, provided it never goes
    /// silent for longer than the read timeout.
    ///
    /// The read-through mirror keeps its deadline, because an anonymous request
    /// starts that fetch and an upstream that trickles bytes must not hold it
    /// open. `migrate` removes it: an operator copying a feed wants its large
    /// packages, and a deadline on the whole transfer fails every package the
    /// source cannot send within `timeout_secs`.
    pub fn set_download_deadline(&mut self, deadline: Option<std::time::Duration>) {
        self.download_deadline = deadline;
    }

    /// Reject an upstream-supplied resource URL that we should not fetch.
    ///
    /// The service index is operator-configured, but every resource URL inside
    /// it — and therefore every URL the mirror actually fetches — is chosen by
    /// the upstream. A hostile or compromised upstream that answers with
    /// `http://169.254.169.254/…` or an address on the server's own network
    /// turns the mirror into an SSRF probe, so non-HTTP schemes and
    /// private/loopback/link-local hosts are refused unless the operator opted
    /// in (which a self-hosted upstream on a private network legitimately does).
    ///
    /// This classifies literal addresses and reserved names; a DNS name that
    /// resolves to a private address is stopped by [`GuardedResolver`] when the
    /// connection is made.
    fn check_url(&self, url: &str, what: &str) -> Result<()> {
        let parsed = reqwest::Url::parse(url)
            .map_err(|e| Error::Other(anyhow::anyhow!("upstream {what} url is invalid: {e}")))?;
        self.check_parsed(&parsed, what)
    }

    fn check_parsed(&self, url: &reqwest::Url, what: &str) -> Result<()> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::Other(anyhow::anyhow!(
                "upstream {what} url uses unsupported scheme {:?}",
                url.scheme()
            )));
        }
        if !self.allow_private_upstream {
            if let Some(host) = url.host_str() {
                if crate::proxy::is_private_host(host) {
                    return Err(Error::Other(anyhow::anyhow!(
                        "upstream {what} url points at the private address {host}; \
                         set allow_private_upstream = true to permit it"
                    )));
                }
            }
        }
        Ok(())
    }

    /// [`Self::check_url`] as a predicate, logging why a resource was dropped.
    fn log_check(&self, url: &str, what: &str) -> bool {
        match self.check_url(url, what) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(url = %redact_url(url), error = %e, "ignoring upstream resource");
                false
            }
        }
    }

    /// Whether a request to `url` may carry the configured credentials: only
    /// when it goes to the upstream's own scheme, host and port.
    ///
    /// Every other URL the mirror fetches was named by the upstream — the
    /// `PackageBaseAddress`, the search and catalog resources, catalog pages,
    /// redirect targets — and a credential sent wherever those point is one a
    /// hostile or compromised upstream can collect by pointing them at itself.
    fn carries_credentials(&self, url: &reqwest::Url) -> bool {
        origin(url).as_ref() == Some(&self.upstream_origin)
    }

    /// GET `url`, following redirects here rather than in reqwest.
    ///
    /// reqwest's own redirect handling cannot be told which headers are
    /// secret: it drops `Authorization` when the host or port changes, but
    /// not an operator's `X-Feed-Key`, and not on a downgrade from https to
    /// http on the same host. So each hop is a request of its own. Every hop
    /// is vetted like the first URL (a `302` to `http://169.254.169.254/`
    /// otherwise walks past the check), a hop from https to http is refused,
    /// and the credentials go only on a hop whose origin is the upstream's.
    /// A download a credentialed upstream hands off to a CDN or blob store is
    /// therefore followed *without* them — which is what such signed links
    /// are for — instead of being stored as the redirect's body.
    ///
    /// `deadline` bounds the whole exchange, every hop and the body included.
    async fn send(
        &self,
        url: &str,
        what: &str,
        deadline: Option<std::time::Duration>,
    ) -> Result<reqwest::Response> {
        let started = tokio::time::Instant::now();
        let mut current = reqwest::Url::parse(url)
            .map_err(|e| Error::Other(anyhow::anyhow!("upstream {what} url is invalid: {e}")))?;
        self.check_parsed(&current, what)?;
        for _ in 0..=MAX_REDIRECTS {
            let mut request = self.client.get(current.clone());
            if self.carries_credentials(&current) {
                request = request.headers(self.credentials.clone());
            }
            if let Some(deadline) = deadline {
                let left = deadline.saturating_sub(started.elapsed());
                if left.is_zero() {
                    return Err(Error::Other(anyhow::anyhow!(
                        "upstream {what} {} timed out",
                        redact_url(current.as_str())
                    )));
                }
                request = request.timeout(left);
            }
            let resp = request
                .send()
                .await
                .map_err(|e| request_error(&current, e))?;
            if !resp.status().is_redirection() {
                return Ok(resp);
            }
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    Error::Other(anyhow::anyhow!(
                        "upstream {what} {} answered {} without a usable Location",
                        redact_url(current.as_str()),
                        resp.status()
                    ))
                })?;
            current = self.redirect_target(&current, location, what)?;
        }
        Err(Error::Other(anyhow::anyhow!(
            "upstream {what} {} redirected more than {MAX_REDIRECTS} times",
            redact_url(url)
        )))
    }

    /// Where a redirect from `current` to `location` leads, if it may be
    /// followed at all.
    fn redirect_target(
        &self,
        current: &reqwest::Url,
        location: &str,
        what: &str,
    ) -> Result<reqwest::Url> {
        let next = current.join(location).map_err(|e| {
            Error::Other(anyhow::anyhow!(
                "upstream {what} {} redirected to an invalid url: {e}",
                redact_url(current.as_str())
            ))
        })?;
        if current.scheme() == "https" && next.scheme() == "http" {
            return Err(Error::Other(anyhow::anyhow!(
                "upstream {what} {} redirected from https to http ({}); refusing the downgrade",
                redact_url(current.as_str()),
                redact_url(next.as_str())
            )));
        }
        self.check_parsed(&next, what)?;
        Ok(next)
    }

    /// Turn a non-success status into an error that says where it came from,
    /// and why, when the likely cause is that credentials were withheld.
    fn check_status(&self, resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let hint = if matches!(status.as_u16(), 401 | 403)
            && !self.credentials.is_empty()
            && !self.carries_credentials(resp.url())
        {
            let (scheme, host, port) = &self.upstream_origin;
            format!("; credentials are only sent to {scheme}://{host}:{port}")
        } else {
            String::new()
        };
        Err(Error::Other(anyhow::anyhow!(
            "upstream {what} {} answered {status}{hint}",
            redact_url(resp.url().as_str())
        )))
    }

    /// Fetch and parse one upstream JSON document: the single path every
    /// metadata request takes, so each is vetted ([`Self::send`]), carries the
    /// credentials only where they belong, is bounded in time by
    /// `timeout_secs` and in size by [`MAX_JSON_BYTES`]. `None` is a 404.
    async fn get_json(&self, url: &str, what: &str) -> Result<Option<serde_json::Value>> {
        let resp = self.send(url, what, Some(self.timeout)).await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = self.check_status(resp, what)?;
        let too_large = || {
            Error::Other(anyhow::anyhow!(
                "upstream {what} {} is larger than {MAX_JSON_BYTES} bytes",
                redact_url(url)
            ))
        };
        if resp.content_length().is_some_and(|n| n > MAX_JSON_BYTES) {
            return Err(too_large());
        }
        let final_url = resp.url().clone();
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| request_error(&final_url, e))?;
            if (body.len() + chunk.len()) as u64 > MAX_JSON_BYTES {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map(Some).map_err(|e| {
            Error::Other(anyhow::anyhow!(
                "upstream {what} {} is not valid JSON: {e}",
                redact_url(url)
            ))
        })
    }

    /// The upstream service-index URL this client mirrors from.
    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    /// How many versions of one package a single read-through miss may fetch.
    pub fn max_versions_per_package(&self) -> Option<usize> {
        self.max_versions_per_package
    }

    /// Apply the server-wide upload cap to mirrored downloads unless the feed
    /// set a tighter one of its own.
    pub fn set_default_size_limit(&mut self, limit: Option<u64>) {
        if self.max_package_size_bytes.is_none() {
            self.max_package_size_bytes = limit;
        }
        // A mirror fetch is started by an *anonymous read*, and it writes what
        // it fetches to the operator's disk. "No limit" is a defensible default
        // for a push, which needs a credential; it is not one here. A feed that
        // genuinely mirrors enormous packages says so.
        if self.max_package_size_bytes.is_none() {
            self.max_package_size_bytes = Some(DEFAULT_MIRROR_MAX_PACKAGE_BYTES);
        }
    }

    async fn resources(&self) -> Result<&MirrorResources> {
        self.resources
            .get_or_try_init(|| async {
                let index = self
                    .get_json(&self.upstream, "service index")
                    .await?
                    .ok_or_else(|| not_found("service index", &self.upstream))?;
                let package_base =
                    find_resource(&index, "PackageBaseAddress/3.0.0").ok_or_else(|| {
                        Error::Other(anyhow::anyhow!(
                            "upstream {} has no PackageBaseAddress resource",
                            redact_url(&self.upstream)
                        ))
                    })?;
                // Search service type names are versioned; accept whichever the
                // upstream advertises, newest first.
                let search = find_first_resource(
                    &index,
                    &[
                        "SearchQueryService/3.5.0",
                        "SearchQueryService/3.0.0-rc",
                        "SearchQueryService/3.0.0-beta",
                        "SearchQueryService",
                    ],
                );
                let catalog = find_resource(&index, "Catalog/3.0.0");
                // Every one of these is a URL the *upstream* chose; vet each
                // before it is ever fetched. A bad optional resource is dropped
                // rather than fatal, so one odd entry cannot disable mirroring.
                self.check_url(&package_base, "PackageBaseAddress")?;
                Ok(MirrorResources {
                    package_base: ensure_trailing_slash(&package_base),
                    search: search.filter(|u| self.log_check(u, "SearchQueryService")),
                    catalog: catalog.filter(|u| self.log_check(u, "Catalog")),
                })
            })
            .await
    }

    /// Fetch the upstream's list of version strings for `lower_id`. An absent
    /// package (404) yields an empty list rather than an error.
    pub async fn upstream_versions(&self, lower_id: &str) -> Result<Vec<String>> {
        let base = &self.resources().await?.package_base;
        let url = format!("{base}{lower_id}/index.json");
        let Some(doc) = self.get_json(&url, "version list").await? else {
            return Ok(Vec::new());
        };
        Ok(doc
            .get("versions")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Stream the upstream's `.nupkg` for `lower_id`/`version` to `dest`,
    /// returning the [`StreamSummary`](streaming::StreamSummary) (size and
    /// SHA-512) computed while streaming — so the caller never re-reads the file
    /// just to hash it.
    pub async fn download_nupkg(
        &self,
        lower_id: &str,
        version: &str,
        dest: &Path,
    ) -> Result<streaming::StreamSummary> {
        let base = &self.resources().await?.package_base;
        let url = format!("{base}{lower_id}/{version}/{lower_id}.{version}.nupkg");
        let resp = self.send(&url, "package", self.download_deadline).await?;
        let resp = self.check_status(resp, "package")?;
        // Reject an over-sized package before a single byte hits the disk when
        // the upstream is honest about its length; the streaming cap below is
        // the real enforcement for when it is not.
        if let (Some(limit), Some(len)) = (self.max_package_size_bytes, resp.content_length()) {
            if len > limit {
                return Err(Error::PayloadTooLarge(format!(
                    "upstream package is {len} bytes, over the {limit} byte mirror limit"
                )));
            }
        }
        let mut file = tokio::fs::File::create(dest).await?;
        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(|e| std::io::Error::other(error_chain(&e.without_url()))));
        // A mirror fetch is triggered by an ordinary (possibly anonymous) read,
        // so an unbounded copy here would let anyone fill the disk by naming
        // packages upstream happens to host. The push path is capped; so is this.
        let summary = streaming::stream_to_writer_limited(
            Box::pin(stream),
            &mut file,
            self.max_package_size_bytes,
            // The mirror's own silence timeout already bounds a stalled upstream.
            None,
        )
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidData {
                Error::PayloadTooLarge(e.to_string())
            } else {
                Error::Io(e)
            }
        })?;
        Ok(summary)
    }

    /// Discover every package id the upstream exposes.
    ///
    /// Prefers the `SearchQueryService`, paging through it with an empty query;
    /// falls back to walking the `Catalog/3.0.0` resource when the upstream has
    /// no search service (or search returns nothing while a catalog exists).
    /// Ids are returned in their original casing, de-duplicated
    /// case-insensitively.
    pub async fn enumerate_package_ids(&self) -> Result<Vec<String>> {
        let res = self.resources().await?;
        if let Some(search) = &res.search {
            let ids = self.enumerate_via_search(search).await?;
            if !ids.is_empty() {
                return Ok(ids);
            }
            // A non-empty search service that returns nothing: either a truly
            // empty feed, or one whose contents only the catalog can reveal.
            if res.catalog.is_none() {
                return Ok(ids);
            }
        }
        if let Some(catalog) = &res.catalog {
            return self.enumerate_via_catalog(catalog).await;
        }
        Err(Error::Other(anyhow::anyhow!(
            "upstream {} exposes neither SearchQueryService nor Catalog/3.0.0; cannot enumerate packages",
            redact_url(&self.upstream)
        )))
    }

    /// Page through the upstream `SearchQueryService` with an empty query,
    /// collecting every package id.
    ///
    /// Only an empty page ends the walk. `totalHits` cannot be trusted to:
    /// BaGetter reports the number of results on the *current page* there, so
    /// stopping once `skip >= totalHits` ended every BaGetter migration after
    /// its first page. A short page cannot either, because a server may return
    /// fewer results than `take` asked for, so `skip` advances by what the page
    /// actually held. The one extra request for the empty page at the end is
    /// the price of not depending on either.
    async fn enumerate_via_search(&self, search: &str) -> Result<Vec<String>> {
        const PAGE: usize = 100;
        // Safety valve: 50,000 pages is five million ids at a full page each,
        // far past any real feed, and stops a source that never runs dry.
        const MAX_PAGES: usize = 50_000;
        let mut ids = DedupIds::new();
        let mut skip: usize = 0;
        for _ in 0..MAX_PAGES {
            let sep = if search.contains('?') { '&' } else { '?' };
            let url = format!(
                "{search}{sep}q=&skip={skip}&take={PAGE}&prerelease=true&semVerLevel=2.0.0"
            );
            let doc = self
                .get_json(&url, "search page")
                .await?
                .ok_or_else(|| not_found("search page", &url))?;

            let page_len = doc
                .get("data")
                .and_then(|d| d.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            if page_len == 0 {
                return Ok(ids.into_vec());
            }
            let known = ids.len();
            for id in page_ids(&doc) {
                ids.push(&id);
            }
            // A page of nothing but ids already seen means the source is not
            // honouring `skip` — it would answer every later page the same way.
            if ids.len() == known {
                tracing::warn!(
                    upstream = %redact_url(&self.upstream),
                    skip,
                    "search page repeated earlier results; stopping enumeration"
                );
                return Ok(ids.into_vec());
            }
            skip += page_len;
        }
        tracing::warn!(
            upstream = %redact_url(&self.upstream),
            pages = MAX_PAGES,
            "search enumeration hit its page limit; the id list may be incomplete"
        );
        Ok(ids.into_vec())
    }

    /// Walk the upstream `Catalog/3.0.0` (index → pages → items), collecting
    /// every package id. Best-effort: a page that fails to load is skipped.
    async fn enumerate_via_catalog(&self, catalog: &str) -> Result<Vec<String>> {
        let index = self
            .get_json(catalog, "catalog index")
            .await?
            .ok_or_else(|| not_found("catalog index", catalog))?;

        let pages: Vec<String> = index
            .get("items")
            .and_then(|i| i.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.get("@id").and_then(|u| u.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let mut ids = DedupIds::new();
        for page_url in pages {
            // Page URLs come from the catalog index, so `get_json` vets each
            // one like any other upstream-chosen URL.
            let page = match self.get_json(&page_url, "catalog page").await {
                Ok(Some(page)) => page,
                Ok(None) => {
                    tracing::warn!(page = %redact_url(&page_url), "catalog page not found");
                    continue;
                }
                Err(e) => {
                    tracing::warn!(page = %redact_url(&page_url), error = %e, "catalog page fetch failed");
                    continue;
                }
            };
            if let Some(items) = page.get("items").and_then(|i| i.as_array()) {
                for item in items {
                    if let Some(id) = item.get("nuget:id").and_then(|i| i.as_str()) {
                        ids.push(id);
                    }
                }
            }
        }
        Ok(ids.into_vec())
    }
}

/// The error for a document the upstream said does not exist.
fn not_found(what: &str, url: &str) -> Error {
    Error::Other(anyhow::anyhow!(
        "upstream {what} {} answered 404 Not Found",
        redact_url(url)
    ))
}

/// Extract the package ids from one `SearchQueryService` response page.
fn page_ids(doc: &serde_json::Value) -> Vec<String> {
    doc.get("data")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|i| i.get("id").and_then(|v| v.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Accumulates package ids in first-seen order, dropping case-insensitive
/// duplicates.
#[derive(Default)]
struct DedupIds {
    seen: std::collections::HashSet<String>,
    ids: Vec<String>,
}

impl DedupIds {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, id: &str) {
        if self.seen.insert(id.to_lowercase()) {
            self.ids.push(id.to_string());
        }
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    fn into_vec(self) -> Vec<String> {
        self.ids
    }
}

/// Options governing how mirrored versions are ingested.
#[derive(Debug, Clone, Default)]
pub struct MirrorOptions {
    /// Mirrored versions land pending (require admin approval before serving).
    pub requires_approval: bool,
    /// The feed's license policy, applied to each mirrored version.
    pub license_policy: LicensePolicyConfig,
}

/// Ensure every upstream version of `id` is present in `feed`, fetching and
/// indexing any that are missing. Returns how many new versions were mirrored.
///
/// Best-effort: a failure for one version is logged and skipped so a single bad
/// package never blocks the rest. Versions already in the feed are left as-is.
pub async fn ensure_package(
    client: &MirrorClient,
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    temp_dir: &Path,
    id: &str,
    options: &MirrorOptions,
) -> Result<usize> {
    // Only mirror well-formed package ids. This rejects anything (path
    // traversal, slashes, control characters) that could escape the upstream's
    // PackageBaseAddress path when interpolated into the request URL.
    if crate::validation::validate_package_id(id).is_err() {
        return Ok(0);
    }
    let lower_id = id.to_lowercase();

    // One read-through miss per id at a time. Without this, N concurrent
    // restores of the same missing package each start their own full download
    // of every upstream version — N times the bandwidth and disk for one
    // result.
    //
    // Declining rather than queueing matters: a fetch can run for minutes, and
    // a waiter would hold its request open for all of it to obtain a result the
    // winner is already producing. Losing the race is treated as a plain cache
    // miss, which is what it is.
    let Some(_guard) = crate::locks::try_lock_version(&lower_id, "<mirror>") else {
        tracing::debug!(%feed, id = %lower_id, "mirror fetch already in progress; skipping");
        return Ok(0);
    };

    let versions = client.upstream_versions(&lower_id).await?;
    let mut versions: Vec<NuGetVersion> = versions
        .iter()
        .filter_map(|raw| NuGetVersion::parse(raw).ok())
        .collect();
    // Newest first, so a bounded fetch keeps the versions clients actually want.
    versions.sort_by(|a, b| b.cmp(a));
    let considered = versions.len();
    if let Some(max) = client.max_versions_per_package() {
        versions.truncate(max);
    }
    if versions.len() < considered {
        tracing::info!(
            %feed, id = %lower_id, considered, fetching = versions.len(),
            "limiting mirrored versions (mirror.max_versions_per_package)"
        );
    }
    let mut mirrored = 0;
    let deadline = tokio::time::Instant::now() + MIRROR_BUDGET;

    for version in versions {
        // Skip versions the feed already exposes.
        if db.exists(feed, &lower_id, &version).await.unwrap_or(false) {
            continue;
        }
        if tokio::time::Instant::now() >= deadline {
            tracing::info!(
                %feed,
                id = %lower_id,
                mirrored,
                "mirror budget spent; the rest of this package's versions will \
                 be fetched on a later request"
            );
            break;
        }

        let normalized = version.normalized().to_lowercase();
        let temp_path = temp_dir.join(format!("mirror-{}.tmp", uuid::Uuid::new_v4()));
        let summary = match client
            .download_nupkg(&lower_id, &normalized, &temp_path)
            .await
        {
            Ok(summary) => summary,
            Err(e) => {
                tracing::warn!(%feed, id = %lower_id, version = %normalized, error = %e, "mirror download failed");
                let _ = tokio::fs::remove_file(&temp_path).await;
                continue;
            }
        };

        let opts = IndexOptions {
            overwrite: crate::config::OverwriteMode::Disabled,
            pending: options.requires_approval,
            license_policy: options.license_policy.clone(),
            // Pin the identity: whatever the upstream returned must be the
            // package we asked for. Otherwise a hostile upstream answers a
            // request for an obscure id with a manifest claiming a popular one,
            // and it lands in the local feed under that trusted name.
            expect: Some(crate::indexing::ExpectedIdentity {
                id: lower_id.clone(),
                version: version.clone(),
            }),
        };
        match indexing::index_package(storage, db, feed, temp_path, summary, &opts).await {
            Ok(_) => {
                mirrored += 1;
                tracing::info!(%feed, id = %lower_id, version = %normalized, pending = options.requires_approval, "mirrored package");
            }
            // A concurrent request may have mirrored it first — not an error.
            Err(Error::PackageAlreadyExists) => {}
            Err(e) => {
                tracing::warn!(%feed, id = %lower_id, version = %normalized, error = %e, "mirror index failed")
            }
        }
    }
    Ok(mirrored)
}

fn find_resource(index: &serde_json::Value, ty: &str) -> Option<String> {
    index
        .get("resources")?
        .as_array()?
        .iter()
        .find(|r| r.get("@type").and_then(|t| t.as_str()) == Some(ty))
        .and_then(|r| r.get("@id"))
        .and_then(|i| i.as_str())
        .map(str::to_string)
}

/// Return the first resource matching any of `types`, in the given priority
/// order (used for the versioned `SearchQueryService` type names).
fn find_first_resource(index: &serde_json::Value, types: &[&str]) -> Option<String> {
    types.iter().find_map(|ty| find_resource(index, ty))
}

fn ensure_trailing_slash(s: &str) -> String {
    if s.ends_with('/') {
        s.to_string()
    } else {
        format!("{s}/")
    }
}

/// An error from sending an upstream request, without the URL reqwest would
/// put in it (it can carry userinfo or a token in the query) but with the
/// cause chain, which is where "timed out" or "resolves to the private
/// address" lives.
fn request_error(url: &reqwest::Url, e: reqwest::Error) -> Error {
    Error::Other(anyhow::anyhow!(
        "upstream request to {} failed: {}",
        redact_url(url.as_str()),
        error_chain(&e.without_url())
    ))
}

/// An error's message followed by each distinct cause's.
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let msg = cause.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        source = cause.source();
    }
    out
}

/// A URL as it may appear in a log line or on screen: without userinfo, and
/// with any query replaced, since both are where feeds put credentials
/// (`https://user:token@host/…`, a signed `?sig=…` download link).
pub fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut parsed) => {
            let _ = parsed.set_username("");
            let _ = parsed.set_password(None);
            if parsed.query().is_some() {
                parsed.set_query(Some("redacted"));
            }
            parsed.set_fragment(None);
            parsed.to_string()
        }
        Err(_) => "<unparseable url>".to_string(),
    }
}

/// Build the header map a mirror client sends to the upstream's own origin
/// from its configured credentials. Basic and Bearer both populate
/// `Authorization` (Basic wins if both are set); custom headers are added as-is.
/// Malformed header names/values are skipped with a warning rather than failing
/// the whole client.
fn auth_headers(auth: &MirrorAuthConfig) -> reqwest::header::HeaderMap {
    use base64::Engine;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};

    let mut headers = HeaderMap::new();
    if let Some(user) = &auth.username {
        let pass = auth.password.as_deref().unwrap_or("");
        let raw = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        if let Ok(mut value) = HeaderValue::from_str(&format!("Basic {raw}")) {
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
    } else if let Some(token) = &auth.token {
        if let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {token}")) {
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
    }
    for (name, value) in &auth.headers {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(n), Ok(mut v)) => {
                v.set_sensitive(true);
                headers.insert(n, v);
            }
            _ => tracing::warn!(header = %name, "ignoring invalid mirror auth header"),
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_headers_basic_bearer_and_custom() {
        use reqwest::header::AUTHORIZATION;

        // No credentials → no headers.
        assert!(auth_headers(&MirrorAuthConfig::default()).is_empty());

        // Basic auth populates Authorization.
        let basic = MirrorAuthConfig {
            username: Some("user".into()),
            password: Some("pass".into()),
            ..Default::default()
        };
        let h = auth_headers(&basic);
        assert!(h
            .get(AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Basic "));

        // Bearer token plus a custom header.
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("X-Feed-Key".to_string(), "abc".to_string());
        let bearer = MirrorAuthConfig {
            token: Some("tok".into()),
            headers,
            ..Default::default()
        };
        let h = auth_headers(&bearer);
        assert_eq!(
            h.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer tok"
        );
        assert_eq!(h.get("X-Feed-Key").unwrap().to_str().unwrap(), "abc");
    }

    #[test]
    fn page_ids_extracts_and_dedup_collects() {
        let page = serde_json::json!({
            "totalHits": 3,
            "data": [
                {"id": "Alpha", "version": "1.0.0"},
                {"id": "Beta", "version": "2.0.0"},
                {"version": "9.9.9"} // no id — skipped
            ]
        });
        assert_eq!(
            page_ids(&page),
            vec!["Alpha".to_string(), "Beta".to_string()]
        );

        // DedupIds keeps first-seen casing and drops case-insensitive repeats.
        let mut ids = DedupIds::new();
        for id in ["Alpha", "Beta", "alpha", "Gamma", "BETA"] {
            ids.push(id);
        }
        assert_eq!(
            ids.into_vec(),
            vec!["Alpha".to_string(), "Beta".to_string(), "Gamma".to_string()]
        );
    }

    #[test]
    fn finds_first_resource_in_priority_order() {
        let index = serde_json::json!({
            "resources": [
                {"@id": "https://up/search-rc", "@type": "SearchQueryService/3.0.0-rc"},
                {"@id": "https://up/search", "@type": "SearchQueryService"}
            ]
        });
        // 3.5.0 is absent, so the next candidate (3.0.0-rc) wins.
        assert_eq!(
            find_first_resource(
                &index,
                &[
                    "SearchQueryService/3.5.0",
                    "SearchQueryService/3.0.0-rc",
                    "SearchQueryService"
                ]
            )
            .as_deref(),
            Some("https://up/search-rc")
        );
        assert!(find_first_resource(&index, &["Catalog/3.0.0"]).is_none());
    }

    #[test]
    fn finds_resource_by_type() {
        let index = serde_json::json!({
            "resources": [
                {"@id": "https://up/flat/", "@type": "PackageBaseAddress/3.0.0"},
                {"@id": "https://up/query", "@type": "SearchQueryService"}
            ]
        });
        assert_eq!(
            find_resource(&index, "PackageBaseAddress/3.0.0").as_deref(),
            Some("https://up/flat/")
        );
        assert!(find_resource(&index, "Nope").is_none());
    }

    #[test]
    fn trailing_slash_is_normalized() {
        assert_eq!(ensure_trailing_slash("https://x/flat"), "https://x/flat/");
        assert_eq!(ensure_trailing_slash("https://x/flat/"), "https://x/flat/");
    }

    #[test]
    fn upstream_urls_are_vetted_before_they_are_fetched() {
        let client = MirrorClient::from_config(&MirrorConfig {
            enabled: true,
            ..Default::default()
        })
        .unwrap();

        // The ordinary case: a public HTTPS feed.
        assert!(client
            .check_url("https://api.nuget.org/v3/index.json", "index")
            .is_ok());

        // Every one of these is a URL the *upstream* would choose, so each is a
        // way to turn the mirror into a probe of the server's own network.
        for hostile in [
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1:8080/v3/index.json",
            "https://localhost/v3/index.json",
            "http://10.1.2.3/flat/",
            "http://[::1]/flat/",
        ] {
            assert!(
                client.check_url(hostile, "resource").is_err(),
                "{hostile} should be refused"
            );
        }

        // Non-HTTP schemes never make sense for a feed resource.
        for scheme in ["file:///etc/passwd", "ftp://host/x", "gopher://host/1"] {
            assert!(
                client.check_url(scheme, "resource").is_err(),
                "{scheme} should be refused"
            );
        }

        // An operator running an upstream on their own network opts in.
        let private_ok = MirrorClient::from_config(&MirrorConfig {
            enabled: true,
            allow_private_upstream: true,
            ..Default::default()
        })
        .unwrap();
        assert!(private_ok
            .check_url("http://10.1.2.3/v3/index.json", "index")
            .is_ok());
        // ...but that opt-in still does not enable other schemes.
        assert!(private_ok.check_url("file:///etc/passwd", "index").is_err());
    }

    fn url(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn credentials_go_to_the_upstreams_own_origin_only() {
        let client = MirrorClient::from_config(&MirrorConfig {
            enabled: true,
            upstream: "https://Feed.Example/v3/index.json".into(),
            auth: MirrorAuthConfig {
                token: Some("secret".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        for same in [
            "https://feed.example/v3/flat/",
            "https://feed.example:443/v3/flat/x/index.json",
            "https://FEED.example/other",
        ] {
            assert!(client.carries_credentials(&url(same)), "{same}");
        }
        for other in [
            "https://cdn.example/v3/flat/",       // another host
            "https://feed.example:8443/v3/flat/", // another port
            "http://feed.example/v3/flat/",       // another scheme
            "https://feed.example.evil/v3/flat/",
        ] {
            assert!(!client.carries_credentials(&url(other)), "{other}");
        }
    }

    #[test]
    fn redirects_are_vetted_like_the_first_url_and_never_downgrade() {
        let client = MirrorClient::from_config(&MirrorConfig {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
        let from = url("https://feed.example/flat/a/1.0.0/a.1.0.0.nupkg");
        // Relative and cross-host redirects are fine in themselves.
        assert_eq!(
            client
                .redirect_target(&from, "/blobs/a.nupkg", "package")
                .unwrap()
                .as_str(),
            "https://feed.example/blobs/a.nupkg"
        );
        assert!(client
            .redirect_target(&from, "https://cdn.example/a.nupkg?sig=x", "package")
            .is_ok());
        for refused in [
            "http://feed.example/a.nupkg", // https → http
            "http://169.254.169.254/latest/meta-data/",
            "https://127.0.0.1/a.nupkg",
            "https://localhost./a.nupkg",
            "file:///etc/passwd",
        ] {
            assert!(
                client.redirect_target(&from, refused, "package").is_err(),
                "{refused} should be refused"
            );
        }
    }

    #[test]
    fn urls_are_redacted_for_logs() {
        assert_eq!(
            redact_url("https://user:pa55@feed.example/v3/index.json"),
            "https://feed.example/v3/index.json"
        );
        assert_eq!(
            redact_url("https://cdn.example/a.nupkg?sig=secret&se=2026#frag"),
            "https://cdn.example/a.nupkg?redacted"
        );
        assert_eq!(redact_url("not a url with a token"), "<unparseable url>");
    }

    #[tokio::test]
    async fn the_resolver_drops_private_addresses() {
        use reqwest::dns::Resolve;

        // `localhost` resolves to loopback on every platform, which is exactly
        // what a name with a private `A` record looks like to the mirror.
        let guarded = GuardedResolver { proxy_host: None };
        match guarded.resolve("localhost".parse().unwrap()).await {
            Ok(_) => panic!("a name resolving to loopback must not connect"),
            Err(e) => assert!(e.to_string().contains("private address"), "{e}"),
        }

        // The operator's own proxy is exempt: it is routinely on the private
        // network, and the operator named it.
        let proxied = GuardedResolver {
            proxy_host: Some("LOCALHOST".into()),
        };
        let addrs = proxied
            .resolve("localhost".parse().unwrap())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(addrs.count() > 0);
    }

    #[test]
    fn read_through_mirroring_is_bounded_by_default() {
        // An anonymous read can trigger a mirror fetch, so an unbounded default
        // would let anyone name a popular upstream id and pull tens of
        // gigabytes onto the disk.
        let cfg = MirrorConfig::default();
        assert!(cfg.max_versions_per_package.is_some());
        assert!(!cfg.allow_private_upstream);
    }

    #[test]
    fn size_limit_is_inherited_from_the_server_unless_set() {
        let mut client = MirrorClient::from_config(&MirrorConfig {
            enabled: true,
            ..Default::default()
        })
        .unwrap();
        assert!(client.max_package_size_bytes.is_none());
        client.set_default_size_limit(Some(1024));
        assert_eq!(client.max_package_size_bytes, Some(1024));
        // A feed that set its own tighter cap keeps it.
        client.set_default_size_limit(Some(9999));
        assert_eq!(client.max_package_size_bytes, Some(1024));
    }

    #[test]
    fn disabled_mirror_builds_no_client() {
        assert!(MirrorClient::from_config(&MirrorConfig::default()).is_none());
        let enabled = MirrorConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(MirrorClient::from_config(&enabled).is_some());
    }

    /// How a fake upstream's search service pages.
    #[derive(Clone, Copy, Debug)]
    enum Quirk {
        /// Reports the real total in `totalHits`, as nuget.org and YANuget do.
        Honest,
        /// BaGetter: `totalHits` is the number of results on the current page.
        PageSizedTotalHits,
        /// Returns at most this many results, whatever `take` asked for.
        CapsTake(usize),
        /// Ignores `skip`: every request gets the first page again.
        IgnoresSkip,
    }

    /// Serve a service index and a search service over `total` package ids
    /// (`Pkg.000`, `Pkg.001`, …). Returns the service-index URL and a count of
    /// the search requests made.
    async fn fake_search_upstream(
        total: usize,
        quirk: Quirk,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::extract::Query;
        use axum::routing::get;
        use axum::{Json, Router};
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let index = serde_json::json!({
            "version": "3.0.0",
            "resources": [
                {"@id": format!("{base}/flat/"), "@type": "PackageBaseAddress/3.0.0"},
                {"@id": format!("{base}/query"), "@type": "SearchQueryService"}
            ]
        });
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let app = Router::new()
            .route(
                "/v3/index.json",
                get(move || {
                    let index = index.clone();
                    async move { Json(index) }
                }),
            )
            .route(
                "/query",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let arg = |k: &str| q.get(k).and_then(|v| v.parse::<usize>().ok());
                    let mut skip = arg("skip").unwrap_or(0);
                    let mut take = arg("take").unwrap_or(20);
                    match quirk {
                        Quirk::CapsTake(cap) => take = take.min(cap),
                        Quirk::IgnoresSkip => skip = 0,
                        Quirk::Honest | Quirk::PageSizedTotalHits => {}
                    }
                    let data: Vec<serde_json::Value> = (skip..total.min(skip + take))
                        .map(|i| serde_json::json!({"id": format!("Pkg.{i:03}"), "version": "1.0.0"}))
                        .collect();
                    let total_hits = match quirk {
                        Quirk::PageSizedTotalHits => data.len(),
                        _ => total,
                    };
                    async move { Json(serde_json::json!({"totalHits": total_hits, "data": data})) }
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("{base}/v3/index.json"), requests)
    }

    async fn enumerate(upstream: String) -> Vec<String> {
        let client = MirrorClient::from_config(&MirrorConfig {
            enabled: true,
            upstream,
            // The fake upstream is on loopback.
            allow_private_upstream: true,
            ..Default::default()
        })
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.enumerate_package_ids(),
        )
        .await
        .expect("enumeration must terminate")
        .unwrap()
    }

    #[tokio::test]
    async fn search_enumeration_reaches_every_page_whatever_totalhits_says() {
        // BaGetter's `totalHits` counts only the results on the page it
        // returns, so a walk that stops once `skip >= totalHits` ends after page
        // one: a real migration off BaGetter found 100 of its 164 package ids.
        for quirk in [Quirk::Honest, Quirk::PageSizedTotalHits] {
            let (upstream, _) = fake_search_upstream(250, quirk).await;
            let ids = enumerate(upstream).await;
            assert_eq!(ids.len(), 250, "{quirk:?}");
            assert_eq!(ids[249], "Pkg.249", "{quirk:?}");
        }
    }

    #[tokio::test]
    async fn search_enumeration_survives_a_server_that_caps_take() {
        // A server may return fewer results than `take` asked for. A short page
        // is then not the end, and `skip` has to advance by what arrived.
        let (upstream, _) = fake_search_upstream(250, Quirk::CapsTake(30)).await;
        assert_eq!(enumerate(upstream).await.len(), 250);
    }

    #[tokio::test]
    async fn search_enumeration_stops_when_the_server_ignores_skip() {
        // With nothing but an empty page to stop on, a server that answers
        // every request with its first page would be asked again until the
        // safety valve. A page that adds no new id ends the walk instead.
        let (upstream, requests) = fake_search_upstream(250, Quirk::IgnoresSkip).await;
        assert_eq!(enumerate(upstream).await.len(), 100);
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
