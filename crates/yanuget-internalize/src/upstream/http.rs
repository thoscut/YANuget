//! HTTP to the upstream, and to the hosts captured downloads point at: every
//! request goes through [`HttpClient::get`], which applies the address policy,
//! follows redirects one hop at a time, sends the credentials only to the
//! upstream's own origin, limits concurrency, backs off on `429` and `503`,
//! and logs once when the upstream stops answering and once when it answers
//! again.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, LOCATION, RETRY_AFTER};
use reqwest::{StatusCode, Url};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use yanuget_pkg::address::{is_private_host, is_private_ip_addr, GuardedResolver};

use super::UpstreamError;
use crate::config::UpstreamConfig;

/// How many redirects one request may follow.
pub const MAX_REDIRECTS: usize = 5;

type Origin = (String, String, u16);

fn origin(url: &Url) -> Option<Origin> {
    Some((
        url.scheme().to_string(),
        url.host_str()?.to_ascii_lowercase(),
        url.port_or_known_default()?,
    ))
}

/// A URL for logs and errors: no credentials, no query (signed download links
/// carry their signature there).
pub fn redact(url: &Url) -> String {
    let mut u = url.clone();
    let _ = u.set_username("");
    let _ = u.set_password(None);
    if u.query().is_some() {
        u.set_query(Some("…"));
    }
    u.to_string()
}

/// When to try a request again.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Tries in all, the first one included.
    pub attempts: u32,
    /// The first wait without a `Retry-After`; each further wait doubles it.
    pub base: Duration,
    /// The longest wait, whatever `Retry-After` asks for.
    pub max_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            attempts: 5,
            base: Duration::from_secs(2),
            max_wait: Duration::from_secs(120),
        }
    }
}

/// A response, holding its concurrency slot until the body is read.
pub struct Fetched {
    pub response: reqwest::Response,
    _permit: OwnedSemaphorePermit,
}

/// What a client is for, and how it may reach out.
#[derive(Debug, Clone)]
pub struct Options {
    /// Names the far side in errors and logs: `upstream`, `download`.
    pub label: &'static str,
    /// The base URL whose origin gets `credentials`, and nothing else does.
    pub origin: Option<Url>,
    pub credentials: HeaderMap,
    /// Allow every loopback, private and link-local address.
    pub allow_private: bool,
    /// Hosts allowed although they are, or resolve to, such an address.
    pub allowed_private_hosts: Vec<String>,
    pub proxy: Option<String>,
    /// For connecting and for any silence while the far side answers.
    pub timeout: Duration,
    pub concurrency: usize,
    /// How the operator permits a private address, for the error.
    pub hint: &'static str,
    /// Log once when the far side stops answering and once when it answers
    /// again: for one upstream, not for downloads from many hosts.
    pub watch_health: bool,
}

#[derive(Debug)]
pub struct HttpClient {
    client: reqwest::Client,
    label: &'static str,
    origin: Option<Origin>,
    credentials: HeaderMap,
    allow_private: bool,
    allowed_private_hosts: Vec<String>,
    /// A proxy resolves the names itself, past the guarded resolver, so each
    /// host is also resolved here before a request.
    proxied: bool,
    hint: &'static str,
    watch_health: bool,
    timeout: Duration,
    permits: Arc<Semaphore>,
    retry: RetryPolicy,
    down: AtomicBool,
}

impl HttpClient {
    /// The client for the upstream feed.
    pub fn new(config: &UpstreamConfig) -> Result<Self, UpstreamError> {
        let invalid = |m: String| UpstreamError::Invalid(format!("upstream: {m}"));
        let base = Url::parse(&config.url).map_err(|e| invalid(e.to_string()))?;
        if base.host_str().is_none() {
            return Err(invalid("the URL has no host".into()));
        }
        Self::with_options(Options {
            label: "upstream",
            origin: Some(base),
            credentials: credentials(config).map_err(invalid)?,
            allow_private: config.allow_private,
            allowed_private_hosts: Vec::new(),
            proxy: config.proxy.clone(),
            timeout: Duration::from_secs(config.timeout_secs.max(1)),
            concurrency: config.concurrency,
            hint: "set upstream.allow_private = true to permit it",
            watch_health: true,
        })
    }

