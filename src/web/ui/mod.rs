//! Server-rendered HTML for the human-facing package gallery.
//!
//! These are pure functions that turn domain types into HTML strings, mirroring
//! how [`crate::nuget`] turns them into JSON. There is no template engine; HTML
//! is assembled with `format!` and **every** value derived from package data is
//! run through [`escape_html`] to prevent stored XSS. The escaping helpers,
//! and the rule for which one a sink needs, live in `escape`.
//!
//! The gallery is geared towards Chocolatey: the install snippet shown first is
//! the `choco install` command (configurable via `primary_client`).
//!
//! One module per page family:
//!
//! * `escape` — [`escape_html`], `safe_href`, `enc_path`
//! * `layout` — the chrome every page shares, the inline style and script,
//!   the [`CSP`] that pins them, and the error page
//! * `gallery` — the package list, paging, tags, and the multi-feed index
//! * `package` — the package page
//! * `stats`, `settings` — those two pages
//! * `admin` — the admin area's pages
//! * `format` — counts, sizes, truncation and key/value rows

mod admin;
mod escape;
#[cfg(test)]
mod fixtures;
mod format;
mod gallery;
mod layout;
mod package;
mod settings;
mod stats;

pub use admin::{
    admin_dashboard_page, admin_package_page, admin_retention_page, AdminPackageExtras,
    RetentionNotice, RetentionView, CSRF_FIELD,
};
pub use escape::escape_html;
pub use gallery::{feeds_index_page, gallery_page, tags_page, GalleryView, MAX_CLOUD_TAGS};
pub(in crate::web) use layout::FONT_URL;
pub use layout::{error_page, CSP};
pub use package::{detail_page, Detail};
pub use settings::settings_page;
pub use stats::stats_page;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::nuget::UrlBuilder;
    use crate::web::ui::fixtures::{feed_ctx, page_of, sample, view};
    use crate::web::ui::layout::STYLE;

    /// The level of every heading in `html`, in document order.
    fn heading_levels(html: &str) -> Vec<u8> {
        let bytes = html.as_bytes();
        (0..bytes.len().saturating_sub(3))
            .filter(|&i| {
                bytes[i] == b'<'
                    && bytes[i + 1] == b'h'
                    && (b'1'..=b'6').contains(&bytes[i + 2])
                    && matches!(bytes[i + 3], b'>' | b' ')
            })
            .map(|i| bytes[i + 2] - b'0')
            .collect()
    }

    #[test]
    fn every_page_outlines_without_skipping_a_heading_level() {
        // The stats, settings, package and first-run pages went from `<h1>`
        // straight to `<h3>`, which a screen reader presents as a section
        // missing its heading.
        let urls = UrlBuilder::new("https://host");
        let mut p = sample();
        p.dependencies = vec![crate::models::DependencyGroup {
            target_framework: Some("net8.0".into()),
            dependencies: vec![],
        }];
        let stats = crate::database::DatabaseStats {
            package_count: 1,
            version_count: 1,
            listed_count: 1,
            total_downloads: 1,
            total_size: 1,
            symbol_count: 0,
            ..Default::default()
        };
        let pages = [
            (
                "detail",
                detail_page(
                    &urls,
                    std::slice::from_ref(&p),
                    &p,
                    &Detail {
                        readme: Some("docs"),
                        primary_client: "choco",
                        has_symbols: false,
                        admin: false,
                        files: &[],
                    },
                ),
            ),
            (
                "stats",
                stats_page(&urls, &stats, &page_of(&["A"]), &[sample()], false),
            ),
            (
                "settings",
                settings_page(&urls, &Config::default(), &feed_ctx(None, Some("adm"))),
            ),
            (
                "first run",
                gallery_page(&urls, &page_of(&[]), &view("", 0, 20)),
            ),
            (
                "gallery",
                gallery_page(&urls, &page_of(&["A", "B"]), &view("", 0, 20)),
            ),
        ];
        for (name, html) in pages {
            let levels = heading_levels(&html);
            assert_eq!(levels.first(), Some(&1), "{name}: {levels:?}");
            for pair in levels.windows(2) {
                assert!(pair[1] <= pair[0] + 1, "{name} skips a level: {levels:?}");
            }
        }
    }

    #[test]
    fn counts_of_one_are_singular_and_steps_are_a_list() {
        let urls = UrlBuilder::new("https://host");
        let mut p = sample();
        p.downloads = 1;
        let html = detail_page(
            &urls,
            std::slice::from_ref(&p),
            &p,
            &Detail {
                readme: None,
                primary_client: "choco",
                has_symbols: false,
                admin: false,
                files: &[],
            },
        );
        // The label's count is a bare number under its caption; the version
        // list spells it out.
        assert!(
            html.contains("<div><dt>Downloads</dt><dd>1</dd></div>"),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"muted\">1 download</span>"),
            "{html}"
        );
        assert!(!html.contains("1 downloads"), "{html}");

        let mut page = page_of(&["A"]);
        page.groups[0].packages[0].downloads = 1;
        let html = gallery_page(&urls, &page, &view("", 0, 20));
        assert!(html.contains("1 version, 1 download"), "{html}");

        // The first-run steps are numbered by an ordered list, not by text in
        // each heading, and no label is set in capitals. The stylesheet draws
        // the numbers, so the list says it is one (Safari drops the role of a
        // list without markers).
        let html = gallery_page(&urls, &page_of(&[]), &view("", 0, 20));
        assert!(
            html.contains("<ol class=\"card steps\" role=\"list\"><li><h2>Add this feed</h2>"),
            "{html}"
        );
        assert!(STYLE.contains("counter-increment:step"));
        assert!(!STYLE.contains(".install h3{margin:14px 0 4px;font-size:13px;text-transform"));
        assert!(!STYLE.contains(".steps h3"));
    }
}
