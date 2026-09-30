//! The HTTP layer: application state, routing and request handlers.
//!
//! Each hosted feed gets its own [`AppState`] (sharing the process-wide storage
//! and database) and its own router, mounted under the feed's path prefix. A
//! single unconfigured feed is served at the root, preserving the original
//! single-feed URLs.

mod assets;
mod docs;
mod files;
pub(crate) mod hosted;
mod ui;

pub use hosted::sweep_expired_uploads;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{
    ConnectInfo, DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State,
};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{delete, get, head, post, put};
use axum::Router;
use futures::StreamExt;
use serde::Deserialize;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::auth::{AdminAuth, ApiKeyAuth, ReadAuth};
use crate::config::{
    Config, LicensePolicyConfig, OverwriteMode, RateLimitConfig, ResolvedFeed, RetentionConfig,
};
use crate::database::{Membership, PackageDatabase, SearchRequest, SearchSort};
use crate::error::{Error, Result};
use crate::indexing::{self, IndexOptions};
use crate::mirror::{self, MirrorClient, MirrorOptions};
use crate::nuget::{self, UrlBuilder};
use crate::proxy::{self, TrustedProxies};
use crate::ratelimit::{self, RateLimiter};
use crate::retention::{self, RetentionPolicy};
use crate::storage::{AuxFile, PackageContent, PackageStorage};
use crate::streaming::{self, StreamSummary};
use crate::symbols;
use crate::version::NuGetVersion;

const NUPKG_CONTENT_TYPE: &str = "application/octet-stream";
const MAX_SEARCH_TAKE: i64 = 1000;

/// The resolved, ready-to-serve context for a single feed: its identity, its
/// own put/get/delete authenticators, and its mirror/policy/retention settings.
pub struct FeedContext {
    /// Database key / slug.
    pub name: String,
    /// URL path prefix: `""` for the root feed, else `/{name}`.
    pub prefix: String,
    /// Push (put) authenticator.
    pub auth: ApiKeyAuth,
    /// Download/restore (get) authenticator.
    pub read_auth: ReadAuth,
    /// Moderation/promotion (delete) authenticator.
    pub admin: AdminAuth,
    pub allow_overwrite: OverwriteMode,
    pub hard_delete_enabled: bool,
    /// Incoming versions are pending (withheld) until an admin approves them.
    pub requires_approval: bool,
    /// The feed an admin can promote a version into (the next release ring).
    pub promotes_to: Option<String>,
    /// Upstream mirror client, when mirroring is enabled for this feed.
    pub mirror: Option<MirrorClient>,
    pub license_policy: LicensePolicyConfig,
    pub retention: RetentionConfig,
    /// The feed's cleanup lock and last report, shared with the background
    /// sweep.
    pub cleanup: Arc<retention::RetentionState>,
}

impl FeedContext {
    /// How content addressed by id and version — a payload, its manifest and
    /// icon, its symbols — may be cached.
    ///
    /// `private` when the feed gates reads, so a shared cache or CDN in front
    /// of the server never hands it to someone without the key. `immutable`
    /// only while overwriting is off: with it on, the same URL can serve new
    /// bytes, and a year-long `immutable` would keep the old ones in every
    /// client and proxy that saw them.
    pub(crate) fn content_cache(&self) -> files::CachePolicy {
        files::CachePolicy {
            private: self.read_auth.is_enabled(),
            immutable: self.allow_overwrite == OverwriteMode::Disabled,
        }
    }

    /// Build a feed's serving context. `upload_limit` is the server-wide
    /// `max_package_size_bytes`, which mirrored downloads inherit unless the
    /// feed's mirror set a tighter one of its own.
    fn from_resolved(feed: &ResolvedFeed, upload_limit: Option<u64>) -> Self {
        let mut mirror = MirrorClient::from_config(&feed.mirror);
        if let Some(client) = mirror.as_mut() {
            client.set_default_size_limit(upload_limit);
        }
        Self {
            name: feed.name.clone(),
            prefix: feed.prefix.clone(),
            auth: ApiKeyAuth::new(feed.api_keys.clone()),
            read_auth: ReadAuth::new(feed.read_api_key.clone()),
            admin: AdminAuth::new(feed.admin_api_key.clone()),
            allow_overwrite: feed.allow_overwrite,
            hard_delete_enabled: feed.hard_delete_enabled,
            requires_approval: feed.requires_approval,
            promotes_to: feed.promotes_to.clone(),
            mirror,
            license_policy: feed.license_policy.clone(),
            retention: feed.retention.clone(),
            cleanup: Arc::default(),
        }
    }
}

/// Lightweight, cross-feed metadata so a handler can resolve a promotion, copy
/// or move target.
#[derive(Debug, Clone)]
pub struct FeedMeta {
    pub name: String,
    pub prefix: String,
    pub requires_approval: bool,
    /// The target feed's own license policy, so a promotion into it is held to
    /// the same rule a direct push would be.
    pub license_policy: LicensePolicyConfig,
    /// The target feed's own admin key. Copying or moving a version into a
    /// feed takes that feed's admin credentials, not just this one's.
    pub admin: AdminAuth,
}

impl FeedMeta {
    /// The cross-feed view of one resolved feed.
    pub fn from_resolved(feed: &ResolvedFeed) -> Self {
        Self {
            name: feed.name.clone(),
            prefix: feed.prefix.clone(),
            requires_approval: feed.requires_approval,
            license_policy: feed.license_policy.clone(),
            admin: AdminAuth::new(feed.admin_api_key.clone()),
        }
    }
}

/// Shared application state for one feed, cheaply cloneable (everything behind
/// `Arc`). Storage, database and the feed registry are shared by all feeds.
#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<dyn PackageStorage>,
    pub db: Arc<dyn PackageDatabase>,
    pub config: Arc<Config>,
    pub feed: Arc<FeedContext>,
    feeds: Arc<Vec<FeedMeta>>,
    temp_dir: PathBuf,
}

impl AppState {
    /// Construct state for the implicit single feed served at the root. Used by
    /// tests and simple single-feed deployments.
    pub async fn new(
        storage: Arc<dyn PackageStorage>,
        db: Arc<dyn PackageDatabase>,
        config: Arc<Config>,
    ) -> Result<Self> {
        let feeds = config.resolved_feeds()?;
        // `resolved_feeds` with no `[[feeds]]` yields exactly the root feed.
        let resolved = feeds
            .into_iter()
            .next()
            .expect("at least one resolved feed");
        Self::for_feed(storage, db, config, &resolved, Arc::new(Vec::new())).await
    }

    /// Construct state for one resolved feed.
    pub async fn for_feed(
        storage: Arc<dyn PackageStorage>,
        db: Arc<dyn PackageDatabase>,
        config: Arc<Config>,
        resolved: &ResolvedFeed,
        feeds: Arc<Vec<FeedMeta>>,
    ) -> Result<Self> {
        // Keep temp uploads on the same filesystem as storage so the final move
        // is an atomic rename rather than a multi-gigabyte copy.
        let temp_dir = config.storage_path().join(".uploads");
        tokio::fs::create_dir_all(&temp_dir).await?;
        sweep_stale_uploads(&temp_dir).await;
        let feed = Arc::new(FeedContext::from_resolved(
            resolved,
            config.max_package_size_bytes,
        ));
        Ok(Self {
            storage,
            db,
            config,
            feed,
            feeds,
            temp_dir,
        })
    }

    /// The feed name used as the database scope for every package query.
    fn feed(&self) -> &str {
        &self.feed.name
    }

    /// Resolve the externally visible base URL for this request, including the
    /// feed's path prefix so generated resource URLs stay within the feed.
    fn url_builder(&self, headers: &HeaderMap) -> UrlBuilder {
        let root = if let Some(base) = &self.config.base_url {
            base.clone()
        } else {
            // Only a scheme a client can actually fetch from. Anything else
            // would be pasted into every absolute URL handed out.
            let scheme = forwarded(headers, "x-forwarded-proto")
                .map(|s| s.to_ascii_lowercase())
                .filter(|s| s == "http" || s == "https")
                .unwrap_or_else(|| self.config.scheme().to_string());
            let host = forwarded(headers, "x-forwarded-host")
                .or_else(|| {
                    headers
                        .get(header::HOST)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "localhost".into());
            format!("{scheme}://{host}")
        };
        UrlBuilder::with_prefix(root, &self.feed.prefix)
    }

    /// Refuse an upload of `incoming` bytes (when the client declared a size)
    /// that would leave the storage volume with less than
    /// `min_free_disk_bytes` free.
    ///
    /// A full disk does not fail cleanly: the database, the temp file and every
    /// other writer on the volume run out together, mid-write. Saying no up
    /// front costs one `statvfs`. With no declared size (a chunked body) only
    /// the reserve itself is checked; the size limit and the idle timeout bound
    /// the rest. When free space cannot be measured the upload is let through,
    /// since a guard that fails closed would take the feed down with it.
    fn ensure_disk_space(&self, incoming: Option<u64>) -> Result<()> {
        let reserve = self.config.min_free_disk_bytes;
        if reserve == 0 {
            return Ok(());
        }
        let available = match fs4::available_space(&self.temp_dir) {
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
                 (the upload plus the configured reserve)"
            )));
        }
        Ok(())
    }

    /// Create a fresh temp file for an incoming upload.
    async fn create_temp(&self) -> Result<(PathBuf, tokio::fs::File)> {
        let path = self.temp_dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let file = tokio::fs::File::create(&path).await?;
        Ok((path, file))
    }

    /// Reject the request unless valid read credentials are presented (a no-op
    /// when the feed allows open reads).
    fn require_read(&self, headers: &HeaderMap) -> Result<()> {
        if self.feed.read_auth.check_headers(headers) {
            Ok(())
        } else {
            Err(Error::Unauthorized)
        }
    }

    /// Best-effort read-through mirror: when the feed has an upstream and a
    /// lookup missed, fetch the package's versions and index them. Errors are
    /// logged, never surfaced — a mirror outage degrades to a normal miss.
    async fn mirror_if_needed(&self, id: &str) {
        let Some(client) = &self.feed.mirror else {
            return;
        };
        let options = MirrorOptions {
            requires_approval: self.feed.requires_approval,
            license_policy: self.feed.license_policy.clone(),
        };
        if let Err(e) = mirror::ensure_package(
            client,
            self.storage.as_ref(),
            self.db.as_ref(),
            self.feed(),
            &self.temp_dir,
            id,
            &options,
        )
        .await
        {
            tracing::warn!(feed = %self.feed(), id, error = %e, "mirror lookup failed");
        }
    }
}

