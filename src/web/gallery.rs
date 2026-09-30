//! The human-facing gallery: the package list and search, the package and
//! version pages, the embedded icon, and the tags, stats and settings pages.
//! The HTML itself is rendered by [`super::ui`].

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::forms::lenient;
use super::helpers::{check_id, parse_version};
use super::protocol::MAX_SEARCH_TAKE;
use super::{files, ui, AppState};
use crate::database::{SearchRequest, SearchSort};
use crate::error::{Error, Result};
use crate::storage::AuxFile;

/// The gallery's query string: a search, plus the pager's `page`.
///
/// Every value is taken as text and parsed with [`lenient`]. These are
/// addresses people bookmark and edit, so an empty or mistyped `?take=` shows
/// the default rather than a 400, which is what a typed field answered.
#[derive(Debug, Deserialize)]
pub(super) struct GalleryParams {
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

pub(super) async fn gallery(
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

pub(super) async fn tags_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>> {
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

pub(super) async fn settings_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>> {
    // The page carries no secrets, but it does describe the feed's policy,
    // mirror and retention posture — the same reconnaissance a gated feed is
    // withholding everywhere else.
    state.require_read(&headers)?;
    let urls = state.url_builder(&headers);
    Ok(Html(ui::settings_page(&urls, &state.config, &state.feed)))
}

pub(super) async fn stats_page(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>> {
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

pub(super) async fn package_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Html<String>> {
    render_detail(&state, &headers, &id, None).await
}

pub(super) async fn package_detail_version(
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
pub(super) async fn package_icon(
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
