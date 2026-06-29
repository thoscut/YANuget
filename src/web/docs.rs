//! Embedded, fully-offline documentation site served under `/docs`.
//!
//! The site is built with `mkdocs build` into `site/` and baked into the binary
//! at compile time via [`rust_embed`], so the running server serves its own help
//! with no filesystem dependency and no external network access. A `build.rs`
//! guarantees `site/` exists (writing a placeholder when mkdocs has not run), so
//! a plain `cargo build` works without Python.

use axum::extract::{OriginalUri, Path};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "site/"]
struct DocSite;

/// `/docs` → redirect to `/docs/` so the site's relative asset/page links
/// resolve correctly (prefix- and proxy-aware via the original request path).
pub async fn docs_root(OriginalUri(uri): OriginalUri) -> Response {
    Redirect::permanent(&format!("{}/", uri.path())).into_response()
}

/// `/docs/` → the documentation home page.
pub async fn docs_index() -> Response {
    serve("")
}

/// `/docs/{*path}` → a page or asset within the embedded site.
pub async fn serve_docs(Path(path): Path<String>) -> Response {
    serve(&path)
}

fn serve(rel: &str) -> Response {
    match resolve(rel) {
        Some((bytes, content_type)) => {
            ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
        }
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

/// Resolve a request path (relative to `/docs/`) to an embedded file, trying the
/// path itself and then its directory index (mkdocs `use_directory_urls`).
fn resolve(rel: &str) -> Option<(Vec<u8>, &'static str)> {
    let rel = rel.trim_matches('/');
    let candidates = if rel.is_empty() {
        vec!["index.html".to_string()]
    } else {
        vec![rel.to_string(), format!("{rel}/index.html")]
    };
    for key in candidates {
        if let Some(file) = DocSite::get(&key) {
            return Some((file.data.into_owned(), content_type_for(&key)));
        }
    }
    None
}

/// Map a file extension to a static content type. Covers everything a mkdocs
/// Material site emits; unknown types fall back to `application/octet-stream`.
fn content_type_for(path: &str) -> &'static str {
    let ext = path
        .rsplit('.')
        .next()
        .filter(|_| path.contains('.'))
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "eot" => "application/vnd.ms-fontobject",
        "txt" => "text/plain; charset=utf-8",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_cover_site_assets() {
        assert_eq!(content_type_for("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type_for("assets/main.abc123.css"),
            "text/css; charset=utf-8"
        );
        assert_eq!(
            content_type_for("assets/bundle.def.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for("assets/fonts/x.woff2"), "font/woff2");
        assert_eq!(
            content_type_for("search/search_index.json"),
            "application/json"
        );
        assert_eq!(content_type_for("noextension"), "application/octet-stream");
    }

    #[test]
    fn resolve_serves_root_index() {
        // build.rs guarantees at least a placeholder index.html is embedded.
        let (bytes, ct) = resolve("").expect("root index present");
        assert!(ct.starts_with("text/html"));
        assert!(!bytes.is_empty());
    }
}