/// Build the complete application from one or more feed states.
///
/// A single root feed is served directly; multiple feeds are each mounted under
/// their `/{name}` prefix with a feed index at the root.
pub fn build_app(states: Vec<AppState>) -> Router {
    let layers = GlobalLayers::from_states(&states);
    let mut top = health_routes(&states).merge(assets::routes());

    if states.len() == 1 && states[0].feed.prefix.is_empty() {
        top = top.merge(feed_routes(states.into_iter().next().expect("one state")));
    } else {
        let index: Vec<(String, String)> = states
            .iter()
            .map(|s| (s.feed.name.clone(), s.feed.prefix.clone()))
            .collect();
        let html = ui::feeds_index_page(&index);
        top = top.route(
            "/",
            get(move || {
                let html = html.clone();
                async move { Html(html) }
            }),
        );
        for s in states {
            let prefix = s.feed.prefix.clone();
            // Nested under `/{name}`: a feed's own routes (including its `/`
            // gallery) live at `/{name}/...`. The feed index links use the
            // trailing-slash form accordingly.
            top = top.nest(&prefix, feed_routes(s));
        }
    }

    apply_global_layers(top, layers)
}

/// Build a single feed's complete application (with global middleware). Used by
/// tests and single-feed deployments.
pub fn router(state: AppState) -> Router {
    let states = std::slice::from_ref(&state);
    let layers = GlobalLayers::from_states(states);
    let app = health_routes(states)
        .merge(assets::routes())
        .merge(feed_routes(state));
    apply_global_layers(app, layers)
}

/// The liveness/readiness routes, which live outside any feed.
///
/// They carry the first feed's state purely for its database handle; the probe
/// is process-wide, not per-feed.
fn health_routes(states: &[AppState]) -> Router {
    let live: Router = Router::new().route("/health/live", get(health_live));
    match states.first() {
        Some(state) => live.merge(
            Router::new()
                .route("/health", get(health))
                .route("/health/ready", get(health))
                .with_state(state.clone()),
        ),
        // No feed configured: there is nothing to probe but the process itself.
        None => live.route("/health", get(health_live)),
    }
}

/// The process-wide middleware settings, derived once from the served feeds.
struct GlobalLayers {
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

/// Delete upload temp files left over from a previous run.
///
/// Every error path removes its own temp file, but nothing can run when the
/// process does not get to return: a SIGKILL, an OOM, or the forced close after
/// the shutdown grace period while an upload is still streaming. Each leak is as
/// large as the package that was in flight, they accumulate across restarts, and
/// they sit on the same filesystem as the package store — so left alone they
/// eventually fill the volume holding the packages.
///
/// Startup is the safe moment: none of this process's uploads can be in flight
/// yet. Best-effort throughout — a directory we cannot read is not a reason to
/// refuse to start.
async fn sweep_stale_uploads(temp_dir: &std::path::Path) {
    let Ok(mut entries) = tokio::fs::read_dir(temp_dir).await else {
        return;
    };
    let (mut removed, mut bytes) = (0u64, 0u64);
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("tmp") {
            continue;
        }
        let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
        if tokio::fs::remove_file(&path).await.is_ok() {
            removed += 1;
            bytes = bytes.saturating_add(size);
        }
    }
    if removed > 0 {
        tracing::info!(
            files = removed,
            bytes,
            "removed upload temp files left by a previous run"
        );
    }
}

