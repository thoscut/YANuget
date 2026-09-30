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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::{watch, OnceCell};

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
    timeout: Duration,
    /// Deadline for one whole `.nupkg` download. `None` leaves only the read
    /// timeout, which bounds how long the upstream may go silent.
    download_deadline: Option<Duration>,
    /// Free space a download must leave on the volume (`min_free_disk_bytes`).
    min_free_disk_bytes: u64,
    /// How long a package's upstream version list is trusted (`refresh_secs`).
    refresh: Duration,
    resources: OnceCell<MirrorResources>,
    tracker: Tracker,
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

/// Host names a test resolves without DNS. Empty outside tests.
#[derive(Default)]
struct TestDns {
    /// Answered by reqwest itself, around the guard: a stand-in for a public
    /// host that happens to be a loopback test server.
    unguarded: Vec<(String, std::net::SocketAddr)>,
    /// Answered by the guarded resolver, and filtered like real DNS answers.
    guarded: Vec<(String, std::net::IpAddr)>,
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
    /// `Catalog/3.0.0`, when present. Walked alongside search, which some
    /// servers cap or lack.
    catalog: Option<String>,
    /// `RegistrationsBaseUrl` (the SemVer 2.0.0 hive when advertised), with a
    /// trailing slash. Where the upstream says which versions are unlisted.
    registration: Option<String>,
}

