//! The HTTP layer: application state, routing and request handlers.
//!
//! Each hosted feed gets its own [`AppState`] (sharing the process-wide storage
//! and database) and its own router, mounted under the feed's path prefix. A
//! single unconfigured feed is served at the root, preserving the original
//! single-feed URLs.

mod assets;
mod docs;
mod files;
mod forms;
mod helpers;
pub(crate) mod hosted;
mod middleware;
mod state;
mod ui;

pub use hosted::sweep_expired_uploads;
pub use state::{AppState, FeedContext, FeedMeta};

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{delete, get, head, post, put};
use axum::Router;
use futures::StreamExt;
use serde::Deserialize;

use crate::database::{Membership, MembershipChange, SearchRequest, SearchSort};
use crate::error::{Error, Result};
use crate::indexing::{self, IndexOptions};
use crate::nuget::{self, UrlBuilder};
use crate::retention::{self, RetentionPolicy};
use crate::storage::{AuxFile, PackageContent, TempPath};
use crate::streaming::{self, StreamSummary};
use crate::symbols;
use crate::version::NuGetVersion;

use forms::{form_field, form_fields, lenient};
use helpers::{
    check_id, content_length, detached, is_semver2_level, map_upload_err, parse_stored_version,
    parse_version, to_io_err,
};
use middleware::{apply_global_layers, gated_feed_headers, html_errors, GlobalLayers};

const NUPKG_CONTENT_TYPE: &str = "application/octet-stream";
const MAX_SEARCH_TAKE: i64 = 1000;

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
        // Read-gated feeds are left off the public index (see
        // `feeds_index_page`).
        let index: Vec<(String, String)> = states
            .iter()
            .filter(|s| !s.feed.read_auth.is_enabled())
            .map(|s| (s.feed.name.clone(), s.feed.prefix.clone()))
            .collect();
        let html = ui::feeds_index_page(&index, index.len() < states.len());
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
    // Built from the request's `Host`, so escaped like every other sink.
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
    let (temp, summary) = receive_push(&state, request).await?;

    let options = IndexOptions {
        overwrite: state.feed.allow_overwrite,
        pending: state.feed.requires_approval,
        license_policy: state.feed.license_policy.clone(),
        // A push is self-describing: the manifest defines the identity.
        expect: None,
        reserved_elsewhere: state.feed.reserved_elsewhere.clone(),
    };
    let state = state.clone();
    detached(async move {
        let result = indexing::index_package(
            state.storage.as_ref(),
            state.db.as_ref(),
            state.feed(),
            temp.path().to_path_buf(),
            summary,
            &options,
        )
        .await?;
        // Pushing a deleted version back is how it is un-deleted for the
        // mirror.
        if let Err(e) = state
            .db
            .clear_tombstone(state.feed(), &result.id, &result.version)
            .await
        {
            tracing::warn!(id = %result.id, error = %e, "could not clear the version's tombstone");
        }

        // Optionally prune older versions of this id in this feed
        // (best-effort: never fail the push because of retention).
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
        Ok(())
    })
    .await?;

    Ok(StatusCode::CREATED.into_response())
}