impl GlobalLayers {
    fn from_states(states: &[AppState]) -> Self {
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
fn apply_global_layers(router: Router, layers: GlobalLayers) -> Router {
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

/// Build one feed's routes (relative paths, no global middleware), ready to be
/// nested under the feed's prefix or merged at the root.
fn feed_routes(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/v3/index.json", get(service_index))
        .route("/api/v2/package", put(push_package))
        // The NuGet client appends a trailing slash to the publish endpoint
        // (`PackageUpdateResource` calls `EnsureTrailingSlash` on the push
        // source), so it `PUT`s `/api/v2/package/`. Axum treats that as a
        // distinct route from `/api/v2/package` and offers no automatic
        // redirect, so register the trailing-slash variant explicitly.
        .route("/api/v2/package/", put(push_package))
        .route(
            "/api/v2/package/{id}/{version}",
            delete(delete_package).post(relist_package),
        )
        .route("/v3/package/{id}/index.json", get(package_versions))
        .route(
            "/v3/package/{id}/{version}/{filename}",
            get(download_package),
        )
        .route("/v3/registration/{id}/index.json", get(registration_index))
        .route(
            "/v3/registration/{id}/page/{lower}/{upper}",
            get(registration_page),
        )
        .route("/v3/registration/{id}/{version}", get(registration_leaf))
        .route("/v3/search", get(search))
        .route("/v3/autocomplete", get(autocomplete))
        // Files attached to versions (the handlers answer 404 while the
        // feature is off). Not NuGet protocol; outside the gallery, so a
        // script gets JSON errors and the files work with the UI disabled.
        .route("/files/{id}/{version}/{name}", get(hosted::download))
        .route(
            "/api/v2/files/{id}/{version}/{name}",
            put(hosted::put).delete(hosted::delete),
        )
        .route(
            "/api/v2/uploads",
            post(hosted::tus_create).options(hosted::tus_options),
        )
        .route(
            "/api/v2/uploads/{upload}",
            head(hosted::tus_head)
                .patch(hosted::tus_patch)
                .delete(hosted::tus_delete),
        );

    // The SemVer2 hive mirrors the routes above. A client picks a hive from the
    // service index, so each has to be reachable at its own path and to keep
    // its self-referencing URLs inside itself.
    let sv2 = UrlBuilder::semver2_hive_segment();
    router = router
        .route(
            &format!("/v3/{sv2}/{{id}}/index.json"),
            get(registration_index_semver2),
        )
        .route(
            &format!("/v3/{sv2}/{{id}}/page/{{lower}}/{{upper}}"),
            get(registration_page_semver2),
        )
        .route(
            &format!("/v3/{sv2}/{{id}}/{{version}}"),
            get(registration_leaf_semver2),
        );

    // Symbol server: push `.snupkg` and serve PDBs over the SSQP path.
    if state.config.enable_symbol_server {
        router = router
            .route("/api/v2/symbol", put(push_symbol_package))
            // Same trailing-slash handling as the package publish endpoint.
            .route("/api/v2/symbol/", put(push_symbol_package))
            .route(
                "/download/symbols/{file}/{key}/{file2}",
                get(download_symbol),
            );
    }

    // Human-facing gallery. When disabled, `/` falls back to a minimal page.
    //
    // Built as its own router so `html_errors` wraps only the pages a person
    // reads. The v3 endpoints must keep answering errors as JSON — that is what
    // a NuGet client parses.
    if state.config.enable_web_ui {
        let mut ui = Router::new()
            .route("/", get(gallery))
            .route("/packages", get(gallery))
            .route("/packages/{id}", get(package_detail))
            .route("/packages/{id}/{version}", get(package_detail_version))
            .route("/packages/{id}/{version}/icon", get(package_icon))
            .route("/stats", get(stats_page))
            .route("/tags", get(tags_page))
            .route("/settings", get(settings_page))
            // Embedded, offline documentation site. `/docs` redirects to
            // `/docs/` so the site's relative links resolve.
            .route("/docs", get(docs::docs_root))
            .route("/docs/", get(docs::docs_index))
            .route("/docs/{*path}", get(docs::serve_docs));

        // Admin area (disable/enable/delete/approve/promote versions), behind
        // HTTP Basic auth. Only mounted when an admin key is configured.
        //
        // Authentication is a `route_layer` over the whole group rather than
        // the first line of each handler, so a handler added here later cannot
        // forget it. State changes additionally check a CSRF token in the
        // handler, since only the handler reads the form body.
        if state.feed.admin.is_enabled() {
            let admin = Router::new()
                .route("/admin", get(admin_dashboard))
                .route(
                    "/admin/packages/{id}",
                    get(admin_package).merge(admin_post(admin_bulk)),
                )
                .route(
                    "/admin/packages/{id}/{version}/disable",
                    admin_post(admin_disable),
                )
                .route(
                    "/admin/packages/{id}/{version}/enable",
                    admin_post(admin_enable),
                )
                .route(
                    "/admin/packages/{id}/{version}/delete",
                    admin_post(admin_delete),
                )
                .route(
                    "/admin/packages/{id}/{version}/approve",
                    admin_post(admin_approve),
                )
                .route(
                    "/admin/packages/{id}/{version}/promote",
                    admin_post(admin_promote),
                )
                .route("/admin/packages/{id}/{version}/pin", admin_post(admin_pin))
                .route(
                    "/admin/packages/{id}/{version}/unpin",
                    admin_post(admin_unpin),
                )
                .route(
                    "/admin/packages/{id}/{version}/files/{name}/delete",
                    admin_post(admin_file_delete),
                )
                .route("/admin/retention", get(admin_retention))
                .route("/admin/retention/run", admin_post(admin_retention_run))
                .route_layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    admin_gate,
                ));
            ui = ui.merge(admin);
        }
        router = router.merge(ui.layer(axum::middleware::from_fn_with_state(
            state.clone(),
            html_errors,
        )));
    } else {
        router = router.route("/", get(index_page));
    }

    if state.feed.read_auth.is_enabled() {
        router = router.layer(axum::middleware::from_fn(gated_feed_headers));
    }
    router.with_state(state)
}

/// Keep a read-gated feed's answers out of shared caches.
///
/// Whatever a handler says about caching, the answer depends on the
/// credential the request carried, so a cache has to key on it (`Vary`) and
/// must not share it (`private`). Content handlers already mark themselves
/// `private`; this covers every other answer (registration, search, the
/// gallery), which carries no `Cache-Control` of its own and would otherwise
/// be left to a cache's heuristics.
async fn gated_feed_headers(request: Request, next: axum::middleware::Next) -> Response {
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
async fn html_errors(
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
    let page = ui::error_page(
        &state.url_builder(&headers),
        status,
        state.feed.admin.is_enabled(),
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    parts.headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    Response::from_parts(parts, axum::body::Body::from(page))
}

/// An admin form POST, capped at a size a form can plausibly be.
///
/// The global body limit is disabled so package uploads can stream to disk, but
/// these handlers read the body into memory to check the CSRF field. Without a
/// cap of their own, an authenticated admin POST with a multi-gigabyte body
/// would be buffered in full.
fn admin_post<H, T>(handler: H) -> axum::routing::MethodRouter<AppState>
where
    H: axum::handler::Handler<T, AppState>,
    T: 'static,
{
    const MAX_ADMIN_FORM_BYTES: usize = 64 * 1024;
    post(handler).layer(DefaultBodyLimit::max(MAX_ADMIN_FORM_BYTES))
}

// ---------------------------------------------------------------------------
// Informational endpoints
// ---------------------------------------------------------------------------

/// Readiness: the process is up **and** its database answers.
///
/// Returns the historical plain `OK` body on success so existing probes keep
/// working, and `503` with a short reason when the store is unreachable — which
/// is the case an orchestrator has to be able to act on. `/health/live` is the
/// dependency-free liveness counterpart.
async fn health(State(state): State<AppState>) -> Response {
    match state.db.ping().await {
        Ok(()) => (StatusCode::OK, "OK").into_response(),
        Err(e) => {
            tracing::error!(error = %e, "health probe failed");
            (StatusCode::SERVICE_UNAVAILABLE, "database unavailable").into_response()
        }
    }
}

/// Liveness: this process is running. Touches nothing else, so a restart loop
/// caused by a slow dependency cannot be triggered from here.
async fn health_live() -> &'static str {
    "OK"
}

async fn index_page(State(state): State<AppState>, headers: HeaderMap) -> Html<String> {
    let urls = state.url_builder(&headers);
    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>YANuget</title></head><body>\
         <h1>YANuget</h1>\
         <p>A fast, streaming NuGet v3 server written in Rust.</p>\
         <p>Service index: <a href=\"{idx}\">{idx}</a></p>\
         <p>Add this feed with:</p>\
         <pre>dotnet nuget add source {idx} -n yanuget</pre>\
         </body></html>",
        idx = ui::escape_html(&urls.service_index())
    ))
}

async fn service_index(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let urls = state.url_builder(&headers);
    Json(nuget::service_index(&urls, state.config.enable_web_ui))
}

// ---------------------------------------------------------------------------
// Push / delete / relist
// ---------------------------------------------------------------------------

async fn push_package(State(state): State<AppState>, request: Request) -> Result<Response> {
    let headers = request.headers().clone();
    if !state.feed.auth.check_headers(&headers) {
        return Err(Error::Unauthorized);
    }

    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.starts_with("multipart/"))
        .unwrap_or(false);

    state.ensure_disk_space(content_length(&headers))?;
    let (temp_path, mut file) = state.create_temp().await?;
    let limit = state.config.max_package_size_bytes;

    // Stream the body to disk. On any failure, drop the temp file.
    let summary = match write_upload(request, &mut file, is_multipart, limit, &state).await {
        Ok(summary) => summary,
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(e);
        }
    };
    // Flush the OS page cache to stable storage before the payload is renamed
    // into the store. The database row that follows says the package exists; if
    // a crash lands between the rename and the kernel's own writeback, that row
    // would point at a truncated or empty file.
    if let Err(e) = file.sync_all().await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(Error::Io(e));
    }
    drop(file);

    let options = IndexOptions {
        overwrite: state.feed.allow_overwrite,
        pending: state.feed.requires_approval,
        license_policy: state.feed.license_policy.clone(),
        // A push is self-describing: the manifest defines the identity.
        expect: None,
    };
    let result = indexing::index_package(
        state.storage.as_ref(),
        state.db.as_ref(),
        state.feed(),
        temp_path,
        summary,
        &options,
    )
    .await?;

    // Optionally prune older versions of this id in this feed (best-effort:
    // never fail the push because of retention).
    if state.feed.retention.enabled && state.feed.retention.prune_on_push {
        let policy = RetentionPolicy::from(&state.feed.retention);
        if let Err(e) = retention::prune_package(
            state.storage.as_ref(),
            state.db.as_ref(),
            state.feed(),
            &result.id,
            &policy,
        )
        .await
        {
            tracing::error!(id = %result.id, error = %e, "prune-on-push failed");
        }
    }

    Ok(StatusCode::CREATED.into_response())
}

/// Write the upload (raw body or the first multipart file field) to `file`.
async fn write_upload(
    request: Request,
    file: &mut tokio::fs::File,
    is_multipart: bool,
    limit: Option<u64>,
    state: &AppState,
) -> Result<StreamSummary> {
    if is_multipart {
        let mut multipart = Multipart::from_request(request, state)
            .await
            .map_err(|e| Error::BadRequest(format!("invalid multipart body: {e}")))?;
        // The package is the first field; NuGet sends exactly one file part.
        let field = multipart
            .next_field()
            .await
            .map_err(|e| Error::BadRequest(format!("invalid multipart field: {e}")))?
            .ok_or_else(|| Error::BadRequest("multipart body contained no file".into()))?;
        let stream = Box::pin(field.map(|r| r.map_err(to_io_err)));
        streaming::stream_to_writer_limited(stream, file, limit, state.config.upload_idle_timeout())
            .await
            .map_err(map_upload_err)
    } else {
        let stream = Box::pin(
            request
                .into_body()
                .into_data_stream()
                .map(|r| r.map_err(to_io_err)),
        );
        streaming::stream_to_writer_limited(stream, file, limit, state.config.upload_idle_timeout())
            .await
            .map_err(map_upload_err)
    }
}

/// The declared size of a request body, when the client sent one.
fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

async fn delete_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<StatusCode> {
    check_id(&id)?;
    if !state.feed.auth.check_headers(&headers) {
        return Err(Error::Unauthorized);
    }
    let version = parse_version(&version)?;

    if state.feed.hard_delete_enabled {
        // Hard delete removes this feed's membership (and, when it was the last
        // feed, the payload, sidecars and indexed symbols).
        let removed = retention::purge_version(
            state.storage.as_ref(),
            state.db.as_ref(),
            state.feed(),
            &id,
            &version,
        )
        .await?;
        if removed {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err(Error::PackageNotFound)
        }
    } else {
        // The NuGet client's "delete" means "unlist".
        let updated = state
            .db
            .set_listed(state.feed(), &id, &version, false)
            .await?;
        if updated {
            Ok(StatusCode::NO_CONTENT)
        } else {
            Err(Error::PackageNotFound)
        }
    }
}

async fn relist_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<StatusCode> {
    check_id(&id)?;
    if !state.feed.auth.check_headers(&headers) {
        return Err(Error::Unauthorized);
    }
    let version = parse_version(&version)?;
    if state
        .db
        .set_listed(state.feed(), &id, &version, true)
        .await?
    {
        Ok(StatusCode::OK)
    } else {
        Err(Error::PackageNotFound)
    }
}

