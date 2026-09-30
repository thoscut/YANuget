//! The NuGet V3 protocol endpoints: the service index, the flat container
//! (package and manifest downloads), both registration hives, search,
//! autocomplete and symbol downloads, plus the health probes and the plain
//! landing page served when the gallery is off.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use serde::Deserialize;

use super::helpers::{check_id, is_semver2_level, parse_stored_version, parse_version};
use super::{files, ui, AppState};
use crate::database::SearchRequest;
use crate::error::{Error, Result};
use crate::nuget;
use crate::storage::{AuxFile, PackageContent};

const NUPKG_CONTENT_TYPE: &str = "application/octet-stream";

pub(super) const MAX_SEARCH_TAKE: i64 = 1000;

// ---------------------------------------------------------------------------
// Informational endpoints
// ---------------------------------------------------------------------------

/// Readiness: the process is up **and** its database answers.
///
/// Returns the historical plain `OK` body on success so existing probes keep
/// working, and `503` with a short reason when the store is unreachable — which
/// is the case an orchestrator has to be able to act on. `/health/live` is the
/// dependency-free liveness counterpart.
pub(super) async fn health(State(state): State<AppState>) -> Response {
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
pub(super) async fn health_live() -> &'static str {
    "OK"
}

pub(super) async fn index_page(State(state): State<AppState>, headers: HeaderMap) -> Html<String> {
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

pub(super) async fn service_index(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Json<serde_json::Value> {
    let urls = state.url_builder(&headers);
    Json(nuget::service_index(&urls, state.config.enable_web_ui))
}

// ---------------------------------------------------------------------------
// Flat container (package content)
// ---------------------------------------------------------------------------

pub(super) async fn package_versions(
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

pub(super) async fn download_package(
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
pub(super) async fn registration_index(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    registration_index_for(&state, &headers, &id, false).await
}

pub(super) async fn registration_index_semver2(
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

pub(super) async fn registration_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, lower, upper)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>> {
    registration_page_for(&state, &headers, &id, &lower, &upper, false).await
}

pub(super) async fn registration_page_semver2(
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

pub(super) async fn registration_leaf(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>> {
    registration_leaf_for(&state, &headers, &id, &version, false).await
}

pub(super) async fn registration_leaf_semver2(
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
pub(super) struct SearchParams {
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

pub(super) async fn search(
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
pub(super) struct AutocompleteParams {
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

pub(super) async fn autocomplete(
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

pub(super) async fn download_symbol(
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
