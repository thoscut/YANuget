//! The statistics page.

use super::escape::{enc_path, escape_html};
use super::format::{group_digits, human_size, plural};
use super::layout::{layout, Nav};
use crate::models::Package;
use crate::nuget::UrlBuilder;

/// The statistics page: feed-wide totals, the most-downloaded packages, and the
/// most recently published versions.
pub fn stats_page(
    urls: &UrlBuilder,
    stats: &crate::database::DatabaseStats,
    top: &crate::database::SearchPage,
    recent: &[Package],
    admin: bool,
) -> String {
    let cards = [
        (group_digits(stats.package_count), "Packages"),
        (group_digits(stats.version_count), "Versions"),
        (group_digits(stats.listed_count), "Listed versions"),
        (group_digits(stats.total_downloads), "Downloads"),
        (
            human_size(stats.total_size.max(0) as u64),
            "Package storage",
        ),
        (group_digits(stats.file_count), "Attached files"),
        (human_size(stats.file_bytes.max(0) as u64), "File storage"),
        (group_digits(stats.symbol_count), "Symbol files"),
    ];
    // The same ruled cells as the package label: a caption over each value.
    let mut tiles = String::from("<div class=\"stats\">");
    for (n, l) in cards {
        tiles.push_str(&format!(
            "<div class=\"stat\"><div class=\"l\">{}</div><div class=\"n\">{}</div></div>",
            l,
            escape_html(&n),
        ));
    }
    tiles.push_str("</div>");

    let top_list = if top.groups.is_empty() {
        "<p class=\"muted\">No packages yet.</p>".to_string()
    } else {
        let mut out = String::from("<ul class=\"rank\">");
        for g in &top.groups {
            // The version a client would be offered, as in the gallery.
            let p = g.headline();
            out.push_str(&format!(
                "<li><span><a class=\"id\" href=\"{href}\">{id}</a> \
                 <span class=\"muted\">{ver}</span></span>\
                 <span class=\"muted\">{dl} download{ds}</span></li>",
                href = escape_html(&urls.app(&format!("/packages/{}", enc_path(&p.lower_id())))),
                id = escape_html(&p.id),
                ver = escape_html(&p.normalized_version()),
                dl = group_digits(g.total_downloads() as i64),
                ds = plural(g.total_downloads() as i64),
            ));
        }
        out.push_str("</ul>");
        out
    };

    let recent_list = if recent.is_empty() {
        "<p class=\"muted\">No packages yet.</p>".to_string()
    } else {
        let mut out = String::from("<ul class=\"rank\">");
        for p in recent {
            out.push_str(&format!(
                "<li><span><a class=\"id\" href=\"{href}\">{id}</a> \
                 <span class=\"muted\">{dv}</span></span>\
                 <span class=\"muted\">{when}</span></li>",
                href = escape_html(&urls.app(&format!(
                    "/packages/{}/{}",
                    enc_path(&p.lower_id()),
                    enc_path(&p.normalized_version())
                ))),
                id = escape_html(&p.id),
                dv = escape_html(&p.normalized_version()),
                when = escape_html(&p.published.format("%Y-%m-%d").to_string()),
            ));
        }
        out.push_str("</ul>");
        out
    };

    let body = format!(
        "<h1 class=\"title\">Statistics</h1>{tiles}\
         <div class=\"lists\">\
         <div class=\"card\"><h2>Most downloaded</h2>{top_list}</div>\
         <div class=\"card\"><h2>Recently published</h2>{recent_list}</div>\
         </div>"
    );
    layout(
        urls,
        "Statistics \u{2014} YANuget",
        Nav {
            active: "stats",
            admin,
        },
        &body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::ui::fixtures::{page_of, sample};
    use crate::web::ui::layout::STYLE;

    #[test]
    fn stats_page_renders_totals_and_lists() {
        let urls = UrlBuilder::new("https://host");
        let stats = crate::database::DatabaseStats {
            package_count: 2,
            version_count: 5,
            listed_count: 4,
            total_downloads: 1234,
            total_size: 2048,
            symbol_count: 1,
            file_count: 2,
            file_bytes: 3 * 1024 * 1024 * 1024,
        };
        let html = stats_page(&urls, &stats, &page_of(&["Top.Pkg"]), &[sample()], false);
        assert!(html.contains("Statistics"));
        assert!(html.contains("1,234")); // grouped downloads
        assert!(html.contains("Top.Pkg"));
        assert!(html.contains("Recently published"));
        // Eight tiles in rows of four (two on narrow screens), never a row
        // with an orphan; the two lists share the width evenly, not the
        // package page's `1fr 340px` split.
        assert_eq!(html.matches("class=\"stat\"").count(), 8);
        assert!(STYLE.contains(".stats{display:grid;grid-template-columns:repeat(4,1fr)"));
        assert!(
            html.contains("<div class=\"l\">File storage</div><div class=\"n\">3.0 GB</div>"),
            "{html}"
        );
        assert!(
            html.contains("<div class=\"lists\"><div class=\"card\">"),
            "{html}"
        );
        assert!(!html.contains("class=\"grid\""), "{html}");
    }
}