// ---------------------------------------------------------------------------
// Flat container (package content)
// ---------------------------------------------------------------------------

async fn package_versions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    check_id(&id)?;
    state.require_read(&headers)?;
    // The flat container is how a client resolves a version it is *about to
    // restore*, so it must include unlisted versions. Unlisting means "hide
    // from discovery, stay restorable" — that is the whole distinction from
    // deleting — and omitting them here makes a project pinned to an unlisted
    // version fail with NU1101, even though the payload is still served.
    //
    // Admin-disabled and still-pending versions are a different matter and
    // remain excluded: those are withheld from clients outright.
    const INCLUDE_UNLISTED: bool = true;
    let mut packages = state
        .db
        .find_versions(state.feed(), &id, INCLUDE_UNLISTED)
        .await?;
    if packages.is_empty() {
        state.mirror_if_needed(&id).await;
        packages = state
            .db
            .find_versions(state.feed(), &id, INCLUDE_UNLISTED)
            .await?;
    }
    if packages.is_empty() {
        return Err(Error::PackageNotFound);
    }
    let versions: Vec<String> = packages
        .iter()
        .map(|p| p.normalized_version().to_lowercase())
        .collect();
    Ok(Json(nuget::flat_container_index(&versions)))
}

async fn download_package(
    State(state): State<AppState>,
    method: axum::http::Method,
    headers: HeaderMap,
    Path((id, version, filename)): Path<(String, String, String)>,
) -> Result<Response> {
    check_id(&id)?;
    state.require_read(&headers)?;
    let version = parse_version(&version)?;
    let normalized = version.normalized();

    // Admin-disabled / pending versions are withheld from clients entirely.
    // On a miss, attempt a read-through mirror before giving up.
    if !state.db.is_servable(state.feed(), &id, &version).await? {
        state.mirror_if_needed(&id).await;
        if !state.db.is_servable(state.feed(), &id, &version).await? {
            return Err(Error::PackageNotFound);
        }
    }

    // The flat container exposes both the `.nupkg` and the bare `.nuspec` under
    // the same path prefix; dispatch on the requested file's extension.
    let is_nuspec = filename.to_lowercase().ends_with(".nuspec");
    let content = if is_nuspec {
        state
            .storage
            .aux_content(&id, &normalized, AuxFile::Nuspec)
            .await?
    } else {
        state.storage.get_package(&id, &normalized).await?
    };
    let PackageContent::LocalPath(path) = content;

    // The stored SHA-512 is a content hash of the package, which makes it a
    // correct strong validator: a client that already holds this package gets
    // a 304 instead of re-downloading gigabytes. The manifest is extracted
    // from exactly those bytes, so a tag derived from the same hash validates
    // it too. The publish time is the matching `Last-Modified`.
    //
    // The row is read on both sides of opening the file. An overwrite renames
    // new bytes into place, so a tag read only before the open could be served
    // with bytes opened after it — and an `If-Range` would then splice old and
    // new. If anything moved in between, the response goes out without
    // validators rather than with wrong ones.
    let before = state
        .db
        .find(state.feed(), &id, &version)
        .await
        .ok()
        .flatten();
    let file = files::open(&path).await?;
    let after = state
        .db
        .find(state.feed(), &id, &version)
        .await
        .ok()
        .flatten();
    let size = file.metadata().await?.len();
    let package = match (before, after) {
        (Some(b), Some(a))
            if b.package_hash == a.package_hash
                && b.published == a.published
                && (is_nuspec || a.package_size == size) =>
        {
            Some(a)
        }
        _ => None,
    };
    let etag = package.as_ref().map(|p| {
        if is_nuspec {
            format!("{}-nuspec", p.package_hash)
        } else {
            p.package_hash.clone()
        }
    });
    let meta = files::FileMeta {
        etag: etag.as_deref(),
        last_modified: package.as_ref().map(|p| p.published),
        cache: state.feed.content_cache(),
        ..Default::default()
    };

    if is_nuspec {
        // These bytes are whatever the pusher put in the manifest, stored
        // verbatim — including anything before or around `<metadata>`, which the
        // parser ignores. Served as bare `application/xml` from this origin, a
        // manifest beginning with an `<?xml-stylesheet?>` PI can make a browser
        // run script here: the same-origin script execution the gallery's
        // hash-pinned CSP exists to prevent, and from which a logged-in
        // operator's admin session is reachable. The icon endpoint already
        // sniffs and restricts its bytes for exactly this reason; the manifest
        // got none of it.
        //
        // So: deny everything via CSP, refuse content sniffing, and mark it a
        // download. Clients fetch this with an HTTP library, which ignores all
        // three; only a browser is affected, and a browser has no business
        // rendering it.
        let mut response = files::serve_open_file(
            file,
            &headers,
            "application/xml",
            files::FileMeta {
                download_name: Some("manifest.nuspec"),
                ..meta
            },
        )
        .await?;
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'"),
        );
        return Ok(response);
    }

    let name = format!("{}.{}.nupkg", id.to_lowercase(), normalized.to_lowercase());
    let response = files::serve_open_file(
        file,
        &headers,
        NUPKG_CONTENT_TYPE,
        files::FileMeta {
            download_name: Some(&name),
            ..meta
        },
    )
    .await?;

    // Count the download once per transfer rather than once per ranged request
    // of it, and only when bytes actually go out: a 304 is a client confirming
    // it already has them. The write runs off the request path, so a busy
    // database never holds up the file.
    if files::counts_as_download(&method, &headers) && files::sends_content(&response) {
        let (db, feed) = (state.db.clone(), state.feed.name.clone());
        tokio::spawn(async move {
            if let Err(e) = db.increment_downloads(&feed, &id, &version).await {
                tracing::debug!(%feed, %id, error = %e, "download not counted");
            }
        });
    }
    Ok(response)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// NuGet exposes two registration hives and a client picks one from the service
/// index. The SemVer1 hive must omit versions such a client cannot parse —
/// dotted pre-release labels and build metadata — so each handler pair differs
/// only in which hive it filters and links to.
async fn registration_index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    registration_index_for(&state, &headers, &id, false).await
}

async fn registration_index_semver2(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    registration_index_for(&state, &headers, &id, true).await
}

async fn registration_index_for(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    semver2: bool,
) -> Result<Json<serde_json::Value>> {
    check_id(id)?;
    state.require_read(headers)?;
    // Registration includes unlisted versions (flagged listed=false).
    let mut packages = state.db.find_versions(state.feed(), id, true).await?;
    if packages.is_empty() {
        state.mirror_if_needed(id).await;
        packages = state.db.find_versions(state.feed(), id, true).await?;
    }
    if packages.is_empty() {
        return Err(Error::PackageNotFound);
    }
    let packages = filter_hive(packages, semver2);
    if packages.is_empty() {
        return Err(Error::PackageNotFound);
    }
    let urls = state.url_builder(headers).with_hive(semver2);
    Ok(Json(nuget::registration_index(&urls, id, &packages)))
}

async fn registration_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, lower, upper)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>> {
    registration_page_for(&state, &headers, &id, &lower, &upper, false).await
}

async fn registration_page_semver2(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, lower, upper)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>> {
    registration_page_for(&state, &headers, &id, &lower, &upper, true).await
}

async fn registration_page_for(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    lower: &str,
    upper: &str,
    semver2: bool,
) -> Result<Json<serde_json::Value>> {
    check_id(id)?;
    state.require_read(headers)?;
    let upper = upper.strip_suffix(".json").unwrap_or(upper);
    let lower = parse_version(lower)?;
    let upper = parse_version(upper)?;
    // Registration includes unlisted versions; restrict to the page's range.
    let packages = state.db.find_versions(state.feed(), id, true).await?;
    let mut packages = filter_hive(packages, semver2);
    packages.retain(|p| p.version >= lower && p.version <= upper);
    if packages.is_empty() {
        return Err(Error::PackageNotFound);
    }
    let urls = state.url_builder(headers).with_hive(semver2);
    Ok(Json(nuget::registration_page(&urls, id, &packages)))
}

async fn registration_leaf(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    registration_leaf_for(&state, &headers, &id, &version, false).await
}

async fn registration_leaf_semver2(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    registration_leaf_for(&state, &headers, &id, &version, true).await
}

async fn registration_leaf_for(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    version: &str,
    semver2: bool,
) -> Result<Json<serde_json::Value>> {
    check_id(id)?;
    state.require_read(headers)?;
    let version = version.strip_suffix(".json").unwrap_or(version);
    let version = parse_version(version)?;
    let package = state
        .db
        .find(state.feed(), id, &version)
        .await?
        .ok_or(Error::PackageNotFound)?;
    // A SemVer2 version has no leaf in the SemVer1 hive at all.
    if !semver2 && package.is_semver2 {
        return Err(Error::PackageNotFound);
    }
    let urls = state.url_builder(headers).with_hive(semver2);
    Ok(Json(nuget::registration_leaf(&urls, id, &package)))
}