    pub fn with_options(options: Options) -> Result<Self, UpstreamError> {
        let label = options.label;
        let invalid = |m: String| UpstreamError::Invalid(format!("{label}: {m}"));
        let timeout = options.timeout;
        let mut builder = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout)
            .user_agent(crate::USER_AGENT)
            // Followed in `send`, which knows where the credentials may go.
            .redirect(reqwest::redirect::Policy::none());
        // A proxy only when the configuration names one: one from the
        // environment would be an egress path nobody chose, and it resolves
        // names itself, past the guarded resolver.
        let mut proxy_host = None;
        match &options.proxy {
            Some(p) => {
                builder = builder.proxy(
                    reqwest::Proxy::all(p.as_str())
                        .map_err(|_| invalid("proxy is not a valid URL".into()))?,
                );
                proxy_host = Url::parse(p)
                    .ok()
                    .and_then(|u| u.host_str().map(str::to_string));
            }
            None => builder = builder.no_proxy(),
        }
        if !options.allow_private {
            builder = builder.dns_resolver(Arc::new(AllowListed {
                guarded: GuardedResolver {
                    proxy_host,
                    fixed: Vec::new(),
                    hint: Some(options.hint.into()),
                },
                hosts: options.allowed_private_hosts.clone(),
            }));
        }
        let client = builder
            .build()
            .map_err(|e| invalid(format!("cannot build the HTTP client: {e}")))?;
        Ok(HttpClient {
            client,
            label,
            origin: options.origin.as_ref().and_then(origin),
            credentials: options.credentials,
            allow_private: options.allow_private,
            allowed_private_hosts: options.allowed_private_hosts,
            proxied: options.proxy.is_some(),
            hint: options.hint,
            watch_health: options.watch_health,
            timeout,
            permits: Arc::new(Semaphore::new(options.concurrency.max(1))),
            retry: RetryPolicy::default(),
            down: AtomicBool::new(false),
        })
    }

    pub fn set_retry(&mut self, retry: RetryPolicy) {
        self.retry = retry;
    }

    /// The limit on one metadata request.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Whether the last request found the upstream failing.
    pub fn is_down(&self) -> bool {
        self.down.load(Ordering::Relaxed)
    }

    /// Whether `url` may be fetched at all: http or https, and not a private
    /// address unless that is allowed. Names that resolve to one are stopped
    /// by the resolver when the connection is made.
    pub fn check(&self, url: &Url, what: &str) -> Result<(), UpstreamError> {
        let label = self.label;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(UpstreamError::Refused(format!(
                "{label} {what} {} uses the scheme {:?}",
                redact(url),
                url.scheme()
            )));
        }
        if !self.allow_private {
            if let Some(host) = url.host_str() {
                if is_private_host(host) && !allowed(&self.allowed_private_hosts, host) {
                    return Err(UpstreamError::Refused(format!(
                        "{label} {what} {} points at the private address {host}; {}",
                        redact(url),
                        self.hint
                    )));
                }
            }
        }
        Ok(())
    }

    /// [`Self::check`], and behind a proxy the host's addresses as well: the
    /// proxy resolves the name itself, so the guarded resolver never sees it.
    /// A name that does not resolve here is refused, since its address
    /// cannot be told. The proxy resolves again, so a DNS answer that changes
    /// in between gets past this; the proxy should refuse private
    /// destinations too.
    async fn vet(&self, url: &Url, what: &str) -> Result<(), UpstreamError> {
        self.check(url, what)?;
        let Some(host) = url.host_str() else {
            return Ok(());
        };
        if !self.proxied
            || self.allow_private
            || allowed(&self.allowed_private_hosts, host)
            // A literal address was classified by `check`.
            || host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .parse::<std::net::IpAddr>()
                .is_ok()
        {
            return Ok(());
        }
        let label = self.label;
        let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, 0))
            .await
            .map_err(|e| {
                UpstreamError::Refused(format!(
                    "{label} {what} {}: cannot resolve {host} here to check its address ({e}); \
                     behind a proxy every host is also resolved locally",
                    redact(url)
                ))
            })?
            .collect();
        if let Some(private) = addrs.iter().find(|a| is_private_ip_addr(a.ip())) {
            return Err(UpstreamError::Refused(format!(
                "{label} {what} {}: {host} resolves to the private address {}; {}",
                redact(url),
                private.ip(),
                self.hint
            )));
        }
        Ok(())
    }

    fn carries_credentials(&self, url: &Url) -> bool {
        self.origin.is_some() && origin(url) == self.origin
    }

    /// GET `url`, retrying `429`, `503` and transport errors. Any other
    /// status is returned as it is; the caller decides what a `404` means.
    pub async fn get(&self, url: &Url, what: &str) -> Result<Fetched, UpstreamError> {
        self.get_with(url, what, &HeaderMap::new()).await
    }

    /// [`Self::get`] with `headers` on every hop, a `Range` for example.
    pub async fn get_with(
        &self,
        url: &Url,
        what: &str,
        headers: &HeaderMap,
    ) -> Result<Fetched, UpstreamError> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("the semaphore is never closed");
        let mut attempt = 0;
        loop {
            attempt += 1;
            let last = attempt >= self.retry.attempts;
            match self.send(url, what, headers).await {
                Ok(resp)
                    if !last
                        && matches!(
                            resp.status(),
                            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE
                        ) =>
                {
                    let wait = self.wait(attempt, retry_after(resp.headers()));
                    tracing::info!(
                        url = %redact(url),
                        status = resp.status().as_u16(),
                        wait_secs = wait.as_secs_f32(),
                        label = self.label,
                        "the far side asks to slow down; waiting"
                    );
                    tokio::time::sleep(wait).await;
                }
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS {
                        self.record_failure(&format!("{what} {} answered {status}", redact(url)));
                    } else {
                        self.record_success();
                    }
                    return Ok(Fetched {
                        response: resp,
                        _permit: permit,
                    });
                }
                Err(e @ UpstreamError::Http(_)) if !last => {
                    tracing::debug!(error = %e, "request failed; trying again");
                    tokio::time::sleep(self.wait(attempt, None)).await;
                }
                Err(e) => {
                    if matches!(e, UpstreamError::Http(_)) {
                        self.record_failure(&e.to_string());
                    }
                    return Err(e);
                }
            }
        }
    }

    /// The wait before try `attempt + 1`, for callers that retry on their own.
    pub fn backoff(&self, attempt: u32) -> Duration {
        self.wait(attempt.max(1), None)
    }

    fn wait(&self, attempt: u32, asked: Option<Duration>) -> Duration {
        let backoff = self
            .retry
            .base
            .saturating_mul(1u32 << (attempt - 1).min(16));
        asked.unwrap_or(backoff).min(self.retry.max_wait)
    }

    /// One request, its redirects followed hop by hop: each hop is vetted
    /// like the first URL, a hop from https to http is refused, and the
    /// credentials go only to the upstream's origin, so a download handed off
    /// to a CDN is fetched without them.
    async fn send(
        &self,
        url: &Url,
        what: &str,
        headers: &HeaderMap,
    ) -> Result<reqwest::Response, UpstreamError> {
        let label = self.label;
        self.vet(url, what).await?;
        let mut current = url.clone();
        for _ in 0..=MAX_REDIRECTS {
            let mut request = self.client.get(current.clone()).headers(headers.clone());
            if self.carries_credentials(&current) {
                request = request.headers(self.credentials.clone());
            }
            let resp = request.send().await.map_err(|e| {
                UpstreamError::Http(format!(
                    "{label} {what} {}: {}",
                    redact(&current),
                    chain(&e)
                ))
            })?;
            if !resp.status().is_redirection() {
                return Ok(resp);
            }
            let location = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    UpstreamError::Invalid(format!(
                        "{label} {what} {} answered {} without a usable Location",
                        redact(&current),
                        resp.status()
                    ))
                })?;
            let next = current.join(location).map_err(|e| {
                UpstreamError::Invalid(format!(
                    "{label} {what} {} redirected to an invalid URL: {e}",
                    redact(&current)
                ))
            })?;
            if current.scheme() == "https" && next.scheme() == "http" {
                return Err(UpstreamError::Refused(format!(
                    "{label} {what} {} redirected from https to http; refusing the downgrade",
                    redact(&current)
                )));
            }
            self.vet(&next, what).await?;
            current = next;
        }
        Err(UpstreamError::Invalid(format!(
            "{label} {what} {} redirected more than {MAX_REDIRECTS} times",
            redact(url)
        )))
    }

    fn record_failure(&self, error: &str) {
        if self.watch_health && !self.down.swap(true, Ordering::Relaxed) {
            tracing::warn!(error, "the {} stopped answering", self.label);
        }
    }

    fn record_success(&self) {
        if self.watch_health && self.down.swap(false, Ordering::Relaxed) {
            tracing::info!("the {} answers again", self.label);
        }
    }
}

