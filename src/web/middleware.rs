//! Middleware: the process-wide layers every served app gets (body limit,
//! CORS, tracing, rate limiting, the host and cross-site request guard,
//! security headers, forwarded-header filtering), and the per-feed layers for
//! read-gated feeds and the gallery's HTML error pages.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::Router;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use super::helpers::forwarded;
use super::{ui, AppState};
use crate::config::RateLimitConfig;
use crate::error::Error;
use crate::proxy::{self, TrustedProxies};
use crate::ratelimit::{self, RateLimiter};

/// The process-wide middleware settings, derived once from the served feeds.
pub(super) struct GlobalLayers {
    rate_limit: Option<RateLimitConfig>,
    /// Stamp HSTS (only when this process terminates TLS itself).
    hsts: bool,
    trusted_proxies: Arc<TrustedProxies>,
    /// Browser origins allowed to read this server. Empty means no CORS headers.
    cors_allowed_origins: Vec<String>,
    /// Host names requests may carry; `None` accepts any.
    allowed_hosts: Option<Arc<Vec<String>>>,
}

/// The CORS layer for the configured origins.
///
/// Nothing at all when none are configured, which is the default. CORS only
/// constrains browsers — a NuGet client neither sends `Origin` nor cares about
/// the response header — so the permissive `*` this replaces bought clients
/// nothing while letting any page an employee visited read a network-gated
/// feed's whole inventory out of `/v3/search`.
///
/// `*` remains available as an explicit choice, and is the right one for a feed
/// that really is public.
///
/// Either way only reads are allowed cross-origin. A browser page has no
/// business pushing, deleting or relisting on a package feed, and `permissive()`
/// allowed every method: on a feed without an API key — the default — that let
/// any page a user visited change it.
fn cors_layer(origins: &[String]) -> CorsLayer {
    use axum::http::Method;
    const READS: [Method; 3] = [Method::GET, Method::HEAD, Method::OPTIONS];
    if origins.is_empty() {
        // `CorsLayer::new()` adds no headers at all.
        return CorsLayer::new();
    }
    if origins.iter().any(|o| o == "*") {
        return CorsLayer::new()
            .allow_origin(tower_http::cors::Any)
            .allow_methods(READS)
            .allow_headers(tower_http::cors::Any)
            .expose_headers(tower_http::cors::Any);
    }
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|o| match HeaderValue::from_str(o) {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!(origin = %o, "ignoring unparseable cors_allowed_origins entry");
                None
            }
        })
        .collect();
    CorsLayer::new()
        .allow_origin(parsed)
        .allow_methods(READS)
        .allow_headers(tower_http::cors::Any)
}

impl GlobalLayers {
    pub(super) fn from_states(states: &[AppState]) -> Self {
        let config = states.first().map(|s| s.config.clone());
        Self {
            rate_limit: config.as_ref().map(|c| c.rate_limit.clone()),
            hsts: config.as_ref().is_some_and(|c| c.tls_enabled),
            cors_allowed_origins: config
                .as_ref()
                .map(|c| c.cors_allowed_origins.clone())
                .unwrap_or_default(),
            allowed_hosts: config
                .as_ref()
                .and_then(|c| c.host_allowlist())
                .map(Arc::new),
            trusted_proxies: Arc::new(
                config
                    .as_ref()
                    .map(|c| c.trusted_proxies())
                    .unwrap_or_default(),
            ),
        }
    }
}