/// Restrict a version list to what the requested hive may expose.
fn filter_hive(
    packages: Vec<crate::models::Package>,
    semver2: bool,
) -> Vec<crate::models::Package> {
    if semver2 {
        packages
    } else {
        packages.into_iter().filter(|p| !p.is_semver2).collect()
    }
}

// ---------------------------------------------------------------------------
// Search / autocomplete
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    skip: Option<i64>,
    #[serde(default)]
    take: Option<i64>,
    #[serde(default)]
    prerelease: Option<bool>,
    #[serde(rename = "semVerLevel", default)]
    semver_level: Option<String>,
    #[serde(rename = "packageType", default)]
    package_type: Option<String>,
}

async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Result<Json<serde_json::Value>> {
    state.require_read(&headers)?;
    let request = SearchRequest {
        query: params.q.unwrap_or_default(),
        skip: params.skip.unwrap_or(0).max(0),
        take: params.take.unwrap_or(20).clamp(0, MAX_SEARCH_TAKE),
        include_prerelease: params.prerelease.unwrap_or(false),
        include_semver2: is_semver2_level(params.semver_level.as_deref()),
        package_type: params.package_type.filter(|s| !s.is_empty()),
        sort: Default::default(),
        tag: None,
    };
    let page = state.db.search(state.feed(), &request).await?;
    // Link results into the hive matching the caller's semVerLevel, so a client
    // that asked for SemVer1 is not sent to registration documents holding
    // versions it cannot parse.
    let urls = state
        .url_builder(&headers)
        .with_hive(request.include_semver2);
    Ok(Json(nuget::search_response(&urls, &page)))
}

#[derive(Debug, Deserialize)]
struct AutocompleteParams {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    skip: Option<i64>,
    #[serde(default)]
    take: Option<i64>,
    #[serde(default)]
    prerelease: Option<bool>,
    #[serde(rename = "semVerLevel", default)]
    semver_level: Option<String>,
}

async fn autocomplete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<AutocompleteParams>,
) -> Result<Json<serde_json::Value>> {
    state.require_read(&headers)?;
    let include_semver2 = is_semver2_level(params.semver_level.as_deref());
    let include_prerelease = params.prerelease.unwrap_or(true);

    // `id` present => enumerate that package's versions.
    if let Some(id) = params.id.filter(|s| !s.is_empty()) {
        // An id no package can have has no versions.
        let packages = if check_id(&id).is_ok() {
            state.db.find_versions(state.feed(), &id, false).await?
        } else {
            Vec::new()
        };
        let versions: Vec<String> = packages
            .iter()
            .filter(|p| include_prerelease || !p.is_prerelease())
            .filter(|p| include_semver2 || !p.is_semver2)
            .map(|p| p.normalized_version())
            .collect();
        return Ok(Json(nuget::enumerate_versions_response(&versions)));
    }

    let take = params.take.unwrap_or(20).clamp(0, MAX_SEARCH_TAKE);
    let skip = params.skip.unwrap_or(0).max(0);
    let (ids, total) = state
        .db
        .autocomplete(
            state.feed(),
            &params.q.unwrap_or_default(),
            include_prerelease,
            include_semver2,
            skip,
            take,
        )
        .await?;
    Ok(Json(nuget::autocomplete_response(&ids, total)))
}

// ---------------------------------------------------------------------------
// Symbol server
// ---------------------------------------------------------------------------

async fn push_symbol_package(State(state): State<AppState>, request: Request) -> Result<Response> {
    let headers = request.headers().clone();
    if !state.feed.auth.check_headers(&headers) {
        return Err(Error::Unauthorized);
    }

    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.starts_with("multipart/"))
        .unwrap_or(false);

    state.ensure_disk_space(content_length(&headers))?;
    let (temp_path, mut file) = state.create_temp().await?;
    let limit = state.config.max_package_size_bytes;

    // The size and hash are computed while the bytes stream past, so recording
    // them costs nothing. Dropping them left the one number in the log that says
    // how much a symbol push actually cost, and any later question about which
    // bytes were stored, unanswerable.
    let summary = match write_upload(request, &mut file, is_multipart, limit, &state).await {
        Ok(summary) => summary,
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(e);
        }
    };
    // Flush the OS page cache to stable storage before the payload is renamed
    // into the store. The database row that follows says the package exists; if
    // a crash lands between the rename and the kernel's own writeback, that row
    // would point at a truncated or empty file.
    if let Err(e) = file.sync_all().await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(Error::Io(e));
    }
    drop(file);

    let result = symbols::index_symbol_package(
        state.storage.as_ref(),
        state.db.as_ref(),
        state.feed(),
        temp_path,
    )
    .await?;
    tracing::info!(
        id = %result.id,
        version = %result.version.normalized(),
        indexed = result.indexed,
        skipped = result.skipped,
        bytes = summary.size,
        sha512 = %summary.sha512_base64,
        "indexed symbol package",
    );
    Ok(StatusCode::CREATED.into_response())
}