/// The part of a push that a package and a symbol package share: check the
/// push key, then stream the body (raw or multipart) to a fresh temp file,
/// synced to disk. The temp file is removed on every way out that does not
/// store it — an error here, a failed index later, or a dropped connection.
async fn receive_push(state: &AppState, request: Request) -> Result<(TempPath, StreamSummary)> {
    let headers = request.headers();
    if !state.feed.auth.check_headers(headers) {
        return Err(Error::Unauthorized);
    }
    let is_multipart = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("multipart/"));

    state.ensure_disk_space(content_length(headers))?;
    let (temp, mut file) = state.create_temp().await?;
    let limit = state.config.max_package_size_bytes;

    let summary = write_upload(request, &mut file, is_multipart, limit, state).await?;
    // Flush the OS page cache to stable storage before the payload is renamed
    // into the store. The database row that follows says the package exists; if
    // a crash lands between the rename and the kernel's own writeback, that row
    // would point at a truncated or empty file.
    file.sync_all().await?;
    drop(file);
    Ok((temp, summary))
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
    } else {
        state.mirror_refresh(&id);
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
        state.mirror_version(&id, &version).await;
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
    } else {
        state.mirror_refresh(id);
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
    // The protocol's default is `false`, as for search: a client that does not
    // ask for pre-releases is not offered them.
    let include_prerelease = params.prerelease.unwrap_or(false);

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
    // The size and hash are computed while the bytes stream past, so recording
    // them costs nothing. Dropping them left the one number in the log that says
    // how much a symbol push actually cost, and any later question about which
    // bytes were stored, unanswerable.
    let (temp, summary) = receive_push(&state, request).await?;

    let indexing = state.clone();
    let result = detached(async move {
        symbols::index_symbol_package(
            indexing.storage.as_ref(),
            indexing.db.as_ref(),
            indexing.feed(),
            temp.path().to_path_buf(),
        )
        .await
    })
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
    let owner_version = parse_stored_version(&owner.normalized_version)?;
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
    // One query for the whole id rather than one per version, newest version
    // first, and only the versions this feed holds.
    let mut files = Vec::new();
    if state.config.files.enabled {
        let mut all = state.db.files_for_id(&id).await?;
        for fv in versions.iter().rev() {
            let v = fv.package.normalized_version();
            files.extend(all.extract_if(.., |f| f.normalized_version == v));
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
/// into it. Either way the version arrives with the listed and enabled state
/// it has here, and still pending if it is pending here: a copy or promotion
/// must not re-publish what this feed withholds (an admin who disabled a
/// broken build would otherwise see it go live in the next ring). A move
/// also keeps its pin, since it is the same version, elsewhere. A version the
/// target already holds keeps its state there. Its files are never touched:
/// the target holds them afterwards either way.
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
    let membership = Membership {
        pending: target.requires_approval || here.pending,
        flagged: outcome.violation.is_some(),
        flag_reason: outcome.violation,
        listed: here.listed,
        enabled: here.enabled,
        pinned: mode == Transfer::Move && here.pinned,
        ..Membership::active(&target.name, &package)
    };
    match state.db.add_membership(&membership).await {
        Ok(()) | Err(Error::PackageAlreadyExists) => {}
        Err(e) => return Err(e),
    }
    if mode == Transfer::Move {
        state.db.remove_membership(state.feed(), id, v).await?;
        // Moved out on purpose: this feed's mirror must not fetch it back.
        if let Err(e) = state.db.add_tombstone(state.feed(), id, v).await {
            tracing::error!(feed = %state.feed(), %id, error = %e, "could not record the move");
        }
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
/// half done, and flag changes are applied in one transaction. Nothing selected sends the admin back to the page unchanged.
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

    // Flag changes go through in one transaction, so a database error part
    // way leaves every version as it was. Deletes and transfers also move
    // files and take per-version locks, which a transaction cannot cover;
    // they are checked up front and then applied version by version.
    let flag = match op {
        BulkOp::Enable => Some(MembershipChange::Enabled(true)),
        BulkOp::Disable => Some(MembershipChange::Enabled(false)),
        BulkOp::Approve => Some(MembershipChange::Approve),
        BulkOp::Pin => Some(MembershipChange::Pinned(true)),
        BulkOp::Unpin => Some(MembershipChange::Pinned(false)),
        BulkOp::Delete | BulkOp::Copy | BulkOp::Move => None,
    };
    if let Some(change) = flag {
        state
            .db
            .update_memberships(state.feed(), &id, &versions, change)
            .await?;
    }

    match op {
        BulkOp::Enable | BulkOp::Disable | BulkOp::Approve | BulkOp::Pin | BulkOp::Unpin => {}
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