/// Apply the process-wide middleware shared by every served app: an unbounded
/// body limit (uploads stream straight to disk, so axum's small default cap is
/// removed; the configured size limit is still enforced while streaming),
/// permissive CORS, request tracing, per-IP rate limiting (when enabled),
/// baseline security response headers and — when TLS is on — HSTS.
///
/// `.layer()` wraps what came before it, so the **last** layer added is the
/// outermost and runs first. The forwarding-header filter must see the request
/// before anything that reads those headers (the rate limiter and every URL
/// builder), so it is added last.
pub(super) fn apply_global_layers(router: Router, layers: GlobalLayers) -> Router {
    let mut router = router
        .layer(DefaultBodyLimit::disable())
        .layer(cors_layer(&layers.cors_allowed_origins))
        .layer(TraceLayer::new_for_http());
    // Added before HSTS so it stays inner: short-circuits abusive callers, and
    // its 429 response still flows out through the HSTS layer below.
    if let Some(cfg) = layers.rate_limit.filter(|c| c.enabled) {
        let window = Duration::from_secs(cfg.window_secs);
        let throttle = ratelimit::Throttle {
            requests: RateLimiter::new(cfg.max_requests, window),
            auth_failures: (cfg.max_failed_auth > 0)
                .then(|| RateLimiter::new(cfg.max_failed_auth, window)),
            trusted: layers.trusted_proxies.clone(),
        };
        router = router.layer(axum::middleware::from_fn_with_state(
            throttle,
            ratelimit::enforce,
        ));
    }
    // Refuse misdirected and cross-site writes before anything else runs;
    // inside the security headers, so the refusal carries them too.
    router = router.layer(axum::middleware::from_fn_with_state(
        layers.allowed_hosts,
        request_guard,
    ));
    // Stamp the baseline security headers (and HSTS when we terminate TLS) on
    // every response, including the rate limiter's 429 and every error body.
    let hsts = layers.hsts;
    router = router.layer(axum::middleware::from_fn(
        move |req: Request, next: axum::middleware::Next| async move {
            security_headers(req, next, hsts).await
        },
    ));
    // Outermost: drop forwarding headers from peers that are not trusted
    // proxies, so nothing downstream can be steered by a spoofed value.
    router.layer(axum::middleware::from_fn_with_state(
        layers.trusted_proxies,
        filter_forwarded_headers,
    ))
}

/// Strip `X-Forwarded-*`/`X-Real-IP`/`Forwarded` unless the connection peer is a
/// configured trusted proxy.
///
/// A request that arrives without connection info (nothing wired
/// `into_make_service_with_connect_info`) has no peer to vouch for it, so its
/// forwarding headers are dropped too — failing closed rather than trusting an
/// unknown sender.
async fn filter_forwarded_headers(
    State(trusted): State<Arc<TrustedProxies>>,
    mut req: Request,
    next: axum::middleware::Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let trust = peer.is_some_and(|ip| trusted.trusts(ip));
    if !trust {
        let headers = req.headers_mut();
        for name in proxy::FORWARDED_HEADERS {
            headers.remove(name);
        }
    }
    adopt_authority_as_host(&mut req);
    next.run(req).await
}

/// Refuse a request for a host this server does not serve, and a state
/// change a browser says came from another site.
///
/// **Host.** With `base_url` or `allowed_hosts` set, the `Host` (or, from a
/// trusted proxy, `X-Forwarded-Host`) must name one of them, or the answer is
/// `421 Misdirected Request`. That is what defeats DNS rebinding: a hostile
/// page can point a name it controls at an intranet feed's address and make
/// the browser talk to it, but the browser still sends the hostile name. The
/// health probes are exempt, since orchestrators probe by address.
///
/// **Cross-site writes.** A browser labels every request with
/// `Sec-Fetch-Site`, and a `POST`, `PUT`, `PATCH` or `DELETE` from another
/// site is never something this server's own pages sent. The relist `POST`
/// needs no CORS preflight at all, so without this a hostile page could
/// relist packages on a feed without an API key. Clients that are not
/// browsers do not send the header and are unaffected.
async fn request_guard(
    State(allowed): State<Option<Arc<Vec<String>>>>,
    req: Request,
    next: axum::middleware::Next,
) -> Response {
    use axum::http::Method;
    if let Some(allowed) = &allowed {
        if !req.uri().path().starts_with("/health") {
            let host = forwarded(req.headers(), "x-forwarded-host").or_else(|| {
                req.headers()
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            });
            let host = host.as_deref().map(crate::config::normalize_host);
            if !host.is_some_and(|h| allowed.contains(&h)) {
                return (
                    StatusCode::MISDIRECTED_REQUEST,
                    Json(serde_json::json!({ "error": "this server does not serve that host" })),
                )
                    .into_response();
            }
        }
    }
    let safe = matches!(
        *req.method(),
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    );
    let cross_site = req
        .headers()
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("cross-site"));
    if !safe && cross_site {
        return Error::Forbidden("cross-site requests may not change anything here".into())
            .into_response();
    }
    next.run(req).await
}