async fn download_symbol(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((file, key, file2)): Path<(String, String, String)>,
) -> Result<Response> {
    // Symbols are package content: a PDB carries source paths, local file
    // layout and (with embedded sources) code, so a feed that gates downloads
    // must gate these too.
    state.require_read(&headers)?;
    // The SSQP path repeats the file name; both segments must agree.
    if !file.eq_ignore_ascii_case(&file2) {
        return Err(Error::PackageNotFound);
    }

    // The symbol *store* is global — a PDB is addressed by its own signature,
    // not by feed — so serving straight from it would hand a caller symbols for
    // a package that only some other feed contains. Resolve the owning package
    // first and require that this feed can actually serve it, which also makes
    // an admin-disabled or still-pending version withhold its symbols.
    let owner = state
        .db
        .find_symbol(&key, &file)
        .await?
        .ok_or(Error::PackageNotFound)?;
    let owner_version = parse_version(&owner.normalized_version)?;
    if !state
        .db
        .is_servable(state.feed(), &owner.lower_id, &owner_version)
        .await?
    {
        return Err(Error::PackageNotFound);
    }

    let content = state.storage.get_symbol(&key, &file).await?;
    match content {
        // A symbol is addressed by its own content signature, so the key itself
        // is a sound validator.
        PackageContent::LocalPath(path) => {
            files::serve_local_file(
                path,
                &headers,
                NUPKG_CONTENT_TYPE,
                files::FileMeta {
                    etag: Some(&key),
                    cache: state.feed.content_cache(),
                    ..Default::default()
                },
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Web gallery (HTML)
// ---------------------------------------------------------------------------

/// The gallery's query string: a search, plus the pager's `page`.
///
/// Every value is taken as text and parsed with [`lenient`]. These are
/// addresses people bookmark and edit, so an empty or mistyped `?take=` shows
/// the default rather than a 400, which is what a typed field answered.
#[derive(Debug, Deserialize)]
struct GalleryParams {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    skip: Option<String>,
    #[serde(default)]
    take: Option<String>,
    /// A 1-based page number, from the pager's "go to page" form.
    #[serde(default)]
    page: Option<String>,
    #[serde(default)]
    prerelease: Option<String>,
    #[serde(rename = "packageType", default)]
    package_type: Option<String>,
    /// `downloads` (the default), `name` or `updated`.
    #[serde(default)]
    sort: Option<String>,
    /// Only packages with this tag.
    #[serde(default)]
    tag: Option<String>,
}

/// A `?tag=` value worth querying for: trimmed and lower-cased, or `None` for
/// one no stored tag could equal (empty, too long, or with whitespace or
/// control characters in it — tags are whitespace-separated when pushed).
fn gallery_tag(raw: Option<&str>) -> Option<String> {
    let tag = raw?.trim().to_lowercase();
    let plausible = !tag.is_empty()
        && tag.chars().count() <= crate::nuspec::MAX_TAG_CHARS
        && !tag.chars().any(|c| c.is_whitespace() || c.is_control());
    plausible.then_some(tag)
}

/// Parse an optional query value, treating an empty or malformed one as absent.
fn lenient<T: std::str::FromStr>(value: Option<&str>) -> Option<T> {
    value.and_then(|v| v.trim().parse().ok())
}

async fn gallery(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<GalleryParams>,
) -> Result<Html<String>> {
    state.require_read(&headers)?;
    let query = params.q.unwrap_or_default();
    let default_take = state.config.gallery_page_size.max(1);
    let take = lenient(params.take.as_deref())
        .unwrap_or(default_take)
        .clamp(1, MAX_SEARCH_TAKE);
    // `page` wins over `skip`, and either way the offset snaps to the start of
    // a page. That makes "page N" one exact page, and it is what lets the
    // page-size form send the current `skip`: at the new size, the page shown
    // is the one holding the package that was first on screen.
    let skip = match lenient::<i64>(params.page.as_deref()) {
        Some(page) => (page.max(1) - 1).saturating_mul(take),
        None => lenient(params.skip.as_deref()).unwrap_or(0).max(0),
    };
    let prerelease = lenient::<bool>(params.prerelease.as_deref());
    let package_type = params.package_type.filter(|s| !s.is_empty());
    // An unknown order is the default one, like every other gallery value.
    let sort = params
        .sort
        .as_deref()
        .and_then(SearchSort::parse)
        .unwrap_or_default();
    let tag = gallery_tag(params.tag.as_deref());
    let request = SearchRequest {
        query: query.clone(),
        skip: skip - skip % take,
        take,
        include_prerelease: prerelease.unwrap_or(true),
        include_semver2: true,
        package_type: package_type.clone(),
        sort,
        tag: tag.clone(),
    };
    let page = state.db.search(state.feed(), &request).await?;
    // The landing page (no search, no filter, first page) offers a way in by
    // tag; anywhere else it would be noise above results someone asked for.
    let landing = query.trim().is_empty()
        && tag.is_none()
        && package_type.is_none()
        && request.skip == 0
        && !page.groups.is_empty();
    let popular = if landing {
        state.db.tag_counts(state.feed(), 12).await?
    } else {
        Vec::new()
    };
    let urls = state.url_builder(&headers).with_hive(true);
    Ok(Html(ui::gallery_page(
        &urls,
        &page,
        &ui::GalleryView {
            query: query.trim(),
            skip: request.skip,
            take,
            default_take,
            prerelease,
            package_type: package_type.as_deref(),
            sort,
            tag: tag.as_deref(),
            popular: &popular,
            admin: state.feed.admin.is_enabled(),
        },
    )))
}

async fn tags_page(State(state): State<AppState>, headers: HeaderMap) -> Result<Html<String>> {
    state.require_read(&headers)?;
    let tags = state
        .db
        .tag_counts(state.feed(), ui::MAX_CLOUD_TAGS)
        .await?;
    let urls = state.url_builder(&headers);
    Ok(Html(ui::tags_page(
        &urls,
        &tags,
        state.feed.admin.is_enabled(),
    )))
}

async fn settings_page(State(state): State<AppState>, headers: HeaderMap) -> Result<Html<String>> {
    // The page carries no secrets, but it does describe the feed's policy,
    // mirror and retention posture — the same reconnaissance a gated feed is
    // withholding everywhere else.
    state.require_read(&headers)?;
    let urls = state.url_builder(&headers);
    Ok(Html(ui::settings_page(&urls, &state.config, &state.feed)))
}

async fn stats_page(State(state): State<AppState>, headers: HeaderMap) -> Result<Html<String>> {
    state.require_read(&headers)?;
    let stats = state.db.stats(state.feed()).await?;
    // Reuse the download-ranked search for the "most downloaded" list.
    let top = state
        .db
        .search(
            state.feed(),
            &SearchRequest {
                take: 10,
                ..Default::default()
            },
        )
        .await?;
    let recent = state.db.recent_packages(state.feed(), 10).await?;
    let urls = state.url_builder(&headers);
    Ok(Html(ui::stats_page(
        &urls,
        &stats,
        &top,
        &recent,
        state.feed.admin.is_enabled(),
    )))
}

async fn package_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    render_detail(&state, &headers, &id, None).await
}

async fn package_detail_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<Html<String>> {
    render_detail(&state, &headers, &id, Some(&version)).await
}

/// Serve a package's embedded icon.
///
/// The bytes come from an uploaded `.nupkg`, so this is attacker-controlled
/// content served same-origin to a browser — the one place in the gallery where
/// that is true. Three things keep it inert: the content type is decided by
/// *sniffing the bytes* rather than by trusting any declared name, only raster
/// formats are recognised (notably **not** SVG, which is a script-bearing
/// document), and the response carries its own `default-src 'none'` policy.
async fn package_icon(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<Response> {
    check_id(&id)?;
    state.require_read(&headers)?;
    let version = parse_version(&version)?;
    let Some(package) = state.db.find(state.feed(), &id, &version).await? else {
        return Err(Error::PackageNotFound);
    };
    // Extracted from the package, so the package's hash validates it.
    let etag = format!("\"{}-icon\"", package.package_hash);
    let cache = state.feed.content_cache().header_value();
    if files::if_none_match_hits(&headers, &etag) {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag.as_str()),
                (header::CACHE_CONTROL, cache),
            ],
        )
            .into_response());
    }
    let bytes = state
        .storage
        .get_aux(&id, &version.normalized(), AuxFile::Icon)
        .await?;
    let Some(content_type) = sniff_image(&bytes) else {
        // Stored, but not something we are willing to hand a browser.
        return Err(Error::PackageNotFound);
    };
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, cache),
            (header::ETAG, etag.as_str()),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'"),
            (header::CONTENT_DISPOSITION, "inline"),
        ],
        bytes,
    )
        .into_response())
}

/// Identify a raster image from its magic bytes, or `None` for anything else.
///
/// An allow-list, deliberately: a format that is not recognised is refused
/// rather than guessed at or passed through as `application/octet-stream`.
fn sniff_image(bytes: &[u8]) -> Option<&'static str> {
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    const GIF87: &[u8] = b"GIF87a";
    const GIF89: &[u8] = b"GIF89a";
    const BMP: &[u8] = b"BM";
    const ICO: &[u8] = b"\x00\x00\x01\x00";

    if bytes.starts_with(PNG) {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(GIF87) || bytes.starts_with(GIF89) {
        return Some("image/gif");
    }
    // RIFF....WEBP
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(BMP) {
        return Some("image/bmp");
    }
    if bytes.starts_with(ICO) {
        return Some("image/x-icon");
    }
    None
}

async fn render_detail(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    version: Option<&str>,
) -> Result<Html<String>> {
    check_id(id)?;
    state.require_read(headers)?;
    let packages = state.db.find_versions(state.feed(), id, true).await?;
    if packages.is_empty() {
        return Err(Error::PackageNotFound);
    }

    // `find_versions` returns ascending; the selected version is the requested
    // one, or otherwise the newest listed version (falling back to the newest).
    let selected = match version {
        Some(v) => {
            let want = parse_version(v)?;
            packages
                .iter()
                .find(|p| p.version == want)
                .cloned()
                .ok_or(Error::PackageNotFound)?
        }
        // Newest listed *stable* version, falling back to the newest listed
        // version of any kind, then to the newest version at all.
        //
        // Defaulting to the newest version outright meant the page — and the
        // `choco install …`/`dotnet add package …` command under the copy
        // button — headlined `2.0.0-beta` while Visual Studio and `dotnet`
        // searching the same feed offered `1.9.0`, because `/v3/search`
        // excludes pre-releases unless asked. Copying the command then pulled a
        // pre-release into a project that had not opted into one.
        None => packages
            .iter()
            .rev()
            .find(|p| p.listed && !p.is_prerelease())
            .or_else(|| packages.iter().rev().find(|p| p.listed))
            .or_else(|| packages.last())
            .cloned()
            .ok_or(Error::PackageNotFound)?,
    };

    let readme = if selected.has_readme {
        state
            .storage
            .get_aux(id, &selected.normalized_version(), AuxFile::Readme)
            .await
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
    } else {
        None
    };

    let has_symbols = !state
        .db
        .find_symbols(id, &selected.version)
        .await
        .unwrap_or_default()
        .is_empty();

    let files = if state.config.files.enabled {
        state.db.files_for(id, &selected.version).await?
    } else {
        Vec::new()
    };

    let urls = state.url_builder(headers);
    Ok(Html(ui::detail_page(
        &urls,
        &packages,
        &selected,
        &ui::Detail {
            readme: readme.as_deref(),
            primary_client: &state.config.primary_client,
            has_symbols,
            admin: state.feed.admin.is_enabled(),
            files: &files,
        },
    )))
}

// ---------------------------------------------------------------------------
// Admin area (HTTP Basic auth)
// ---------------------------------------------------------------------------

/// Guard every admin route: valid admin credentials or a Basic-auth
/// challenge, and never a cached copy of what is behind it.
///
/// Admin pages embed a CSRF token and list what an operator can see; neither
/// belongs in a browser's disk cache or a shared cache, so every answer —
/// the challenge included — is `no-store`.
async fn admin_gate(
    State(state): State<AppState>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let mut response = if state.feed.admin.check_headers(request.headers()) {
        next.run(request).await
    } else {
        Error::AdminUnauthorized.into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

/// Header alternative to the hidden form field, for scripted admin calls.
const CSRF_HEADER: &str = "x-csrf-token";

/// Check that an admin *state change* was actually issued from the admin UI.
/// The credentials themselves were already checked by [`admin_gate`].
///
/// HTTP Basic credentials are replayed by the browser on every request to this
/// origin, so authentication alone does not distinguish a click in `/admin`
/// from a form auto-submitted by a hostile page in another tab. Two independent
/// checks close that:
///
/// * `Sec-Fetch-Site` — browsers set it on every request; anything other than
///   same-origin is rejected outright. Non-browser callers omit it.
/// * The CSRF token, an HMAC under a per-process secret (see
///   [`crate::auth::AdminAuth::csrf_token`]). An attacker who cannot read an
///   admin page cannot produce it.
fn require_admin_action(state: &AppState, headers: &HeaderMap, body: &str) -> Result<()> {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        if !matches!(site.trim(), "same-origin" | "none") {
            return Err(Error::BadRequest(
                "cross-site admin requests are refused".into(),
            ));
        }
    }

    let presented = headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .or_else(|| form_field(body, ui::CSRF_FIELD));

    if state.feed.admin.check_csrf(presented.as_deref()) {
        Ok(())
    } else {
        Err(Error::BadRequest(format!(
            "missing, expired or invalid {} token (reload the page)",
            ui::CSRF_FIELD
        )))
    }
}

/// Read one field out of an `application/x-www-form-urlencoded` body.
fn form_field(body: &str, name: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (decode_form_value(k) == name).then(|| decode_form_value(v))
    })
}