/// Whether `host` is on an allow-list of host names, compared without case.
fn allowed(hosts: &[String], host: &str) -> bool {
    let bare = |h: &str| {
        h.trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase()
    };
    let host = bare(host);
    hosts.iter().any(|h| bare(h) == host)
}

/// The guarded resolver, except for the hosts the operator allowed.
struct AllowListed {
    guarded: GuardedResolver,
    hosts: Vec<String>,
}

impl reqwest::dns::Resolve for AllowListed {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        if !allowed(&self.hosts, name.as_str()) {
            return self.guarded.resolve(name);
        }
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// `Retry-After` in seconds. The HTTP-date form is rare from feeds and is
/// treated as absent, which falls back to the backoff.
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

fn credentials(config: &UpstreamConfig) -> Result<HeaderMap, String> {
    use base64::Engine;
    let mut headers = HeaderMap::new();
    let mut put = |name: HeaderName, value: String| -> Result<(), String> {
        let mut v = HeaderValue::from_str(&value)
            .map_err(|_| format!("the value for {name} is not a valid header value"))?;
        v.set_sensitive(true);
        headers.insert(name, v);
        Ok(())
    };
    if let (Some(user), Some(password)) = (&config.username, &config.password) {
        let pair = format!("{user}:{}", password.expose());
        put(
            AUTHORIZATION,
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(pair)
            ),
        )?;
    }
    if let Some(token) = &config.token {
        put(AUTHORIZATION, format!("Bearer {}", token.expose()))?;
    }
    for (name, value) in &config.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| format!("{name:?} is not a header name"))?;
        put(name, value.expose().to_string())?;
    }
    Ok(headers)
}

/// An error with its causes, which is where reqwest says what went wrong.
fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let c = cause.to_string();
        if !s.contains(&c) {
            s.push_str(": ");
            s.push_str(&c);
        }
        source = cause.source();
    }
    s
}
