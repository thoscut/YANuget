//! The HTTP layer: application state, routing and request handlers.
//!
//! Each hosted feed gets its own [`AppState`] (sharing the process-wide storage
//! and database) and its own router, mounted under the feed's path prefix. A
//! single unconfigured feed is served at the root, preserving the original
//! single-feed URLs.

mod files;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use futures::StreamExt;
use serde::Deserialize;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::auth::{AdminAuth, ApiKeyAuth, ReadAuth};
use crate::config::{Config, LicensePolicyConfig, ResolvedFeed, RetentionConfig};
use crate::database::{Membership, PackageDatabase, SearchRequest};
use crate::error::{Error, Result};
use crate::indexing::{self, IndexOptions};
use crate::mirror::{self, MirrorClient, MirrorOptions};
use crate::nuget::{self, UrlBuilder};
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
    pub allow_overwrite: bool,
    pub hard_delete_enabled: bool,
    /// Incoming versions are pending (withheld) until an admin approves them.
    pub requires_approval: bool,
    /// The feed an admin can promote a version into (the next release ring).
    pub promotes_to: Option<String>,
    /// Upstream mirror client, when mirroring is enabled for this feed.
    pub mirror: Option<MirrorClient>,
    pub license_policy: LicensePolicyConfig,
    pub retention: RetentionConfig,
}

impl FeedContext {
    fn from_resolved(feed: &ResolvedFeed) -> Self {
        Self {
            name: feed.name.clone(),
            prefix: feed.prefix.clone(),
            auth: ApiKeyAuth::new(feed.api_key.clone()),
            read_auth: ReadAuth::new(feed.read_api_key.clone()),
            admin: AdminAuth::new(feed.admin_api_key.clone()),
            allow_overwrite: feed.allow_overwrite,
            hard_delete_enabled: feed.hard_delete_enabled,
            requires_approval: feed.requires_approval,
            promotes_to: feed.promotes_to.clone(),
            mirror: MirrorClient::from_config(&feed.mirror),
            license_policy: feed.license_policy.clone(),
            retention: feed.retention.clone(),
        }
    }
}

/// Lightweight, cross-feed metadata so a handler can resolve a promotion target.
#[derive(Debug, Clone)]
pub struct FeedMeta {
    pub name: String,
    pub prefix: String,
    pub requires_approval: bool,
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
        Ok(Self {
            storage,
            db,
            config,
            feed: Arc::new(FeedContext::from_resolved(resolved)),
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
            let scheme = forwarded(headers, "x-forwarded-proto")
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
    let mut top = Router::new().route("/health", get(health));

    if states.len() == 1 && states[0].feed.prefix.is_empty() {
        top = top.merge(feed_routes(states.into_iter().next().expect("one state")));
    } else {
        let index: Vec<(String, String)> = states
            .iter()
            .map(|s| (s.feed.name.clone(), format!("{}/", s.feed.prefix)))
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
            top = top.nest(&prefix, feed_routes(s));
        }
    }

    top.layer(DefaultBodyLimit::disable())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
}

/// Build a single feed's complete application (with global middleware). Used by
/// tests and single-feed deployments.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .merge(feed_routes(state))
        // Uploads stream straight to disk; remove axum's small default cap so
        // multi-gigabyte packages are accepted (the configured size limit is
        // still enforced while streaming).
        .layer(DefaultBodyLimit::disable())
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
}

/// Build one feed's routes (relative paths, no global middleware), ready to be
/// nested under the feed's prefix or merged at the root.
fn feed_routes(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/v3/index.json", get(service_index))
        .route("/api/v2/package", put(push_package))
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
        .route("/v3/registration/{id}/{version}", get(registration_leaf))
        .route("/v3/search", get(search))
        .route("/v3/autocomplete", get(autocomplete));

    // Symbol server: push `.snupkg` and serve PDBs over the SSQP path.
    if state.config.enable_symbol_server {
        router = router
            .route("/api/v2/symbol", put(push_symbol_package))
            .route(
                "/download/symbols/{file}/{key}/{file2}",
                get(download_symbol),
            );
    }

    // Human-facing gallery. When disabled, `/` falls back to a minimal page.
    if state.config.enable_web_ui {
        router = router
            .route("/", get(gallery))
            .route("/packages", get(gallery))
            .route("/packages/{id}", get(package_detail))
            .route("/packages/{id}/{version}", get(package_detail_version))
            .route("/stats", get(stats_page))
            .route("/settings", get(settings_page));

        // Admin area (disable/enable/delete/approve/promote versions), behind
        // HTTP Basic auth. Only mounted when an admin key is configured.
        if state.feed.admin.is_enabled() {
            router = router
                .route("/admin", get(admin_dashboard))
                .route("/admin/packages/{id}", get(admin_package))
                .route(
                    "/admin/packages/{id}/{version}/disable",
                    post(admin_disable),
                )
                .route("/admin/packages/{id}/{version}/enable", post(admin_enable))
                .route("/admin/packages/{id}/{version}/delete", post(admin_delete))
                .route(
                    "/admin/packages/{id}/{version}/approve",
                    post(admin_approve),
                )
                .route(
                    "/admin/packages/{id}/{version}/promote",
                    post(admin_promote),
                );
        }
    } else {
        router = router.route("/", get(index_page));
    }

    router.with_state(state)
}

