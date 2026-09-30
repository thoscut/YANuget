//! The HTTP layer: application state, routing and request handlers.
//!
//! Each hosted feed gets its own [`AppState`] (sharing the process-wide storage
//! and database) and its own router, mounted under the feed's path prefix. A
//! single unconfigured feed is served at the root, preserving the original
//! single-feed URLs.
//!
//! This file builds the routers and nothing else. The rest is split by
//! concern:
//!
//! * `state` — [`AppState`], [`FeedContext`], [`FeedMeta`]
//! * `middleware` — the global and per-feed layers (security headers, host
//!   and cross-site guard, forwarded-header filtering, HTML error pages)
//! * `protocol` — the NuGet V3 read endpoints and the health probes
//! * `publish` — push, symbol push, delete/unlist, relist
//! * `gallery` — the human-facing pages; their HTML is rendered by `ui`
//! * `admin` — the admin area, all of it behind one `route_layer`
//! * `hosted` — files attached to versions, and resumable (tus) uploads
//! * `files` — Range-aware file responses; `docs` and `assets` — embedded
//!   static content
//! * `forms`, `helpers` — form/query parsing and small shared helpers

mod admin;
mod assets;
mod docs;
mod files;
mod forms;
mod gallery;
mod helpers;
pub(crate) mod hosted;
mod middleware;
mod protocol;
mod publish;
mod state;
mod ui;

pub use hosted::sweep_expired_uploads;
pub use state::{AppState, FeedContext, FeedMeta};

use axum::response::Html;
use axum::routing::{delete, get, head, post, put};
use axum::Router;

use crate::nuget::UrlBuilder;

use admin::{
    admin_approve, admin_bulk, admin_dashboard, admin_delete, admin_disable, admin_enable,
    admin_file_delete, admin_gate, admin_package, admin_pin, admin_post, admin_promote,
    admin_retention, admin_retention_run, admin_unpin,
};
use gallery::{
    gallery, package_detail, package_detail_version, package_icon, settings_page, stats_page,
    tags_page,
};
use middleware::{apply_global_layers, gated_feed_headers, html_errors, GlobalLayers};
use protocol::{
    autocomplete, download_package, download_symbol, health, health_live, index_page,
    package_versions, registration_index, registration_index_semver2, registration_leaf,
    registration_leaf_semver2, registration_page, registration_page_semver2, search, service_index,
};
use publish::{delete_package, push_package, push_symbol_package, relist_package};

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
