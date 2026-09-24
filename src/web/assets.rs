//! The one static asset the gallery's stylesheet loads: its font.
//!
//! Atkinson Hyperlegible Next (SIL Open Font License 1.1, see `fonts/OFL.txt`),
//! the Latin subset of its variable build, is baked into the binary so the
//! gallery stays fully offline. It is served from the process root, outside
//! every feed, like the health probes — every feed's pages share the one file,
//! and a browser caches it once.

use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

use super::ui::FONT_URL;

const FONT: &[u8] = include_bytes!("fonts/atkinson-hyperlegible-next-2.001-latin-wght.woff2");

/// The asset routes, merged at the root next to the health probes.
pub fn routes() -> Router {
    Router::new().route(FONT_URL, get(font))
}

/// The font never changes under its URL (the name carries its version), so a
/// browser may keep it for a year without asking again.
async fn font() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, super::IMMUTABLE_CACHE),
        ],
        FONT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_font_is_a_woff2_under_the_route_the_stylesheet_names() {
        assert!(FONT.starts_with(b"wOF2"), "not a WOFF2 file");
        assert!(FONT_URL.starts_with("/_assets/"));
        // The file is cached as immutable, so its URL must name its version.
        assert!(FONT_URL.contains("2.001"), "{FONT_URL}");
    }
}
