//! The HTTP client for one upstream: building it, vetting and sending
//! requests, and the errors and redaction around them.

use std::time::Duration;

use futures::StreamExt;
use tokio::sync::OnceCell;
use yanuget_pkg::fetch::{decode_capped, DecodeError};

use crate::config::{MirrorAuthConfig, MirrorConfig};
use crate::error::{Error, Result};

use super::health::UpstreamHealth;
use super::tracker::Tracker;
use super::MirrorClient;

/// Host names a test resolves without DNS. Empty outside tests.
#[derive(Default)]
pub(super) struct TestDns {
    /// Answered by reqwest itself, around the guard: a stand-in for a public
    /// host that happens to be a loopback test server.
    unguarded: Vec<(String, std::net::SocketAddr)>,
    /// Answered by the guarded resolver, and filtered like real DNS answers.
    guarded: Vec<(String, std::net::IpAddr)>,
}

/// `(scheme, host, port)`: what two URLs must share for a credential meant for
/// one to be sent to the other.
pub(super) type Origin = (String, String, u16);

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

/// Default cap on a single mirrored `.nupkg` when neither the feed nor the
/// server configured one.
const DEFAULT_MIRROR_MAX_PACKAGE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

use yanuget_pkg::address::GuardedResolver;

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
        let proxied = config.proxy.is_some() || (env_proxy && env_proxy_set());
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
                hint: Some("set allow_private_upstream = true to permit it".into()),
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
            proxied,
            fixed_dns: dns.guarded.clone(),
            max_package_size_bytes: config.max_package_size_bytes,
            max_versions_per_package: config.max_versions_per_package,
            timeout,
            download_deadline: match config.download_timeout_secs {
                0 => None,
                secs => Some(Duration::from_secs(secs)),
            },
            min_free_disk_bytes: 0,
            space: None,
            refresh: Duration::from_secs(config.refresh_secs),
            resources: OnceCell::new(),
            tracker: Tracker::default(),
            health: UpstreamHealth::default(),
            feed: String::new(),
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

    /// Admit downloads through `space` — the guard the server's uploads go
    /// through — instead of checking the reserve alone: a download then
    /// reserves the length the upstream declared, as an upload does, and
    /// counts what is promised to the uploads in progress. Downloads must be
    /// written to `space`'s volume.
    pub fn set_space(&mut self, space: std::sync::Arc<crate::space::SpaceGuard>) {
        self.space = Some(space);
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
    pub(super) fn check_url(&self, url: &str, what: &str) -> Result<()> {
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
    /// Behind a proxy, which resolves the names itself past
    /// [`GuardedResolver`], resolve the host here as well and refuse it when
    /// it does not resolve or resolves to a private address (see
    /// [`yanuget_pkg::address::check_resolved`]).
    async fn check_proxied(&self, url: &reqwest::Url, what: &str) -> Result<()> {
        let Some(host) = url.host_str() else {
            return Ok(());
        };
        if !self.proxied || self.allow_private_upstream {
            return Ok(());
        }
        yanuget_pkg::address::check_resolved(host, &self.fixed_dns)
            .await
            .map_err(|why| {
                Error::Other(anyhow::anyhow!(
                    "upstream {what} {}: refused, since {host} {}; \
                     set allow_private_upstream = true to permit it",
                    redact_url(url.as_str()),
                    match why {
                        yanuget_pkg::address::UnsafeHost::Private(ip) =>
                            format!("resolves to the private address {ip}"),
                        yanuget_pkg::address::UnsafeHost::Unresolvable(e) => format!(
                            "cannot be resolved here to check its address ({e}); behind a \
                             proxy every host is also resolved locally"
                        ),
                    }
                ))
            })
    }

    pub(super) fn log_check(&self, url: &str, what: &str) -> bool {
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
    ///
    /// Every outcome is recorded in the client's [`UpstreamStatus`](super::UpstreamStatus): an
    /// upstream that cannot be reached, or answers with a server error or a
    /// refusal of the credentials, counts as failing.
    pub(super) async fn send(
        &self,
        url: &str,
        what: &str,
        deadline: Option<Duration>,
    ) -> Result<reqwest::Response> {
        let result = self.send_unrecorded(url, what, deadline).await;
        match &result {
            Ok(resp) if upstream_failed(resp.status()) => self.record_failure(&format!(
                "upstream {what} {} answered {}",
                redact_url(resp.url().as_str()),
                resp.status()
            )),
            Ok(_) => self.record_success(),
            Err(e) => self.record_failure(&e.to_string()),
        };
        result
    }

    async fn send_unrecorded(
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
            self.check_proxied(&current, what).await?;
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
    pub(super) fn check_status(
        &self,
        resp: reqwest::Response,
        what: &str,
    ) -> Result<reqwest::Response> {
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
        Err(Error::Other(anyhow::Error::new(UpstreamAnswer {
            what: what.to_string(),
            url: redact_url(resp.url().as_str()),
            status,
            hint,
        })))
    }

    /// Fetch and parse one upstream JSON document: the single path every
    /// metadata request takes, so each is vetted ([`Self::send`]), carries the
    /// credentials only where they belong, is bounded in time by
    /// `timeout_secs` and in size by [`MAX_JSON_BYTES`]. `None` is a 404.
    ///
    /// The body is read here; decompressing and parsing it, which take a
    /// while for a large registration, run on the blocking pool.
    pub(super) async fn get_json<T>(&self, url: &str, what: &str) -> Result<Option<T>>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
        Ok(self.get_json_at(url, what).await?.map(|(doc, _)| doc))
    }

    /// [`Self::get_json`], with the URL the document came from in the end,
    /// after any redirects: the base its relative `@id`s resolve against.
    pub(super) async fn get_json_at<T>(
        &self,
        url: &str,
        what: &str,
    ) -> Result<Option<(T, reqwest::Url)>>
    where
        T: serde::de::DeserializeOwned + Send + 'static,
    {
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
        let label = what.to_string();
        let shown = redact_url(url);
        let doc = tokio::task::spawn_blocking(move || {
            let body = decode_capped(body, gzipped, MAX_JSON_BYTES).map_err(|e| match e {
                DecodeError::TooLarge => Error::Other(anyhow::anyhow!(
                    "upstream {label} {shown} is larger than {MAX_JSON_BYTES} bytes"
                )),
                DecodeError::Gzip(e) => Error::Other(anyhow::anyhow!(
                    "upstream {label} {shown} is not valid gzip: {e}"
                )),
            })?;
            serde_json::from_slice(&body).map(Some).map_err(|e| {
                Error::Other(anyhow::anyhow!(
                    "upstream {label} {shown} is not valid JSON: {e}"
                ))
            })
        })
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("parsing the upstream {what}: {e}")))??;
        Ok(doc.map(|doc| (doc, final_url)))
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
}

/// An upstream answer that was not a success, as [`MirrorClient::check_status`]
/// reports it. A type of its own so [`permanent_download_failure`] can tell
/// an absent package from an upstream having a bad moment.
#[derive(Debug, thiserror::Error)]
#[error("upstream {what} {url} answered {status}{hint}")]
struct UpstreamAnswer {
    what: String,
    url: String,
    status: reqwest::StatusCode,
    hint: String,
}

/// Whether a failed package download will fail the same way when asked
/// again soon: the upstream refused this request (a `4xx` other than a
/// timeout or a rate limit; `404` and `410` above all), or the package is
/// over the size cap. A timeout, a reset connection, a `5xx` or a full disk
/// is transient, and the upstream's own back-off already spaces out retries.
pub(super) fn permanent_download_failure(e: &Error) -> bool {
    match e {
        Error::PayloadTooLarge(_) => true,
        Error::Other(e) => e
            .downcast_ref::<UpstreamAnswer>()
            .is_some_and(|s| s.status.is_client_error() && !matches!(s.status.as_u16(), 408 | 429)),
        _ => false,
    }
}

/// Whether an upstream answer means the upstream is not usable: a server
/// error, a refusal of the credentials, or a rate limit. A `404` is a normal
/// answer (the upstream lacks that package), and so is anything else.
fn upstream_failed(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || matches!(status.as_u16(), 401 | 403 | 407 | 429)
}

/// Whether the environment names a proxy reqwest would use.
fn env_proxy_set() -> bool {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ]
    .iter()
    .any(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
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
pub(super) fn error_chain(e: &dyn std::error::Error) -> String {
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
        Ok(parsed) => yanuget_pkg::fetch::redact(&parsed),
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
    use yanuget_pkg::v3::ServiceIndex;

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

    /// The first resource of any of `types`, by preference, as a string.
    fn find_resource(index: &ServiceIndex, base: &reqwest::Url, types: &[&str]) -> Option<String> {
        index.resolve(types, base).map(String::from)
    }

    #[test]
    fn finds_first_resource_in_priority_order() {
        let index = service_index(serde_json::json!({
            "resources": [
                {"@id": "https://up/search-rc", "@type": "SearchQueryService/3.0.0-rc"},
                {"@id": "https://up/search", "@type": "SearchQueryService"}
            ]
        }));
        // 3.5.0 is absent, so the next candidate (3.0.0-rc) wins.
        assert_eq!(
            find_resource(
                &index,
                &url("https://up/v3/index.json"),
                &[
                    "SearchQueryService/3.5.0",
                    "SearchQueryService/3.0.0-rc",
                    "SearchQueryService"
                ]
            )
            .as_deref(),
            Some("https://up/search-rc")
        );
        let base = url("https://up/v3/index.json");
        assert!(find_resource(&index, &base, &["Catalog/3.0.0"]).is_none());
    }

    fn service_index(v: serde_json::Value) -> ServiceIndex {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn finds_resource_by_type() {
        let index = service_index(serde_json::json!({
            "resources": [
                {"@id": "https://up/flat/", "@type": "PackageBaseAddress/3.0.0"},
                {"@id": "https://up/query", "@type": "SearchQueryService"}
            ]
        }));
        let base = url("https://up/v3/index.json");
        assert_eq!(
            find_resource(&index, &base, &["PackageBaseAddress/3.0.0"]).as_deref(),
            Some("https://up/flat/")
        );
        assert!(find_resource(&index, &base, &["Nope"]).is_none());
    }

    #[test]
    fn finds_resources_typed_by_a_list_and_relative_to_the_index() {
        let index = service_index(serde_json::json!({
            "resources": [
                {"@id": "flat/", "@type": ["PackageBaseAddress/3.0.0", "Other"]},
                {"@id": "/query", "@type": ["SearchQueryService/3.5.0"]}
            ]
        }));
        let base = url("https://up/v3/index.json");
        assert_eq!(
            find_resource(&index, &base, &["PackageBaseAddress/3.0.0"]).as_deref(),
            Some("https://up/v3/flat/")
        );
        assert_eq!(
            find_resource(&index, &base, &["SearchQueryService/3.5.0"]).as_deref(),
            Some("https://up/query")
        );
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

    /// Behind a proxy the guarded resolver never sees the upstream's names,
    /// so each is resolved here too: one with a private address, or none,
    /// is refused before the proxy is asked (SEC-36).
    #[tokio::test]
    async fn behind_a_proxy_each_host_is_resolved_here_first() {
        // Nothing listens at the proxy: a request that got that far would
        // fail with a connection error instead.
        let config = MirrorConfig {
            enabled: true,
            upstream: "http://internal.test/v3/index.json".into(),
            proxy: Some("http://127.0.0.1:9".into()),
            timeout_secs: 5,
            ..Default::default()
        };
        let dns = TestDns {
            unguarded: Vec::new(),
            guarded: vec![("internal.test".into(), "169.254.169.254".parse().unwrap())],
        };
        let client = MirrorClient::build_with(&config, false, &dns).unwrap();
        let err = client.upstream_versions("pkg").await.unwrap_err();
        assert!(
            err.to_string()
                .contains("resolves to the private address 169.254.169.254"),
            "{err}"
        );

        let unknown = MirrorClient::build_with(
            &MirrorConfig {
                upstream: "http://nothing.invalid/v3/index.json".into(),
                ..config.clone()
            },
            false,
            &dns,
        )
        .unwrap();
        let err = unknown.upstream_versions("pkg").await.unwrap_err();
        assert!(err.to_string().contains("cannot be resolved here"), "{err}");

        // Allowed private upstreams skip the check, and reach the proxy.
        let allowed = MirrorClient::build_with(
            &MirrorConfig {
                allow_private_upstream: true,
                ..config
            },
            false,
            &dns,
        )
        .unwrap();
        let err = allowed.upstream_versions("pkg").await.unwrap_err();
        assert!(!err.to_string().contains("private address"), "{err}");
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
}