/// Every value of a repeated form field (`v=1.0.0&v=2.0.0`), in order.
fn form_fields(body: &str, name: &str) -> Vec<String> {
    body.split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            (decode_form_value(k) == name).then(|| decode_form_value(v))
        })
        .collect()
}

fn decode_form_value(raw: &str) -> String {
    let plus_decoded = raw.replace('+', " ");
    percent_encoding::percent_decode_str(&plus_decoded)
        .decode_utf8_lossy()
        .into_owned()
}

async fn admin_dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>> {
    let ids = state.db.all_package_ids(state.feed()).await?;
    let urls = state.url_builder(&headers);
    Ok(Html(ui::admin_dashboard_page(&urls, &ids)))
}

async fn admin_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    check_id(&id)?;
    let versions = state.db.find_all_versions(state.feed(), &id).await?;
    if versions.is_empty() {
        return Err(Error::PackageNotFound);
    }
    let urls = state.url_builder(&headers);
    let targets: Vec<String> = transfer_targets(&state, &headers)
        .map(|m| m.name.clone())
        .collect();
    // The id as published, not as typed into the address.
    let display_id = &versions[0].package.id;
    // What the next cleanup would do to this package, when one will run.
    let policy = RetentionPolicy::from(&state.feed.retention);
    let plan = if state.feed.retention.enabled {
        retention::plan_for(&versions, &policy, chrono::Utc::now())
    } else {
        Vec::new()
    };
    let mut files = Vec::new();
    if state.config.files.enabled {
        for fv in versions.iter().rev() {
            files.extend(state.db.files_for(&id, &fv.package.version).await?);
        }
    }
    Ok(Html(ui::admin_package_page(
        &urls,
        display_id,
        &versions,
        &ui::AdminPackageExtras {
            promote_target: state.feed.promotes_to.as_deref(),
            transfer_targets: &targets,
            retention_plan: &plan,
            files_enabled: state.config.files.enabled,
            files: &files,
        },
        &state.feed.admin.csrf_token().unwrap_or_default(),
    )))
}

/// Detach a file from the admin page.
async fn admin_file_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version, name)): Path<(String, String, String)>,
    body: String,
) -> Result<Response> {
    check_id(&id)?;
    require_admin_action(&state, &headers, &body)?;
    let v = parse_version(&version)?;
    hosted::detach(&state, &id, &v, &name).await?;
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, &id)).into_response())
}

/// The feeds this request may copy or move versions into: every other feed
/// whose own admin key the request also presents, and the promotion target,
/// which the configuration already trusts this feed's admin to fill.
///
/// Holding one feed's admin key must not be a way to write into another:
/// without the target's key, a `dev` admin could move anything into `stable`.
fn transfer_targets<'a>(
    state: &'a AppState,
    headers: &'a HeaderMap,
) -> impl Iterator<Item = &'a FeedMeta> + 'a {
    state.feeds.iter().filter(move |m| {
        m.name != state.feed.name
            && (m.admin.check_headers(headers)
                || state.feed.promotes_to.as_deref() == Some(m.name.as_str()))
    })
}

/// Whether [`transfer_version`] leaves the version in this feed too.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Transfer {
    /// Add it to the target and keep it here (what a promotion does).
    Copy,
    /// Add it to the target and take it out of here.
    Move,
}

/// Copy or move one version of this feed into `target`.
///
/// The target's license policy and approval gate apply, exactly as for a push
/// into it. A copy arrives active, as a promotion always has; a move keeps
/// the listed and enabled state it had here, since it is the same version,
/// elsewhere. A version the target already holds keeps its state there. Its
/// files are never touched: the target holds them afterwards either way.
async fn transfer_version(
    state: &AppState,
    target: &FeedMeta,
    id: &str,
    v: &NuGetVersion,
    mode: Transfer,
) -> Result<()> {
    // You can only hand on what *this* feed holds — not any globally-known
    // version that happens to live in some other feed. Checked again under
    // the lock below; this first look keeps such a version from being judged
    // against the target's policy at all.
    if !state.db.exists(state.feed(), id, v).await? {
        return Err(Error::PackageNotFound);
    }
    let package = state
        .db
        .get_package_data(id, v)
        .await?
        .ok_or(Error::PackageNotFound)?;

    // The target feed's own license policy, not this one's. A promotion is how
    // a version enters that feed, so it has to clear the same rule a direct
    // push would: otherwise `dev` with no policy is a way around `stable`'s
    // `action = "block"`.
    let outcome = crate::policy::evaluate_license(&target.license_policy, &package);
    if !outcome.allowed {
        return Err(Error::PolicyViolation(format!(
            "{} rejects this package: {}",
            target.name,
            outcome.violation.unwrap_or_else(|| "license policy".into())
        )));
    }

    // Under the same lock as a push or a purge of this version, so the checks
    // below and the membership changes after them see one consistent state.
    // Without it a retention sweep that drops the last membership in between
    // deletes the shared data, and the insert then leaves a membership
    // pointing at a package row that no longer exists — invisible to every
    // query (they all inner-join) yet enough to make a later push conflict.
    let _guard = crate::locks::lock_version(id, &v.normalized()).await;
    let Some(here) = state.db.get_membership(state.feed(), id, v).await? else {
        return Err(Error::PackageNotFound);
    };
    if !state.db.package_data_exists(id, v).await? {
        return Err(Error::PackageNotFound);
    }
    let mut membership = Membership {
        pending: target.requires_approval,
        flagged: outcome.violation.is_some(),
        flag_reason: outcome.violation,
        ..Membership::active(&target.name, &package)
    };
    if mode == Transfer::Move {
        membership.listed = here.listed;
        membership.enabled = here.enabled;
        membership.pinned = here.pinned;
    }
    match state.db.add_membership(&membership).await {
        Ok(()) | Err(Error::PackageAlreadyExists) => {}
        Err(e) => return Err(e),
    }
    if mode == Transfer::Move {
        state.db.remove_membership(state.feed(), id, v).await?;
    }
    Ok(())
}

/// What the admin page's selection form asks for.
#[derive(Clone, Copy)]
enum BulkOp {
    Enable,
    Disable,
    Approve,
    Pin,
    Unpin,
    Delete,
    Copy,
    Move,
}

impl BulkOp {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "enable" => Self::Enable,
            "disable" => Self::Disable,
            "approve" => Self::Approve,
            "pin" => Self::Pin,
            "unpin" => Self::Unpin,
            "delete" => Self::Delete,
            "copy" => Self::Copy,
            "move" => Self::Move,
            _ => return None,
        })
    }
}

/// Apply one action to every version ticked on the admin page — the way a
/// whole package is disabled, deleted, or moved to another feed.
///
/// Every version is checked before anything changes, so a typo'd version or
/// a target that refuses the package leaves the feed as it was rather than
/// half done. Nothing selected sends the admin back to the page unchanged.
async fn admin_bulk(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: String,
) -> Result<Response> {
    check_id(&id)?;
    require_admin_action(&state, &headers, &body)?;
    let back = Redirect::to(&admin_package_url(&state.feed.prefix, &id)).into_response();
    let op = form_field(&body, "op")
        .as_deref()
        .and_then(BulkOp::parse)
        .ok_or_else(|| Error::BadRequest("unknown or missing action".into()))?;
    let versions = form_fields(&body, "v")
        .iter()
        .map(|v| parse_version(v))
        .collect::<Result<Vec<_>>>()?;
    if versions.is_empty() {
        return Ok(back);
    }
    for v in &versions {
        if !state.db.exists(state.feed(), &id, v).await? {
            return Err(Error::PackageNotFound);
        }
    }

    match op {
        BulkOp::Enable | BulkOp::Disable => {
            let enabled = matches!(op, BulkOp::Enable);
            for v in &versions {
                state.db.set_enabled(state.feed(), &id, v, enabled).await?;
            }
        }
        BulkOp::Approve => {
            for v in &versions {
                state.db.approve_membership(state.feed(), &id, v).await?;
            }
        }
        BulkOp::Pin | BulkOp::Unpin => {
            let pinned = matches!(op, BulkOp::Pin);
            for v in &versions {
                state.db.set_pinned(state.feed(), &id, v, pinned).await?;
            }
        }
        BulkOp::Delete => {
            for v in &versions {
                retention::purge_version(
                    state.storage.as_ref(),
                    state.db.as_ref(),
                    state.feed(),
                    &id,
                    v,
                )
                .await?;
            }
        }
        BulkOp::Copy | BulkOp::Move => {
            let name = form_field(&body, "target").unwrap_or_default();
            let target = transfer_targets(&state, &headers)
                .find(|m| m.name == name)
                .ok_or_else(|| {
                    Error::BadRequest(format!(
                        "{name:?} is not a feed these credentials may add versions to"
                    ))
                })?;
            // Refuse the lot up front if the target's policy refuses any of
            // it, rather than moving half a package.
            for v in &versions {
                let package = state
                    .db
                    .get_package_data(&id, v)
                    .await?
                    .ok_or(Error::PackageNotFound)?;
                let outcome = crate::policy::evaluate_license(&target.license_policy, &package);
                if !outcome.allowed {
                    return Err(Error::PolicyViolation(format!(
                        "{} rejects {} {}: {}",
                        target.name,
                        package.id,
                        v.normalized(),
                        outcome.violation.unwrap_or_else(|| "license policy".into())
                    )));
                }
            }
            let mode = if matches!(op, BulkOp::Move) {
                Transfer::Move
            } else {
                Transfer::Copy
            };
            for v in &versions {
                transfer_version(&state, target, &id, v, mode).await?;
            }
            tracing::info!(
                from = %state.feed(),
                to = %target.name,
                %id,
                count = versions.len(),
                moved = mode == Transfer::Move,
                "transferred versions"
            );
        }
    }

    // Back to the package if this feed still holds any of it, else the list.
    if state
        .db
        .find_all_versions(state.feed(), &id)
        .await?
        .is_empty()
    {
        return Ok(Redirect::to(&format!("{}/admin", state.feed.prefix)).into_response());
    }
    Ok(back)
}