// ---------------------------------------------------------------------------
// Informational endpoints
// ---------------------------------------------------------------------------

async fn health() -> &'static str {
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
        idx = urls.service_index()
    ))
}

async fn service_index(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let urls = state.url_builder(&headers);
    Json(nuget::service_index(&urls))
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
    drop(file);

    let options = IndexOptions {
        allow_overwrite: state.feed.allow_overwrite,
        pending: state.feed.requires_approval,
        license_policy: state.feed.license_policy.clone(),
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
        streaming::stream_to_writer_limited(stream, file, limit)
            .await
            .map_err(map_upload_err)
    } else {
        let stream = Box::pin(
            request
                .into_body()
                .into_data_stream()
                .map(|r| r.map_err(to_io_err)),
        );
        streaming::stream_to_writer_limited(stream, file, limit)
            .await
            .map_err(map_upload_err)
    }
}

async fn delete_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<StatusCode> {
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
    state.require_read(&headers)?;
    let mut packages = state.db.find_versions(state.feed(), &id, false).await?;
    if packages.is_empty() {
        state.mirror_if_needed(&id).await;
        packages = state.db.find_versions(state.feed(), &id, false).await?;
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
    headers: HeaderMap,
    Path((id, version, filename)): Path<(String, String, String)>,
) -> Result<Response> {
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
    if filename.to_lowercase().ends_with(".nuspec") {
        let nuspec = state
            .storage
            .get_aux(&id, &normalized, AuxFile::Nuspec)
            .await?;
        return Ok(([(header::CONTENT_TYPE, "application/xml")], nuspec).into_response());
    }

    let content = state.storage.get_package(&id, &normalized).await?;

    // Count the download (best effort — never block the response on it).
    let _ = state.db.increment_downloads(&id, &version).await;

    match content {
        PackageContent::LocalPath(path) => {
            let name = format!("{}.{}.nupkg", id.to_lowercase(), normalized.to_lowercase());
            files::serve_local_file(path, &headers, NUPKG_CONTENT_TYPE, Some(&name)).await
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

async fn registration_index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    state.require_read(&headers)?;
    // Registration includes unlisted versions (flagged listed=false).
    let mut packages = state.db.find_versions(state.feed(), &id, true).await?;
    if packages.is_empty() {
        state.mirror_if_needed(&id).await;
        packages = state.db.find_versions(state.feed(), &id, true).await?;
    }
    if packages.is_empty() {
        return Err(Error::PackageNotFound);
    }
    let urls = state.url_builder(&headers);
    Ok(Json(nuget::registration_index(&urls, &id, &packages)))
}

async fn registration_leaf(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    state.require_read(&headers)?;
    let version = version.strip_suffix(".json").unwrap_or(&version);
    let version = parse_version(version)?;
    let package = state
        .db
        .find(state.feed(), &id, &version)
        .await?
        .ok_or(Error::PackageNotFound)?;
    let urls = state.url_builder(&headers);
    Ok(Json(nuget::registration_leaf(&urls, &id, &package)))
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
    };
    let page = state.db.search(state.feed(), &request).await?;
    let urls = state.url_builder(&headers);
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
        let packages = state.db.find_versions(state.feed(), &id, false).await?;
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
    let ids = state
        .db
        .autocomplete(state.feed(), &params.q.unwrap_or_default(), skip, take)
        .await?;
    let total = ids.len() as i64;
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

    let (temp_path, mut file) = state.create_temp().await?;
    let limit = state.config.max_package_size_bytes;

    if let Err(e) = write_upload(request, &mut file, is_multipart, limit, &state).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(e);
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
        "indexed symbol package",
    );
    Ok(StatusCode::CREATED.into_response())
}

async fn download_symbol(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((file, key, file2)): Path<(String, String, String)>,
) -> Result<Response> {
    // The SSQP path repeats the file name; both segments must agree.
    if !file.eq_ignore_ascii_case(&file2) {
        return Err(Error::PackageNotFound);
    }
    let content = state.storage.get_symbol(&key, &file).await?;
    match content {
        PackageContent::LocalPath(path) => {
            files::serve_local_file(path, &headers, NUPKG_CONTENT_TYPE, None).await
        }
    }
}

// ---------------------------------------------------------------------------
// Web gallery (HTML)
// ---------------------------------------------------------------------------

async fn gallery(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<SearchParams>,
) -> Result<Html<String>> {
    let query = params.q.unwrap_or_default();
    let default_take = state.config.gallery_page_size.max(1);
    let request = SearchRequest {
        query: query.clone(),
        skip: params.skip.unwrap_or(0).max(0),
        take: params
            .take
            .unwrap_or(default_take)
            .clamp(1, MAX_SEARCH_TAKE),
        include_prerelease: params.prerelease.unwrap_or(true),
        include_semver2: true,
        package_type: params.package_type.filter(|s| !s.is_empty()),
    };
    let page = state.db.search(state.feed(), &request).await?;
    let urls = state.url_builder(&headers);
    Ok(Html(ui::gallery_page(
        &urls,
        &page,
        query.trim(),
        request.skip,
        request.take,
    )))
}

async fn settings_page(State(state): State<AppState>, headers: HeaderMap) -> Html<String> {
    let urls = state.url_builder(&headers);
    Html(ui::settings_page(&urls, &state.config, &state.feed))
}

async fn stats_page(State(state): State<AppState>, headers: HeaderMap) -> Result<Html<String>> {
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
    Ok(Html(ui::stats_page(&urls, &stats, &top, &recent)))
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

async fn render_detail(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    version: Option<&str>,
) -> Result<Html<String>> {
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
        None => packages
            .iter()
            .rev()
            .find(|p| p.listed)
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

    let urls = state.url_builder(headers);
    Ok(Html(ui::detail_page(
        &urls,
        &packages,
        &selected,
        readme.as_deref(),
        &state.config.primary_client,
        has_symbols,
    )))
}

// ---------------------------------------------------------------------------
// Admin area (HTTP Basic auth)
// ---------------------------------------------------------------------------

/// Reject the request with a Basic-auth challenge unless valid admin
/// credentials are presented.
fn require_admin(state: &AppState, headers: &HeaderMap) -> Result<()> {
    if state.feed.admin.check_headers(headers) {
        Ok(())
    } else {
        Err(Error::AdminUnauthorized)
    }
}

async fn admin_dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>> {
    require_admin(&state, &headers)?;
    let ids = state.db.all_package_ids(state.feed()).await?;
    let urls = state.url_builder(&headers);
    Ok(Html(ui::admin_dashboard_page(&urls, &ids)))
}

async fn admin_package(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    require_admin(&state, &headers)?;
    let versions = state.db.find_all_versions(state.feed(), &id).await?;
    if versions.is_empty() {
        return Err(Error::PackageNotFound);
    }
    let urls = state.url_builder(&headers);
    Ok(Html(ui::admin_package_page(
        &urls,
        &id,
        &versions,
        state.feed.promotes_to.as_deref(),
    )))
}

async fn admin_disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<Response> {
    admin_set_enabled(&state, &headers, &id, &version, false).await
}

async fn admin_enable(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<Response> {
    admin_set_enabled(&state, &headers, &id, &version, true).await
}

async fn admin_set_enabled(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    version: &str,
    enabled: bool,
) -> Result<Response> {
    require_admin(state, headers)?;
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
) -> Result<Response> {
    require_admin(&state, &headers)?;
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
) -> Result<Response> {
    require_admin(&state, &headers)?;
    let Some(target) = &state.feed.promotes_to else {
        return Err(Error::BadRequest(
            "this feed has no promotion target".into(),
        ));
    };
    let v = parse_version(&version)?;
    // The version's global data already exists (it is in this feed); promoting
    // just adds a membership to the next ring, pending if that ring gates.
    let package = state
        .db
        .get_package_data(&id, &v)
        .await?
        .ok_or(Error::PackageNotFound)?;
    let target_gates = state
        .feeds
        .iter()
        .find(|m| &m.name == target)
        .map(|m| m.requires_approval)
        .unwrap_or(false);
    let membership = Membership {
        pending: target_gates,
        ..Membership::active(target, &package)
    };
    match state.db.add_membership(&membership).await {
        Ok(()) | Err(Error::PackageAlreadyExists) => {}
        Err(e) => return Err(e),
    }
    tracing::info!(from = %state.feed(), to = %target, %id, version = %v.normalized(), "promoted version");
    Ok(Redirect::to(&admin_package_url(&state.feed.prefix, &id)).into_response())
}

async fn admin_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<Response> {
    require_admin(&state, &headers)?;
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

fn admin_package_url(prefix: &str, id: &str) -> String {
    format!("{prefix}/admin/packages/{}", id.to_lowercase())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
    } else {
        Error::Io(e)
    }
}