/// Make the request's authority visible as a `Host` header.
///
/// HTTP/1.1 carries the target host in `Host`; HTTP/2 and HTTP/3 carry it in
/// the `:authority` pseudo-header instead, which hyper surfaces on the URI and
/// *not* as a header. Everything downstream reads `Host` to work out the
/// server's externally visible name, so without this an HTTP/2 client falls
/// through to the `localhost` default and is handed absolute package URLs
/// pointing at `https://localhost/…` — every restore over HTTP/2 then fails.
///
/// Both forms are equally client-supplied, so this changes what is read, not
/// how far it is trusted.
fn adopt_authority_as_host(req: &mut Request) {
    if req.headers().contains_key(header::HOST) {
        return;
    }
    let Some(authority) = req.uri().authority().map(|a| a.to_string()) else {
        return;
    };
    if let Ok(value) = HeaderValue::from_str(&authority) {
        req.headers_mut().insert(header::HOST, value);
    }
}

/// Add the baseline security response headers, plus a one-year
/// `Strict-Transport-Security` when this process terminates TLS.
///
/// A `Content-Security-Policy` is applied to HTML responses that do not already
/// carry one, so the gallery (which renders package-controlled metadata) cannot
/// execute injected script even if an escaping bug ever slips through. The
/// embedded docs site sets its own, looser policy.
async fn security_headers(req: Request, next: axum::middleware::Next, hsts: bool) -> Response {
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    // Protocol documents embed absolute URLs derived from the request's host and
    // scheme, so a shared cache in front of this server must key on them.
    // Without it, one request's answer — including where clients are told to
    // fetch packages from — can be replayed to everyone else.
    //
    // Appended, not inserted: the CORS layer's `Vary: origin, …` and a gated
    // feed's `Vary: Authorization, …` must survive, or a shared cache replays
    // one origin's `Access-Control-Allow-Origin`, or one key's answer, to
    // another.
    headers.append(
        header::VARY,
        HeaderValue::from_static("Host, X-Forwarded-Host, X-Forwarded-Proto"),
    );
    if hsts {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    let is_html = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("text/html"));
    if is_html && !headers.contains_key(header::CONTENT_SECURITY_POLICY) {
        headers.insert(header::CONTENT_SECURITY_POLICY, CSP_HEADER.clone());
    }
    resp
}

/// The gallery's CSP, pre-parsed once (its inline hashes are computed lazily).
static CSP_HEADER: std::sync::LazyLock<HeaderValue> = std::sync::LazyLock::new(|| {
    HeaderValue::from_str(&ui::CSP).expect("gallery CSP is a valid header value")
});

/// Keep a read-gated feed's answers out of shared caches.
///
/// Whatever a handler says about caching, the answer depends on the
/// credential the request carried, so a cache has to key on it (`Vary`) and
/// must not share it (`private`). Content handlers already mark themselves
/// `private`; this covers every other answer (registration, search, the
/// gallery), which carries no `Cache-Control` of its own and would otherwise
/// be left to a cache's heuristics.
pub(super) async fn gated_feed_headers(request: Request, next: axum::middleware::Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.append(
        header::VARY,
        HeaderValue::from_static("Authorization, X-NuGet-ApiKey"),
    );
    if !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private"));
    }
    response
}

/// Re-render an error from a gallery route as an HTML page.
///
/// These routes are read in a browser, and every one of them could answer with
/// a bare `{"error":"package not found"}` — no chrome, no styling, no way back.
/// That is not a rare path: the detail page links every dependency by id, and on
/// a private feed most dependencies come from nuget.org and are not held here,
/// so the most obvious click on the page produced raw JSON. A mistyped URL, a
/// bad version and a cancelled login prompt did the same.
///
/// Only the gallery is wrapped. The v3 endpoints keep their JSON, because that
/// is what a NuGet client parses.
pub(super) async fn html_errors(
    State(state): State<AppState>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let headers = request.headers().clone();
    let response = next.run(request).await;
    let status = response.status();
    if !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    // Keep whatever headers the error already carried — notably the
    // `WWW-Authenticate` challenge on a 401, without which a browser never
    // prompts — and replace only the body and its content type.
    let (mut parts, _) = response.into_parts();
    // The admin area's challenge names its own realm; its page must ask for
    // the admin key, not the read key.
    let admin_login = parts
        .headers
        .get(header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("Admin"));
    let page = ui::error_page(
        &state.url_builder(&headers),
        status,
        state.feed.admin.is_enabled(),
        admin_login,
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Response::from_parts(parts, axum::body::Body::from(page))
}