/// The package ids an upstream exposes, and what could not be read while
/// finding them.
#[derive(Debug, Default)]
pub struct Enumeration {
    /// Ids in their original casing, de-duplicated case-insensitively.
    pub ids: Vec<String>,
    /// `(what, error)` for each part of the upstream that failed to load —
    /// a catalog page, say. Ids it held are missing from `ids`.
    pub failures: Vec<(String, String)>,
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
const MIRROR_BUDGET: Duration = Duration::from_secs(60);

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
#[derive(Default)]
struct GuardedResolver {
    /// A configured outbound proxy's host, resolved without the filter: the
    /// operator named it, and it is routinely on the private network.
    proxy_host: Option<String>,
    /// Names answered without a DNS lookup, and filtered like any other.
    /// Empty outside tests, which need a name that resolves to loopback.
    fixed: Vec<(String, std::net::IpAddr)>,
}

impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let exempt = self
            .proxy_host
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(&host));
        let fixed = self
            .fixed
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&host))
            .map(|(_, ip)| std::net::SocketAddr::new(*ip, 0));
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> = match fixed {
                Some(addr) => vec![addr],
                None => tokio::net::lookup_host((host.as_str(), 0)).await?.collect(),
            };
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
    /// mirroring is disabled for the feed — or when the client cannot be
    /// built, which is logged: a mirror that silently turned itself off looked
    /// exactly like an upstream that had nothing. The server itself uses
    /// [`Self::try_from_config`] and refuses to start instead.
    pub fn from_config(config: &MirrorConfig) -> Option<Self> {
        Self::try_from_config(config).unwrap_or_else(|e| {
            tracing::error!(
                upstream = %redact_url(&config.upstream),
                error = %e,
                "mirror disabled: the upstream client could not be built"
            );
            None
        })
    }

    /// [`Self::from_config`], with a configuration that cannot work — an
    /// unparseable upstream or proxy, conflicting credentials, an unreadable
    /// CA file — reported as an error.
    pub fn try_from_config(config: &MirrorConfig) -> Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        Self::build(config, false).map(Some)
    }

    /// Build the client `yanuget migrate` copies a source with.
    ///
    /// Unlike the read-through mirror, a migration is a command an operator
    /// runs by hand, like `curl`, so it uses the shell's `HTTP(S)_PROXY` when
    /// no proxy is configured; and a download may take as long as it needs
    /// (see [`Self::set_download_deadline`]).
    pub fn for_migration(config: &MirrorConfig) -> Result<Self> {
        let mut client = Self::build(config, true)?;
        client.set_download_deadline(None);
        Ok(client)
    }

    fn build(config: &MirrorConfig, env_proxy: bool) -> Result<Self> {
        Self::build_with(config, env_proxy, &TestDns::default())
    }

    fn build_with(config: &MirrorConfig, env_proxy: bool, dns: &TestDns) -> Result<Self> {
        let invalid = |msg: String| Error::BadRequest(format!("mirror: {msg}"));
        config.validate()?;
        let upstream_origin = reqwest::Url::parse(&config.upstream)
            .ok()
            .as_ref()
            .and_then(origin)
            .ok_or_else(|| {
                invalid(format!(
                    "upstream {} is not an absolute URL",
                    redact_url(&config.upstream)
                ))
            })?;
        let credentials = auth_headers(&config.auth).map_err(invalid)?;
        // Connecting and every read are bounded on the client. A read timeout
        // restarts with each chunk received, so it limits how long the upstream
        // may go silent, not how long a transfer may take; each request adds a
        // total deadline of its own on top (`timeout`, `download_deadline`).
        let timeout = Duration::from_secs(config.timeout_secs.max(1));
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
                let parsed = reqwest::Proxy::all(proxy.as_str()).map_err(|_| {
                    invalid(format!("proxy {} is not a valid URL", redact_url(proxy)))
                })?;
                builder = builder.proxy(parsed);
                proxy_host = reqwest::Url::parse(proxy)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string));
            }
            None if env_proxy => {}
            None => builder = builder.no_proxy(),
        }
        if !config.allow_private_upstream {
            builder = builder.dns_resolver(std::sync::Arc::new(GuardedResolver {
                proxy_host,
                fixed: dns.guarded.clone(),
            }));
        }
        for (name, addr) in &dns.unguarded {
            builder = builder.resolve(name, *addr);
        }
        // The system store and the bundled roots are both trusted already;
        // an internal CA the host does not carry can be named here.
        if let Some(path) = &config.ca_cert_path {
            let unusable = |why: String| invalid(format!("ca_cert_path {}: {why}", path.display()));
            let pem = std::fs::read(path).map_err(|e| unusable(e.to_string()))?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|e| unusable(error_chain(&e)))?;
            if certs.is_empty() {
                return Err(unusable("no PEM certificate in the file".into()));
            }
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }
        let client = builder
            .build()
            .map_err(|e| invalid(format!("cannot build the HTTP client: {}", error_chain(&e))))?;
        Ok(Self {
            client,
            upstream: config.upstream.clone(),
            upstream_origin,
            credentials,
            allow_private_upstream: config.allow_private_upstream,
            max_package_size_bytes: config.max_package_size_bytes,
            max_versions_per_package: config.max_versions_per_package,
            timeout,
            download_deadline: match config.download_timeout_secs {
                0 => None,
                secs => Some(Duration::from_secs(secs)),
            },
            min_free_disk_bytes: 0,
            refresh: Duration::from_secs(config.refresh_secs),
            resources: OnceCell::new(),
            tracker: Tracker::default(),
        })
    }

    /// Replace the deadline on one whole `.nupkg` download. `None` removes it:
    /// the upstream may then take as long as it needs, provided it never goes
    /// silent for longer than the read timeout.
    ///
    /// The read-through mirror keeps one (`download_timeout_secs`), because an
    /// anonymous request starts that fetch and an upstream that trickles bytes
    /// must not hold it open forever. `migrate` removes it: an operator copying
    /// a feed wants its large packages, however long they take.
    pub fn set_download_deadline(&mut self, deadline: Option<Duration>) {
        self.download_deadline = deadline;
    }

    /// Refuse a download that would leave less than `reserve` bytes free on
    /// the volume it is written to — the server's `min_free_disk_bytes`.
    ///
    /// A push is held to that reserve; a mirror fetch, which an anonymous read
    /// starts, was not, and could fill the volume the database lives on.
    pub fn set_min_free_disk_bytes(&mut self, reserve: u64) {
        self.min_free_disk_bytes = reserve;
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
        deadline: Option<Duration>,
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
        // nuget.org serves its registration hives from blob storage stored
        // gzipped, with `Content-Encoding: gzip` whether or not it was asked
        // for; the decoded size is held to the same cap.
        let gzipped = resp
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("gzip"));
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| request_error(&final_url, e))?;
            if (body.len() + chunk.len()) as u64 > MAX_JSON_BYTES {
                return Err(too_large());
            }
            body.extend_from_slice(&chunk);
        }
        if gzipped {
            use std::io::Read;
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(body.as_slice())
                .take(MAX_JSON_BYTES + 1)
                .read_to_end(&mut decoded)
                .map_err(|e| {
                    Error::Other(anyhow::anyhow!(
                        "upstream {what} {} is not valid gzip: {e}",
                        redact_url(url)
                    ))
                })?;
            if decoded.len() as u64 > MAX_JSON_BYTES {
                return Err(too_large());
            }
            body = decoded;
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
                let registration = find_first_resource(
                    &index,
                    &[
                        "RegistrationsBaseUrl/3.6.0",
                        "RegistrationsBaseUrl/3.4.0",
                        "RegistrationsBaseUrl/3.0.0-rc",
                        "RegistrationsBaseUrl/3.0.0-beta",
                        "RegistrationsBaseUrl",
                    ],
                );
                // Every one of these is a URL the *upstream* chose; vet each
                // before it is ever fetched. A bad optional resource is dropped
                // rather than fatal, so one odd entry cannot disable mirroring.
                self.check_url(&package_base, "PackageBaseAddress")?;
                Ok(MirrorResources {
                    package_base: ensure_trailing_slash(&package_base),
                    search: search.filter(|u| self.log_check(u, "SearchQueryService")),
                    catalog: catalog.filter(|u| self.log_check(u, "Catalog")),
                    registration: registration
                        .filter(|u| self.log_check(u, "RegistrationsBaseUrl"))
                        .map(|u| ensure_trailing_slash(&u)),
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

    /// The versions of `lower_id` the upstream has unlisted, normalized and
    /// lower-cased, from its registration. Empty when the upstream has no
    /// registration resource or no registration for the package.
    ///
    /// The flat container a mirror lists versions from says nothing about
    /// listing, so without this every copied version was listed: a version
    /// the upstream had deliberately hidden came back into search and
    /// "latest".
    pub async fn upstream_unlisted(
        &self,
        lower_id: &str,
    ) -> Result<std::collections::HashSet<String>> {
        let mut unlisted = std::collections::HashSet::new();
        let Some(base) = &self.resources().await?.registration else {
            return Ok(unlisted);
        };
        let url = format!("{base}{lower_id}/index.json");
        let Some(index) = self.get_json(&url, "registration").await? else {
            return Ok(unlisted);
        };
        let pages = index
            .get("items")
            .and_then(|i| i.as_array())
            .cloned()
            .unwrap_or_default();
        for page in pages {
            // Small packages inline their leaves; large ones page them out.
            let page = match page.get("items") {
                Some(_) => page,
                None => {
                    let Some(page_url) = page.get("@id").and_then(|u| u.as_str()) else {
                        continue;
                    };
                    self.get_json(page_url, "registration page")
                        .await?
                        .ok_or_else(|| not_found("registration page", page_url))?
                }
            };
            for leaf in page
                .get("items")
                .and_then(|i| i.as_array())
                .into_iter()
                .flatten()
            {
                let entry = leaf.get("catalogEntry").unwrap_or(leaf);
                // `listed: false` is the flag; a `published` date in 1900 is
                // how older servers said the same thing.
                let hidden = entry.get("listed").and_then(|l| l.as_bool()) == Some(false)
                    || entry
                        .get("published")
                        .and_then(|p| p.as_str())
                        .is_some_and(|p| p.starts_with("1900-"));
                let version = entry
                    .get("version")
                    .and_then(|v| v.as_str())
                    .and_then(|v| NuGetVersion::parse(v).ok());
                if let (true, Some(version)) = (hidden, version) {
                    unlisted.insert(version.normalized().to_lowercase());
                }
            }
        }
        Ok(unlisted)
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
        self.download_nupkg_with_progress(lower_id, version, dest, &|_| {})
            .await
    }

    /// [`Self::download_nupkg`], calling `progress` with the size of each
    /// chunk as it arrives.
    pub async fn download_nupkg_with_progress(
        &self,
        lower_id: &str,
        version: &str,
        dest: &Path,
        progress: &(dyn Fn(u64) + Send + Sync),
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
        ensure_free_space(dest, resp.content_length(), self.min_free_disk_bytes)?;
        let mut file = tokio::fs::File::create(dest).await?;
        let stream = resp.bytes_stream().map(|r| {
            if let Ok(chunk) = &r {
                progress(chunk.len() as u64);
            }
            r.map_err(|e| std::io::Error::other(error_chain(&e.without_url())))
        });
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
    /// Pages through the `SearchQueryService` with an empty query *and* walks
    /// the `Catalog/3.0.0` resource when the upstream has both, and returns
    /// the union. Either alone can come up short: a server may cap search at
    /// some number of results, and a catalog is only as complete as its
    /// pages. A search that fails outright is not fatal when there is a
    /// catalog to fall back on; a catalog page that fails is reported in
    /// [`Enumeration::failures`], because the ids on it are then missing.
    pub async fn enumerate_package_ids(&self) -> Result<Enumeration> {
        let res = self.resources().await?;
        if res.search.is_none() && res.catalog.is_none() {
            return Err(Error::Other(anyhow::anyhow!(
                "upstream {} exposes neither SearchQueryService nor Catalog/3.0.0; cannot enumerate packages",
                redact_url(&self.upstream)
            )));
        }
        let mut ids = DedupIds::new();
        let mut failures = Vec::new();
        if let Some(search) = &res.search {
            match self.enumerate_via_search(search).await {
                Ok(found) => found.iter().for_each(|id| ids.push(id)),
                Err(e) if res.catalog.is_some() => {
                    tracing::warn!(error = %e, "search enumeration failed; relying on the catalog");
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(catalog) = &res.catalog {
            match self.enumerate_via_catalog(catalog, &mut failures).await {
                Ok(found) => found.iter().for_each(|id| ids.push(id)),
                // Search already answered; a catalog that cannot even be
                // opened leaves what it found.
                Err(e) if res.search.is_some() => {
                    failures.push(("catalog".to_string(), e.to_string()));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Enumeration {
            ids: ids.into_vec(),
            failures,
        })
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
    /// every package id. A page that fails to load is recorded in `failures`
    /// and the walk goes on.
    async fn enumerate_via_catalog(
        &self,
        catalog: &str,
        failures: &mut Vec<(String, String)>,
    ) -> Result<Vec<String>> {
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
                    let e = not_found("catalog page", &page_url);
                    failures.push((
                        format!("catalog page {}", redact_url(&page_url)),
                        e.to_string(),
                    ));
                    continue;
                }
                Err(e) => {
                    failures.push((
                        format!("catalog page {}", redact_url(&page_url)),
                        e.to_string(),
                    ));
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
    /// Id prefixes other feeds reserved, which this feed refuses to mirror.
    pub reserved_elsewhere: Vec<crate::config::ReservedPrefix>,
}

/// How long a request waits for another request's fetch of the same package
/// before answering with what the feed has.
const WAIT_FOR_FETCH: Duration = MIRROR_BUDGET;

/// The first retry after an upstream failure; each further failure in a row
/// doubles it, up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// How long a version that failed to mirror (absent upstream, over the size
/// cap, the wrong identity, refused by policy) is not asked for again.
const BAD_VERSION_RETRY: Duration = Duration::from_secs(15 * 60);

/// Packages whose fetch state is remembered. Past this, entries with nothing
/// left to remember go first, then the oldest; forgetting one only costs an
/// upstream request.
const MAX_TRACKED: usize = 10_000;

/// Consecutive upstream failures, and when asking again is allowed.
#[derive(Debug, Default)]
struct Backoff {
    failures: u32,
    until: Option<Instant>,
}

impl Backoff {
    fn active(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }

    fn fail(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let factor = 1u32 << (self.failures - 1).min(10);
        self.until = Some(now + (FIRST_BACKOFF * factor).min(MAX_BACKOFF));
    }

    fn succeed(&mut self) {
        *self = Self::default();
    }
}

/// What the mirror remembers about one package in one feed.
#[derive(Default)]
struct IdState {
    /// When the upstream version list was last fetched *and* worked through.
    /// A fetch the budget cut short does not count, so the next read carries
    /// on; an empty list does, which is the negative cache for unknown ids.
    listed_at: Option<Instant>,
    /// Upstream failures for this package.
    backoff: Backoff,
    /// Versions that failed to mirror, until when.
    bad_versions: HashMap<String, Instant>,
    /// The fetch in progress, if any: its number and a receiver that wakes
    /// when it ends (its sender is dropped).
    inflight: Option<(u64, watch::Receiver<()>)>,
    touched: Option<Instant>,
}

impl IdState {
    /// Whether nothing about this package needs remembering any more.
    fn is_idle(&self, now: Instant, refresh: Duration) -> bool {
        self.inflight.is_none()
            && !self.backoff.active(now)
            && self.bad_versions.values().all(|until| *until <= now)
            && self
                .listed_at
                .is_none_or(|at| now.duration_since(at) >= refresh)
    }
}

/// Per-package fetch state for one mirror client: when each package was
/// last listed, which upstream failures to back off from, and which fetches
/// are running — keyed by (feed, id), so two feeds mirroring the same package
/// never wait on each other.
#[derive(Default)]
struct Tracker {
    ids: StdMutex<HashMap<(String, String), IdState>>,
    /// The service index itself failing: backs off every package at once.
    upstream: StdMutex<Backoff>,
    next_fetch: std::sync::atomic::AtomicU64,
}

/// Either this request fetches, or it waits for the one that already is.
enum Turn<'a> {
    Lead(FetchGuard<'a>),
    Wait(watch::Receiver<()>),
}

/// Held while a request fetches a package. Dropping it — when the fetch
/// ends, or when the request is cancelled mid-way — wakes every waiter.
struct FetchGuard<'a> {
    tracker: &'a Tracker,
    key: (String, String),
    fetch: u64,
    _done: watch::Sender<()>,
}

impl Drop for FetchGuard<'_> {
    fn drop(&mut self) {
        let mut ids = self.tracker.lock();
        if let Some(state) = ids.get_mut(&self.key) {
            if state
                .inflight
                .as_ref()
                .is_some_and(|(n, _)| *n == self.fetch)
            {
                state.inflight = None;
            }
        }
    }
}

impl Tracker {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), IdState>> {
        // A panic while holding the lock leaves only fetch bookkeeping behind.
        self.ids.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn upstream(&self) -> std::sync::MutexGuard<'_, Backoff> {
        self.upstream.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn with<T>(
        &self,
        key: &(String, String),
        refresh: Duration,
        f: impl FnOnce(&mut IdState) -> T,
    ) -> T {
        let now = Instant::now();
        let mut ids = self.lock();
        if !ids.contains_key(key) && ids.len() >= MAX_TRACKED {
            ids.retain(|_, s| !s.is_idle(now, refresh));
            if ids.len() >= MAX_TRACKED {
                let mut by_age: Vec<((String, String), Option<Instant>)> = ids
                    .iter()
                    .filter(|(_, s)| s.inflight.is_none())
                    .map(|(k, s)| (k.clone(), s.touched))
                    .collect();
                by_age.sort_by_key(|(_, touched)| *touched);
                for (k, _) in by_age.into_iter().take(ids.len() + 1 - MAX_TRACKED * 3 / 4) {
                    ids.remove(&k);
                }
            }
        }
        let state = ids.entry(key.clone()).or_default();
        state.touched = Some(now);
        f(state)
    }

    /// Take this package's fetch, or join the one in progress.
    fn turn(&self, key: &(String, String), refresh: Duration) -> Turn<'_> {
        let fetch = self
            .next_fetch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let lead = self.with(key, refresh, |state| {
            if let Some((_, rx)) = &state.inflight {
                return Err(rx.clone());
            }
            let (tx, rx) = watch::channel(());
            state.inflight = Some((fetch, rx));
            Ok(tx)
        });
        match lead {
            Ok(tx) => Turn::Lead(FetchGuard {
                tracker: self,
                key: key.clone(),
                fetch,
                _done: tx,
            }),
            Err(rx) => Turn::Wait(rx),
        }
    }
}

impl MirrorClient {
    fn key(feed: &str, lower_id: &str) -> (String, String) {
        (feed.to_string(), lower_id.to_string())
    }

    /// Whether a read of `id` in `feed` should re-list it upstream: nothing is
    /// fetching it, the upstream is not being backed off, and its version list
    /// is older than `refresh_secs` (or was never fetched by this process).
    pub fn wants_refresh(&self, feed: &str, id: &str) -> bool {
        self.list_is_stale(feed, &id.to_lowercase(), true)
    }

    /// Whether the version list of `lower_id` is due to be fetched again;
    /// with `idle`, also that no fetch of it is running right now.
    fn list_is_stale(&self, feed: &str, lower_id: &str, idle: bool) -> bool {
        let now = Instant::now();
        if self.tracker.upstream().active(now) {
            return false;
        }
        self.tracker
            .with(&Self::key(feed, lower_id), self.refresh, |state| {
                (!idle || state.inflight.is_none())
                    && !state.backoff.active(now)
                    && state
                        .listed_at
                        .is_none_or(|at| now.duration_since(at) >= self.refresh)
            })
    }
}

/// Bring `feed` up to date with the upstream's versions of `id`: list them
/// (unless that was done within `refresh_secs`) and fetch and index the
/// newest `max_versions_per_package` the feed does not have. Returns how many
/// versions were mirrored.
///
/// Best-effort: a failure for one version is logged and skipped so a single bad
/// package never blocks the rest. Versions already in the feed are left as-is,
/// and versions deleted from it are never fetched again.
pub async fn ensure_package(
    client: &MirrorClient,
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    temp_dir: &Path,
    id: &str,
    options: &MirrorOptions,
) -> Result<usize> {
    MirrorTarget {
        client,
        storage,
        db,
        feed,
        temp_dir,
        options,
    }
    .ensure_package(id)
    .await
}

/// A feed a mirror client fills: where fetched packages are stored, indexed
/// and staged, and how they are admitted.
pub struct MirrorTarget<'a> {
    pub client: &'a MirrorClient,
    pub storage: &'a dyn PackageStorage,
    pub db: &'a dyn PackageDatabase,
    pub feed: &'a str,
    /// Where downloads are staged: on the package store's filesystem, so
    /// indexing can move them into place with a rename.
    pub temp_dir: &'a Path,
    pub options: &'a MirrorOptions,
}

impl MirrorTarget<'_> {
    /// See [`ensure_package`].
    pub async fn ensure_package(&self, id: &str) -> Result<usize> {
        self.ensure(id, None).await
    }

    /// Fetch one version a client asked for and the feed does not have —
    /// whether or not it is among the newest `max_versions_per_package`, since
    /// a project pinned to an older version would otherwise never restore.
    /// Returns 1 when it was mirrored.
    pub async fn ensure_version(&self, id: &str, version: &NuGetVersion) -> Result<usize> {
        self.ensure(id, Some(version)).await
    }

    async fn ensure(&self, id: &str, want: Option<&NuGetVersion>) -> Result<usize> {
        // Only mirror well-formed package ids. This rejects anything (path
        // traversal, slashes, control characters) that could escape the
        // upstream's PackageBaseAddress path when interpolated into the
        // request URL.
        if crate::validation::validate_package_id(id).is_err() {
            return Ok(0);
        }
        // Reserved for another feed: indexing would refuse every version, so
        // nothing is worth downloading.
        if self.options.reserved_elsewhere.iter().any(|r| r.covers(id)) {
            return Ok(0);
        }
        let client = self.client;
        let lower_id = id.to_lowercase();
        let key = MirrorClient::key(self.feed, &lower_id);

        // One fetch per package and feed at a time. Without this, N concurrent
        // restores of the same missing package each start their own download
        // of every upstream version — N times the bandwidth and disk for one
        // result. The others wait for it, a bounded while, and then answer
        // from what it stored: answering 404 at once failed every restore
        // that happened to lose the race for a package just being fetched.
        let give_up = Instant::now() + WAIT_FOR_FETCH;
        let _fetching = loop {
            match client.tracker.turn(&key, client.refresh) {
                Turn::Lead(guard) => break guard,
                Turn::Wait(mut done) => {
                    let left = give_up.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(0);
                    }
                    let _ = tokio::time::timeout(left, done.changed()).await;
                    // Whatever that fetch did, a listing waiter is served
                    // unless the list is still stale. A version waiter may
                    // need a fetch of its own: the one it waited on may have
                    // been for another version, or the newest few only.
                    let satisfied = match want {
                        None => !client.list_is_stale(self.feed, &lower_id, false),
                        Some(v) => self
                            .db
                            .exists(self.feed, &lower_id, v)
                            .await
                            .unwrap_or(false),
                    };
                    if satisfied || Instant::now() >= give_up {
                        return Ok(0);
                    }
                }
            }
        };

        match want {
            Some(version) => self.fetch_requested(&lower_id, version).await,
            None => self.fetch_listing(&lower_id).await,
        }
    }

    /// Whether `version` should be fetched: the feed lacks it, it was not
    /// deleted from the feed, and it did not just fail. A database error
    /// counts as "no" — a mirror that cannot tell must not re-add a version
    /// that may have been deleted.
    async fn wanted(&self, lower_id: &str, version: &NuGetVersion) -> bool {
        let (db, feed) = (self.db, self.feed);
        if db.exists(feed, lower_id, version).await.unwrap_or(true)
            || db
                .is_tombstoned(feed, lower_id, version)
                .await
                .unwrap_or(true)
        {
            return false;
        }
        let now = Instant::now();
        let normalized = version.normalized().to_lowercase();
        self.client.tracker.with(
            &MirrorClient::key(feed, lower_id),
            self.client.refresh,
            |state| {
                state
                    .bad_versions
                    .get(&normalized)
                    .is_none_or(|until| *until <= now)
            },
        )
    }

    fn remember_bad(&self, lower_id: &str, version: &NuGetVersion) {
        let until = Instant::now() + BAD_VERSION_RETRY;
        let normalized = version.normalized().to_lowercase();
        self.client.tracker.with(
            &MirrorClient::key(self.feed, lower_id),
            self.client.refresh,
            |state| {
                state.bad_versions.retain(|_, u| *u > Instant::now());
                state.bad_versions.insert(normalized, until);
            },
        );
    }

    fn backing_off(&self, lower_id: &str) -> bool {
        let now = Instant::now();
        let client = self.client;
        client.tracker.upstream().active(now)
            || client.tracker.with(
                &MirrorClient::key(self.feed, lower_id),
                client.refresh,
                |state| state.backoff.active(now),
            )
    }

    fn record_upstream(&self, lower_id: &str, ok: bool) {
        let now = Instant::now();
        self.client.tracker.with(
            &MirrorClient::key(self.feed, lower_id),
            self.client.refresh,
            |state| {
                if ok {
                    state.backoff.succeed();
                } else {
                    state.backoff.fail(now);
                }
            },
        );
    }

    /// The service index, or an error that backs off every package: when it
    /// cannot be read, nothing else can be either.
    async fn resources_ready(&self) -> Result<()> {
        match self.client.resources().await {
            Ok(_) => {
                self.client.tracker.upstream().succeed();
                Ok(())
            }
            Err(e) => {
                self.client.tracker.upstream().fail(Instant::now());
                Err(e)
            }
        }
    }

    async fn fetch_requested(&self, lower_id: &str, version: &NuGetVersion) -> Result<usize> {
        if self.backing_off(lower_id) || !self.wanted(lower_id, version).await {
            return Ok(0);
        }
        self.resources_ready().await?;
        match self.fetch_one(lower_id, version).await {
            Fetched::Mirrored => {
                self.follow_listing(lower_id, version, &mut None).await;
                Ok(1)
            }
            Fetched::NotMirrored => Ok(0),
            Fetched::DownloadFailed(e) => {
                // It may be this version, or the upstream: back off both.
                self.record_upstream(lower_id, false);
                Err(e)
            }
        }
    }

    async fn fetch_listing(&self, lower_id: &str) -> Result<usize> {
        let client = self.client;
        let feed = self.feed;
        // This request holds the fetch, so only the list's age matters.
        if self.backing_off(lower_id) || !client.list_is_stale(feed, lower_id, false) {
            return Ok(0);
        }
        self.resources_ready().await?;
        let listed = client.upstream_versions(lower_id).await;
        self.record_upstream(lower_id, listed.is_ok());
        let mut versions: Vec<NuGetVersion> = listed?
            .iter()
            .filter_map(|raw| match NuGetVersion::parse(raw) {
                Ok(version) => Some(version),
                // A version NuGet itself would refuse cannot be mirrored, but
                // it must not cost the package its other versions either.
                Err(e) => {
                    tracing::warn!(id = %lower_id, version = %raw, error = %e, "skipping an upstream version that does not parse");
                    None
                }
            })
            .collect();
        // Newest first, so a bounded fetch keeps the versions clients actually want.
        versions.sort_by(|a, b| b.cmp(a));
        let considered = versions.len();
        if let Some(max) = client.max_versions_per_package() {
            versions.truncate(max);
        }
        if versions.len() < considered {
            tracing::debug!(
                %feed, id = %lower_id, considered, fetching = versions.len(),
                "limiting mirrored versions (mirror.max_versions_per_package)"
            );
        }
        let mut mirrored = 0;
        let deadline = Instant::now() + MIRROR_BUDGET;
        let mut complete = true;
        let mut unlisted = None;

        for version in versions {
            if !self.wanted(lower_id, &version).await {
                continue;
            }
            if Instant::now() >= deadline {
                tracing::info!(
                    %feed,
                    id = %lower_id,
                    mirrored,
                    "mirror budget spent; the rest of this package's versions will \
                     be fetched on a later request"
                );
                complete = false;
                break;
            }
            match self.fetch_one(lower_id, &version).await {
                Fetched::Mirrored => {
                    mirrored += 1;
                    self.follow_listing(lower_id, &version, &mut unlisted).await;
                }
                Fetched::NotMirrored => {}
                Fetched::DownloadFailed(e) => {
                    tracing::warn!(%feed, id = %lower_id, version = %version.normalized(), error = %e, "mirror download failed");
                }
            }
        }
        if complete {
            client.tracker.with(
                &MirrorClient::key(feed, lower_id),
                client.refresh,
                |state| {
                    state.listed_at = Some(Instant::now());
                },
            );
        }
        Ok(mirrored)
    }

    /// Unlist a just-mirrored version when the upstream has it unlisted, so a
    /// version hidden there stays out of search and "latest" here. Reads the
    /// upstream's list once per fetch, into `unlisted`; best-effort, since a
    /// registration that cannot be read must not undo the mirroring.
    async fn follow_listing(
        &self,
        lower_id: &str,
        version: &NuGetVersion,
        unlisted: &mut Option<std::collections::HashSet<String>>,
    ) {
        let feed = self.feed;
        if unlisted.is_none() {
            *unlisted = Some(match self.client.upstream_unlisted(lower_id).await {
                Ok(set) => set,
                Err(e) => {
                    tracing::warn!(%feed, id = %lower_id, error = %e, "could not read which versions the upstream unlisted");
                    Default::default()
                }
            });
        }
        let hidden = unlisted
            .as_ref()
            .is_some_and(|set| set.contains(&version.normalized().to_lowercase()));
        if hidden {
            if let Err(e) = self.db.set_listed(feed, lower_id, version, false).await {
                tracing::warn!(%feed, id = %lower_id, error = %e, "could not unlist a version the upstream unlisted");
            }
        }
    }

    /// Download and index one version. Every failure but the download itself
    /// is logged here; a version that failed is not asked for again for a
    /// while, so a broken one cannot turn each read into a fresh download.
    async fn fetch_one(&self, lower_id: &str, version: &NuGetVersion) -> Fetched {
        let (feed, options) = (self.feed, self.options);
        let normalized = version.normalized().to_lowercase();
        let temp_path = self
            .temp_dir
            .join(format!("mirror-{}.tmp", uuid::Uuid::new_v4()));
        let summary = match self
            .client
            .download_nupkg(lower_id, &normalized, &temp_path)
            .await
        {
            Ok(summary) => summary,
            Err(e) => {
                let _ = tokio::fs::remove_file(&temp_path).await;
                self.remember_bad(lower_id, version);
                return Fetched::DownloadFailed(e);
            }
        };

        // A delete may have landed while the download ran.
        if self
            .db
            .is_tombstoned(feed, lower_id, version)
            .await
            .unwrap_or(true)
        {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Fetched::NotMirrored;
        }

        let opts = IndexOptions {
            overwrite: crate::config::OverwriteMode::Disabled,
            pending: options.requires_approval,
            license_policy: options.license_policy.clone(),
            // Pin the identity: whatever the upstream returned must be the
            // package we asked for. Otherwise a hostile upstream answers a
            // request for an obscure id with a manifest claiming a popular one,
            // and it lands in the local feed under that trusted name.
            expect: Some(crate::indexing::ExpectedIdentity {
                id: lower_id.to_string(),
                version: version.clone(),
            }),
            reserved_elsewhere: options.reserved_elsewhere.clone(),
        };
        match indexing::index_package(self.storage, self.db, feed, temp_path, summary, &opts).await
        {
            Ok(_) => {
                tracing::info!(%feed, id = %lower_id, version = %normalized, pending = options.requires_approval, "mirrored package");
                Fetched::Mirrored
            }
            // A concurrent fetch in another feed, or a push, got there first.
            Err(Error::PackageAlreadyExists)
                if self
                    .db
                    .exists(feed, lower_id, version)
                    .await
                    .unwrap_or(false) =>
            {
                Fetched::NotMirrored
            }
            Err(e) => {
                tracing::warn!(%feed, id = %lower_id, version = %normalized, error = %e, "mirror index failed");
                self.remember_bad(lower_id, version);
                Fetched::NotMirrored
            }
        }
    }
}

/// How fetching one version went.
enum Fetched {
    Mirrored,
    /// Present already, deleted meanwhile, or refused (and logged).
    NotMirrored,
    /// The download failed; the caller decides how loudly.
    DownloadFailed(Error),
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

/// Refuse to write `incoming` bytes (when the upstream declared a length) next
/// to `dest` if that would leave the volume with less than `reserve` free.
///
/// The same rule a push is held to: a full disk does not fail cleanly, since
/// the database and every other writer on the volume run out with it. With no
/// declared length only the reserve itself is checked, and the size cap bounds
/// the rest. Free space that cannot be measured lets the download through,
/// since a guard that fails closed would stop the mirror over a `statvfs`.
fn ensure_free_space(dest: &Path, incoming: Option<u64>, reserve: u64) -> Result<()> {
    if reserve == 0 {
        return Ok(());
    }
    let dir = dest.parent().unwrap_or(dest);
    let available = match fs4::available_space(dir) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "could not measure free disk space");
            return Ok(());
        }
    };
    let needed = incoming.unwrap_or(0).saturating_add(reserve);
    if available < needed {
        return Err(Error::InsufficientStorage(format!(
            "{available} bytes free on the storage volume, {needed} needed \
             (the upstream package plus the configured reserve)"
        )));
    }
    Ok(())
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
/// from its configured credentials: `Authorization` from Basic or Bearer
/// ([`MirrorAuthConfig::validate`] rules out both), plus the custom headers.
/// A header that cannot be sent is an error rather than a warning — a
/// credential dropped at startup surfaces later as a baffling 401.
fn auth_headers(
    auth: &MirrorAuthConfig,
) -> std::result::Result<reqwest::header::HeaderMap, String> {
    use base64::Engine;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};

    let secret = |raw: String| {
        HeaderValue::from_str(&raw).map(|mut value| {
            value.set_sensitive(true);
            value
        })
    };
    let mut headers = HeaderMap::new();
    if let Some(user) = &auth.username {
        let pass = auth.password.as_deref().unwrap_or("");
        let raw = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        let value = secret(format!("Basic {raw}"))
            .map_err(|_| "auth.username cannot be sent in a header".to_string())?;
        headers.insert(AUTHORIZATION, value);
    } else if let Some(token) = &auth.token {
        let value = secret(format!("Bearer {token}"))
            .map_err(|_| "auth.token cannot be sent in a header".to_string())?;
        headers.insert(AUTHORIZATION, value);
    }
    for (name, value) in &auth.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("auth.headers: {name:?} is not a valid header name"))?;
        let value = secret(value.clone())
            .map_err(|_| format!("auth.headers: the value of {name} cannot be sent"))?;
        headers.insert(name, value);
    }
    Ok(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_headers_basic_bearer_and_custom() {
        use reqwest::header::AUTHORIZATION;

        // No credentials → no headers.
        assert!(auth_headers(&MirrorAuthConfig::default())
            .unwrap()
            .is_empty());

        // Basic auth populates Authorization.
        let basic = MirrorAuthConfig {
            username: Some("user".into()),
            password: Some("pass".into()),
            ..Default::default()
        };
        let h = auth_headers(&basic).unwrap();
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
        let h = auth_headers(&bearer).unwrap();
        assert_eq!(
            h.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer tok"
        );
        assert_eq!(h.get("X-Feed-Key").unwrap().to_str().unwrap(), "abc");

        // A header that cannot be sent fails the client instead of vanishing.
        let mut bad = std::collections::BTreeMap::new();
        bad.insert("Not A Header".to_string(), "x".to_string());
        assert!(auth_headers(&MirrorAuthConfig {
            headers: bad,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn basic_and_bearer_together_are_a_configuration_error() {
        // Both need `Authorization`; the token used to be dropped silently.
        let both = MirrorConfig {
            enabled: true,
            auth: MirrorAuthConfig {
                username: Some("ci".into()),
                password: Some("pw".into()),
                token: Some("tok".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(both.validate().is_err());
        assert!(MirrorClient::try_from_config(&both).is_err());
        // `from_config` logs it and turns the mirror off rather than guessing.
        assert!(MirrorClient::from_config(&both).is_none());

        let orphan_password = MirrorAuthConfig {
            password: Some("pw".into()),
            ..Default::default()
        };
        assert!(orphan_password.validate().is_err());
    }

    #[test]
    fn debug_output_carries_no_secrets() {
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("X-Feed-Key".to_string(), "header-secret".to_string());
        let config = MirrorConfig {
            enabled: true,
            upstream: "https://user:url-secret@feed.example/v3/index.json".into(),
            auth: MirrorAuthConfig {
                username: Some("ci".into()),
                password: Some("password-secret".into()),
                headers,
                ..Default::default()
            },
            proxy: Some("http://proxy:proxy-secret@proxy.internal:3128".into()),
            ..Default::default()
        };
        let client = MirrorClient::from_config(&config).unwrap();
        for shown in [format!("{config:?}"), format!("{client:?}")] {
            for secret in [
                "url-secret",
                "password-secret",
                "header-secret",
                "proxy-secret",
            ] {
                assert!(!shown.contains(secret), "{secret} in {shown}");
            }
            assert!(shown.contains("feed.example"), "{shown}");
        }
        assert!(format!("{config:?}").contains("X-Feed-Key"));
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

    #[test]
    fn backoff_doubles_up_to_a_cap_and_resets_on_success() {
        let now = Instant::now();
        let mut backoff = Backoff::default();
        assert!(!backoff.active(now));
        backoff.fail(now);
        assert_eq!(backoff.until, Some(now + FIRST_BACKOFF));
        backoff.fail(now);
        assert_eq!(backoff.until, Some(now + FIRST_BACKOFF * 2));
        for _ in 0..40 {
            backoff.fail(now);
        }
        assert_eq!(backoff.until, Some(now + MAX_BACKOFF));
        assert!(backoff.active(now));
        backoff.succeed();
        assert!(!backoff.active(now));
    }

    #[test]
    fn the_fetch_tracker_stays_bounded() {
        // Every unknown id an anonymous client names gets an entry (that is
        // the negative cache); the map must not grow without end.
        let tracker = Tracker::default();
        let refresh = Duration::from_secs(600);
        for i in 0..MAX_TRACKED * 2 {
            tracker.with(&("feed".into(), format!("pkg.{i}")), refresh, |s| {
                s.listed_at = Some(Instant::now());
            });
        }
        assert!(tracker.lock().len() <= MAX_TRACKED);
    }

    #[test]
    fn only_one_fetch_per_package_and_feed_runs_at_once() {
        let tracker = Tracker::default();
        let refresh = Duration::from_secs(600);
        let a = ("a".to_string(), "pkg".to_string());
        let b = ("b".to_string(), "pkg".to_string());
        let Turn::Lead(first) = tracker.turn(&a, refresh) else {
            panic!("the first fetch leads");
        };
        let Turn::Wait(waiting) = tracker.turn(&a, refresh) else {
            panic!("a second fetch of the same package waits");
        };
        // Another feed mirroring the same id does not wait on this one.
        assert!(matches!(tracker.turn(&b, refresh), Turn::Lead(_)));
        drop(first);
        assert!(waiting.has_changed().is_err(), "the waiter is woken");
        assert!(matches!(tracker.turn(&a, refresh), Turn::Lead(_)));
    }

    #[tokio::test]
    async fn the_resolver_drops_private_addresses() {
        use reqwest::dns::Resolve;

        // `localhost` resolves to loopback on every platform, which is exactly
        // what a name with a private `A` record looks like to the mirror.
        let guarded = GuardedResolver::default();
        match guarded.resolve("localhost".parse().unwrap()).await {
            Ok(_) => panic!("a name resolving to loopback must not connect"),
            Err(e) => assert!(e.to_string().contains("private address"), "{e}"),
        }

        // The operator's own proxy is exempt: it is routinely on the private
        // network, and the operator named it.
        let proxied = GuardedResolver {
            proxy_host: Some("LOCALHOST".into()),
            ..Default::default()
        };
        let addrs = proxied
            .resolve("localhost".parse().unwrap())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(addrs.count() > 0);
    }

    #[tokio::test]
    async fn a_redirect_or_resource_that_resolves_to_a_private_address_is_refused() {
        use axum::http::{header, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::get;

        // One loopback server plays every host. `upstream.test` stands for a
        // public upstream (reqwest resolves it, around the guard);
        // `internal.test` is a name whose DNS answer is a private address, as
        // `metadata.evil.example` pointing at 169.254.169.254 would be.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        let public = format!("http://upstream.test:{port}");
        let redirect = |to: String| {
            move || {
                let to = to.clone();
                async move { (StatusCode::FOUND, [(header::LOCATION, to)]).into_response() }
            }
        };
        let index = serde_json::json!({
            "resources": [{"@id": format!("{public}/flat/"), "@type": "PackageBaseAddress/3.0.0"}]
        });
        let app = axum::Router::new()
            .route(
                "/v3/index.json",
                get(move || async move { axum::Json(index) }),
            )
            .route(
                "/versions.json",
                get(|| async { axum::Json(serde_json::json!({"versions": ["1.0.0"]})) }),
            )
            .route(
                "/flat/ok/index.json",
                get(redirect(format!("{public}/versions.json"))),
            )
            .route(
                "/flat/by-name/index.json",
                get(redirect(format!(
                    "http://internal.test:{port}/versions.json"
                ))),
            )
            .route(
                "/flat/by-literal/index.json",
                get(redirect(format!("http://127.0.0.1:{port}/versions.json"))),
            )
            .route(
                "/flat/rebound/index.json",
                get(redirect(format!(
                    "http://[::ffff:127.0.0.1]:{port}/versions.json"
                ))),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let config = MirrorConfig {
            enabled: true,
            upstream: format!("{public}/v3/index.json"),
            timeout_secs: 5,
            ..Default::default()
        };
        let dns = TestDns {
            unguarded: vec![("upstream.test".into(), addr)],
            guarded: vec![("internal.test".into(), addr.ip())],
        };
        let client = MirrorClient::build_with(&config, false, &dns).unwrap();

        // The control: a redirect within the public upstream is followed.
        assert_eq!(client.upstream_versions("ok").await.unwrap(), ["1.0.0"]);
        for refused in ["by-name", "by-literal", "rebound"] {
            let err = client.upstream_versions(refused).await.unwrap_err();
            assert!(
                err.to_string().contains("private address"),
                "{refused}: {err}"
            );
        }

        // The same name as a resource URL, not a redirect, is refused too.
        let direct = MirrorClient::build_with(
            &MirrorConfig {
                upstream: format!("http://internal.test:{port}/v3/index.json"),
                ..config.clone()
            },
            false,
            &dns,
        )
        .unwrap();
        let err = direct.upstream_versions("ok").await.unwrap_err();
        assert!(err.to_string().contains("private address"), "{err}");
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
        .ids
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