async fn admin_disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    admin_set_enabled(&state, &headers, &body, &id, &version, false).await
}

async fn admin_enable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    admin_set_enabled(&state, &headers, &body, &id, &version, true).await
}

async fn admin_set_enabled(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    id: &str,
    version: &str,
    enabled: bool,
) -> Result<Response> {
    check_id(id)?;
    require_admin_action(state, headers, body)?;
    let v = parse_version(version)?;
    if !state.db.set_enabled(state.feed(), id, &v, enabled).await? {
        return Err(Error::PackageNotFound);
    }
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, id)).into_response())
}

async fn admin_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    check_id(&id)?;
    require_admin_action(&state, &headers, &body)?;
    let v = parse_version(&version)?;
    if !state.db.approve_membership(state.feed(), &id, &v).await? {
        return Err(Error::PackageNotFound);
    }
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, &id)).into_response())
}

async fn admin_promote(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    check_id(&id)?;
    require_admin_action(&state, &headers, &body)?;
    let Some(target) = &state.feed.promotes_to else {
        return Err(Error::BadRequest(
            "this feed has no promotion target".into(),
        ));
    };
    let v = parse_version(&version)?;
    // The target must be a known feed; the copy clears its policy and gate.
    let target_meta = state
        .feeds
        .iter()
        .find(|m| &m.name == target)
        .ok_or_else(|| Error::BadRequest(format!("unknown promotion target {target:?}")))?;
    transfer_version(&state, target_meta, &id, &v, Transfer::Copy).await?;
    tracing::info!(from = %state.feed(), to = %target, %id, version = %v.normalized(), "promoted version");
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, &id)).into_response())
}

async fn admin_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    check_id(&id)?;
    require_admin_action(&state, &headers, &body)?;
    let v = parse_version(&version)?;
    if !retention::purge_version(
        state.storage.as_ref(),
        state.db.as_ref(),
        state.feed(),
        &id,
        &v,
    )
    .await?
    {
        return Err(Error::PackageNotFound);
    }
    // Back to the package page if other versions remain, else the dashboard.
    let remaining = state.db.find_all_versions(state.feed(), &id).await?;
    let target = if remaining.is_empty() {
        format!("{}/admin", state.feed.prefix)
    } else {
        admin_package_url(&state.feed.prefix, &id)
    };
    Ok(Redirect::to(&target).into_response())
}

async fn admin_pin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    admin_set_pinned(&state, &headers, &body, &id, &version, true).await
}

async fn admin_unpin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
    body: String,
) -> Result<Response> {
    admin_set_pinned(&state, &headers, &body, &id, &version, false).await
}

async fn admin_set_pinned(
    state: &AppState,
    headers: &HeaderMap,
    body: &str,
    id: &str,
    version: &str,
    pinned: bool,
) -> Result<Response> {
    check_id(id)?;
    require_admin_action(state, headers, body)?;
    let v = parse_version(version)?;
    if !state.db.set_pinned(state.feed(), id, &v, pinned).await? {
        return Err(Error::PackageNotFound);
    }
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, id)).into_response())
}

/// What the retention page reports after a cleanup it was asked for: numbers
/// only, parsed, never echoed text.
#[derive(Debug, Deserialize)]
struct RetentionQuery {
    #[serde(default)]
    deleted: Option<String>,
    #[serde(default)]
    freed: Option<String>,
    #[serde(default)]
    errors: Option<String>,
    #[serde(default)]
    changed: Option<String>,
    #[serde(default)]
    busy: Option<String>,
}

async fn admin_retention(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RetentionQuery>,
) -> Result<Html<String>> {
    let rules = &state.feed.retention;
    let policy = RetentionPolicy::from(rules);
    let preview = if policy.has_limits() {
        Some(
            retention::preview(state.db.as_ref(), state.feed(), &policy, chrono::Utc::now())
                .await?,
        )
    } else {
        None
    };
    let notice = if q.busy.is_some() {
        ui::RetentionNotice::Busy
    } else if q.changed.is_some() {
        ui::RetentionNotice::Changed
    } else if let Some(deleted) = lenient::<usize>(q.deleted.as_deref()) {
        ui::RetentionNotice::Done(retention::Outcome {
            deleted,
            freed: lenient(q.freed.as_deref()).unwrap_or(0),
            errors: lenient(q.errors.as_deref()).unwrap_or(0),
        })
    } else {
        ui::RetentionNotice::None
    };
    let urls = state.url_builder(&headers);
    Ok(Html(ui::admin_retention_page(
        &urls,
        &ui::RetentionView {
            rules,
            last: state.feed.cleanup.last(),
            running: state.feed.cleanup.is_running(),
            preview: preview.as_ref(),
            csrf_token: &state.feed.admin.csrf_token().unwrap_or_default(),
            notice,
        },
    )))
}

/// Delete what the retention page showed — and only that.
///
/// The form carries the fingerprint of the plan it displayed. The plan is
/// recomputed here and applied only when it still matches, so a push landing
/// between looking and clicking can never widen what the click deletes. A
/// cleanup already running (the background sweep, or another admin) turns
/// this away rather than queueing a second one behind it.
async fn admin_retention_run(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: String,
) -> Result<Response> {
    require_admin_action(&state, &headers, &body)?;
    let policy = RetentionPolicy::from(&state.feed.retention);
    if !state.feed.retention.enabled || !policy.has_limits() {
        return Err(Error::BadRequest(
            "retention is not enabled for this feed".into(),
        ));
    }
    let fingerprint = form_field(&body, "plan").unwrap_or_default();
    let page = format!("{}/admin/retention", state.feed.prefix);
    let result = state
        .feed
        .cleanup
        .run_shown(
            state.storage.as_ref(),
            state.db.as_ref(),
            state.feed(),
            &policy,
            &fingerprint,
        )
        .await?;
    let target = match result {
        None => format!("{page}?busy=1"),
        Some(Err(_)) => format!("{page}?changed=1"),
        Some(Ok(o)) => {
            tracing::info!(feed = %state.feed(), deleted = o.deleted, freed = o.freed, "manual retention cleanup");
            format!(
                "{page}?deleted={}&freed={}&errors={}",
                o.deleted, o.freed, o.errors
            )
        }
    };
    Ok(Redirect::to(&target).into_response())
}

fn admin_package_url(prefix: &str, id: &str) -> String {
    format!("{prefix}/admin/packages/{}", id.to_lowercase())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Refuse a package id taken from a URL unless it is one a package could
/// have, before it reaches the database, the store or a lock.
///
/// The layers do not fold case the same way: the database uses Unicode
/// `to_lowercase()`, storage paths and the version lock ASCII. An id no push
/// could create — `\u{212A}` (the Kelvin sign) folds to `k` in one and not in
/// the other — could match a database row while missing its directory and its
/// lock: a delete then removed the rows, orphaned the payload, and did not
/// serialise with a concurrent push. Such an id names nothing, so it is a 404.
fn check_id(id: &str) -> Result<()> {
    crate::validation::validate_package_id(id).map_err(|_| Error::PackageNotFound)
}

fn parse_version(raw: &str) -> Result<NuGetVersion> {
    NuGetVersion::parse(raw).map_err(|e| Error::InvalidVersion(e.to_string()))
}

/// A `semVerLevel` of `2.0.0` (or higher major) enables SemVer2 results.
fn is_semver2_level(level: Option<&str>) -> bool {
    match level {
        Some(l) => NuGetVersion::parse(l)
            .map(|v| v.core().0 >= 2)
            .unwrap_or(false),
        None => false,
    }
}

fn forwarded(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
        .filter(|s| !s.is_empty())
}

fn to_io_err<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

fn map_upload_err(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::InvalidData {
        Error::PayloadTooLarge(e.to_string())
    } else if e.kind() == std::io::ErrorKind::TimedOut {
        Error::UploadTimeout(e.to_string())
    } else {
        Error::Io(e)
    }
}
