//! The package list: search results, paging, sort and tag filters, the
//! first-run panel, the tag cloud, and the multi-feed index at the root.

use super::escape::{enc_path, escape_html};
use super::format::{group_digits, plural, truncate};
use super::layout::{layout, layout_with_chrome, Chrome, Nav};
use super::package::{command_html, render_tags};
use crate::database::SearchSort;
use crate::database::TagCount;
use crate::nuget::UrlBuilder;

/// What the gallery was asked to show: the search, the page, the order, and the
/// filters that every paging link and form has to carry.
#[derive(Debug, Clone, Copy, Default)]
pub struct GalleryView<'a> {
    pub query: &'a str,
    pub skip: i64,
    pub take: i64,
    /// The configured page size (`gallery_page_size`).
    pub default_take: i64,
    pub prerelease: Option<bool>,
    pub package_type: Option<&'a str>,
    pub sort: SearchSort,
    /// Only packages with this (lower-cased) tag.
    pub tag: Option<&'a str>,
    /// The feed's most used tags, offered above the list on the landing page.
    pub popular: &'a [TagCount],
    /// Whether this feed has an admin area, for the header's link to it.
    pub admin: bool,
}

impl GalleryView<'_> {
    /// The gallery URL of the page starting at `skip`, escaped for an
    /// attribute, carrying the search, the page size, the order and the
    /// filters.
    fn href(&self, urls: &UrlBuilder, skip: i64) -> String {
        self.href_sorted(urls, skip, self.sort)
    }

    /// [`Self::href`] in another order. The default order is left out of the
    /// URL, so the addresses people already bookmarked keep meaning the same.
    fn href_sorted(&self, urls: &UrlBuilder, skip: i64, sort: SearchSort) -> String {
        // `take` has to be carried, or paging silently changes the page size
        // back to the default: `?take=5` showed "1–5 of N", and Next then
        // returned twenty items while the counter still claimed five. And
        // `&` is `&amp;` inside an HTML attribute — a bare one is only
        // tolerated because no entity name follows it here.
        let mut href = format!(
            "{}?q={}&amp;skip={skip}&amp;take={}",
            escape_html(&urls.app("/packages")),
            enc_path(self.query),
            self.take
        );
        if let Some(pre) = self.prerelease {
            href.push_str(&format!("&amp;prerelease={pre}"));
        }
        if let Some(ty) = self.package_type {
            href.push_str(&format!("&amp;packageType={}", enc_path(ty)));
        }
        if let Some(tag) = self.tag {
            href.push_str(&format!("&amp;tag={}", enc_path(tag)));
        }
        if sort != SearchSort::default() {
            href.push_str(&format!("&amp;sort={}", sort.as_str()));
        }
        href
    }

    /// Hidden inputs carrying the search, the order and the filters into a GET
    /// form. They have no ids: the header's search box already owns `id="q"`.
    fn hidden_fields(&self) -> String {
        let mut out = format!(
            "<input type=\"hidden\" name=\"q\" value=\"{}\">",
            escape_html(self.query)
        );
        if let Some(pre) = self.prerelease {
            out.push_str(&format!(
                "<input type=\"hidden\" name=\"prerelease\" value=\"{pre}\">"
            ));
        }
        if let Some(ty) = self.package_type {
            out.push_str(&format!(
                "<input type=\"hidden\" name=\"packageType\" value=\"{}\">",
                escape_html(ty)
            ));
        }
        out.push_str(&tag_field(self.tag));
        out.push_str(&sort_field(self.sort));
        out
    }
}

/// A hidden `sort` input, or nothing for the default order.
fn sort_field(sort: SearchSort) -> String {
    if sort == SearchSort::default() {
        String::new()
    } else {
        format!(
            "<input type=\"hidden\" name=\"sort\" value=\"{}\">",
            sort.as_str()
        )
    }
}

/// A hidden `tag` input, or nothing without a tag filter.
fn tag_field(tag: Option<&str>) -> String {
    match tag {
        Some(tag) => format!(
            "<input type=\"hidden\" name=\"tag\" value=\"{}\">",
            escape_html(tag)
        ),
        None => String::new(),
    }
}

/// The gallery filtered to one tag, from its first page.
pub(super) fn tag_href(urls: &UrlBuilder, tag: &str) -> String {
    format!(
        "{}?tag={}",
        escape_html(&urls.app("/packages")),
        enc_path(&tag.to_lowercase())
    )
}

/// The orders the gallery offers, as links: one click, no form, and each one
/// starts again from the first page, since page three of another order is a
/// different set of packages.
fn sort_links(urls: &UrlBuilder, view: &GalleryView) -> String {
    let mut links = String::new();
    for (sort, label) in [
        (SearchSort::Downloads, "Downloads"),
        (SearchSort::Name, "Name"),
        (SearchSort::Updated, "Recently updated"),
    ] {
        let current = if sort == view.sort {
            " aria-current=\"true\""
        } else {
            ""
        };
        links.push_str(&format!(
            "<a href=\"{}\"{current}>{label}</a>",
            view.href_sorted(urls, 0, sort)
        ));
    }
    format!(
        "<nav class=\"sort\" aria-label=\"Sort order\">Sort by <span class=\"seg\">{links}</span></nav>"
    )
}

/// The landing page's way in by tag: the most used tags, and the whole cloud.
fn popular_tags(urls: &UrlBuilder, tags: &[TagCount]) -> String {
    if tags.len() < 2 {
        return String::new();
    }
    let links: String = tags
        .iter()
        .map(|t| {
            format!(
                "<a class=\"tag\" href=\"{}\">{}</a>",
                tag_href(urls, &t.tag),
                escape_html(&t.tag)
            )
        })
        .collect();
    format!(
        "<nav class=\"popular\" aria-label=\"Popular tags\"><span>Popular tags</span>{links}\
         <a class=\"all\" href=\"{}\">All tags</a></nav>",
        escape_html(&urls.app("/tags"))
    )
}

/// The gallery / search-results page.
pub fn gallery_page(
    urls: &UrlBuilder,
    page: &crate::database::SearchPage,
    view: &GalleryView,
) -> String {
    let view = GalleryView {
        skip: view.skip.max(0),
        take: view.take.max(1),
        ..*view
    };
    let query = view.query;
    let searching = !query.trim().is_empty();
    let pages = (page.total_hits + view.take - 1) / view.take;
    let current = view.skip / view.take + 1;
    // The same search and order without the tag: what "clear the tag" means.
    let untagged = GalleryView { tag: None, ..view };
    let tagged = view
        .tag
        .map(|t| format!(" tagged \u{201c}{}\u{201d}", escape_html(t)))
        .unwrap_or_default();
    let body = if page.groups.is_empty() {
        let browse_all = escape_html(&urls.app("/packages"));
        if page.total_hits > 0 {
            // Empty page, matches exist: `skip` is past the end. That happens
            // from a bookmarked link, a hand-edited URL, or a `skip` that was
            // valid until a delete or a retention sweep shortened the list.
            // Answering it with the onboarding panel told an operator with
            // thousands of packages that their feed was empty. It is checked
            // before the search case, which said a search with four matches
            // had none.
            // With a single page, the last page is the first: one link, not two
            // that lead to the same place.
            let last = if pages > 1 {
                format!(
                    "<p><a href=\"{}\">Go to the last page ({pages})</a></p>",
                    view.href(urls, (pages - 1) * view.take)
                )
            } else {
                String::new()
            };
            format!(
                "<div class=\"empty\"><h1 class=\"title\">There is nothing on this page</h1>\
                 {last}<p><a href=\"{first}\">Back to the first page</a></p></div>",
                first = view.href(urls, 0),
            )
        } else if view.tag.is_some() {
            let heading = if searching {
                format!(
                    "No packages match \u{201c}{}\u{201d}{tagged}",
                    escape_html(query)
                )
            } else {
                format!("No packages{tagged}")
            };
            format!(
                "<div class=\"empty\"><h1 class=\"title\">{heading}</h1>\
                 <p><a href=\"{clear}\">Show them without the tag</a></p>\
                 <p><a href=\"{all}\">See every tag</a></p></div>",
                clear = untagged.href(urls, 0),
                all = escape_html(&urls.app("/tags")),
            )
        } else if searching {
            format!(
                "<div class=\"empty\"><h1 class=\"title\">No packages match \u{201c}{}\u{201d}</h1>\
                 <p><a href=\"{browse_all}\">Clear search and browse all packages</a></p></div>",
                escape_html(query),
            )
        } else {
            first_run_panel(urls)
        }
    } else {
        let mut rows = String::new();
        // A real `<h1>`, not a muted paragraph: this is the landing page, and
        // without one a screen reader announces no page heading at all — while
        // the *empty* state did have one, so the structure changed with the
        // content.
        let heading = if searching {
            format!(
                "{} result{} for \u{201c}{}\u{201d}{tagged}",
                page.total_hits,
                plural(page.total_hits),
                escape_html(query)
            )
        } else {
            format!(
                "{} package{}{tagged}",
                page.total_hits,
                plural(page.total_hits)
            )
        };
        // One package has no order to choose.
        let sort = if page.total_hits > 1 {
            sort_links(urls, &view)
        } else {
            String::new()
        };
        let filter = match view.tag {
            Some(_) => format!(
                "<p class=\"filter\"><a href=\"{}\">Clear the tag</a> \
                 <a href=\"{}\">See every tag</a></p>",
                untagged.href(urls, 0),
                escape_html(&urls.app("/tags")),
            ),
            None => String::new(),
        };
        rows.push_str(&format!(
            "<div class=\"bar\"><h1 class=\"title\">{heading}</h1>{sort}</div>{filter}{popular}\
             <ul class=\"manifest\">",
            popular = popular_tags(urls, view.popular),
        ));
        for group in &page.groups {
            // The newest *stable* version, matching what a NuGet client
            // searching this feed is offered.
            let p = group.headline();
            let id = escape_html(&p.id);
            let url = escape_html(&urls.app(&format!("/packages/{}", enc_path(&p.lower_id()))));
            let pre = if p.is_prerelease() {
                " <span class=\"badge pre\">prerelease</span>"
            } else {
                ""
            };
            let desc = if p.description.trim().is_empty() {
                String::new()
            } else {
                format!("<p>{}</p>", escape_html(&truncate(&p.description, 240)))
            };
            let authors = if p.authors.is_empty() {
                String::new()
            } else {
                format!("<div>by {}</div>", escape_html(&p.authors.join(", ")))
            };
            rows.push_str(&format!(
                "<li class=\"pkg\"><h2><a href=\"{url}\">{id}</a> \
                 <span class=\"ver\">{ver}</span>{pre}</h2>{desc}\
                 <div class=\"figs\"><div>{nv} version{vs}, {dl} download{ds}</div>{authors}</div>\
                 {tags}</li>",
                ver = escape_html(&p.normalized_version()),
                nv = group.packages.len(),
                vs = plural(group.packages.len() as i64),
                dl = group_digits(group.total_downloads() as i64),
                ds = plural(group.total_downloads() as i64),
                tags = render_tags(urls, &p.tags),
            ));
        }
        rows.push_str("</ul>");
        rows.push_str(&pager(
            urls,
            &view,
            page.groups.len() as i64,
            page.total_hits,
        ));
        rows
    };
    // Searches, tags and later pages get titles of their own. Otherwise every
    // search, and every page of one, shares one <title>, so tabs, bookmarks and
    // history entries look identical.
    let search = match (searching, view.tag) {
        (true, None) => Some(format!("Search: \u{201c}{query}\u{201d}")),
        (true, Some(t)) => Some(format!(
            "Search: \u{201c}{query}\u{201d}, tagged \u{201c}{t}\u{201d}"
        )),
        (false, Some(t)) => Some(format!("Tagged \u{201c}{t}\u{201d}")),
        (false, None) => None,
    };
    let later_page = current > 1 && !page.groups.is_empty();
    let title = match (search, later_page) {
        (None, false) => "YANuget".to_string(),
        (None, true) => format!("Packages, page {current} of {pages} \u{2014} YANuget"),
        (Some(s), false) => format!("{s} \u{2014} YANuget"),
        (Some(s), true) => format!("{s}, page {current} of {pages} \u{2014} YANuget"),
    };
    // A new search starts on page one, but keeps a page size, a tag and an
    // order someone chose.
    let mut search_hidden = if view.take != view.default_take.max(1) {
        format!(
            "<input type=\"hidden\" name=\"take\" value=\"{}\">",
            view.take
        )
    } else {
        String::new()
    };
    search_hidden.push_str(&tag_field(view.tag));
    search_hidden.push_str(&sort_field(view.sort));
    layout_with_chrome(
        urls,
        &title,
        query,
        Nav {
            admin: view.admin,
            ..Nav::default()
        },
        &body,
        Chrome::Feed,
        &search_hidden,
    )
}

/// How many tags the tag page shows at most.
pub const MAX_CLOUD_TAGS: i64 = 300;

/// The tag cloud: every tag of the feed's visible packages, alphabetically,
/// set larger the more packages carry it.
///
/// Size is never the only signal: each tag carries its count, so the page
/// reads the same to a screen reader and to someone who cannot tell 18 px
/// from 22 px. Five steps on a log scale, because a feed's tag counts are
/// long-tailed — a linear scale makes one tag huge and every other one tiny.
pub fn tags_page(urls: &UrlBuilder, tags: &[TagCount], admin: bool) -> String {
    let body = if tags.is_empty() {
        "<div class=\"empty\"><h1 class=\"title\">No tags yet</h1>\
         <p>Packages get their tags from the <code>&lt;tags&gt;</code> element of their \
         <code>.nuspec</code>.</p></div>"
            .to_string()
    } else {
        let most = tags.iter().map(|t| t.packages).max().unwrap_or(1).max(1);
        // While counts are small, a step per package reads truer than a log
        // scale: with counts of 1 and 2 the log puts the 2 at the largest size.
        let step = |n: i64| -> usize {
            if most <= 5 {
                return n.clamp(1, 5) as usize;
            }
            let ratio = (n.max(1) as f64).ln() / (most as f64).ln();
            1 + (ratio * 4.0).round().clamp(0.0, 4.0) as usize
        };
        let mut sorted: Vec<&TagCount> = tags.iter().collect();
        sorted.sort_by(|a, b| a.tag.cmp(&b.tag));
        let items: String = sorted
            .iter()
            .map(|t| {
                format!(
                    "<li><a class=\"t{step}\" href=\"{href}\">{tag}</a>\
                     <span class=\"n\">{n}<span class=\"vh\"> package{s}</span></span></li>",
                    step = step(t.packages),
                    href = tag_href(urls, &t.tag),
                    tag = escape_html(&t.tag),
                    n = group_digits(t.packages),
                    s = plural(t.packages),
                )
            })
            .collect();
        let capped = if tags.len() as i64 >= MAX_CLOUD_TAGS {
            format!(" The {MAX_CLOUD_TAGS} most used are shown.")
        } else {
            String::new()
        };
        format!(
            "<h1 class=\"title\">Tags</h1>\
             <p class=\"muted\">{n} tag{s} on this feed's packages; the larger, the more \
             packages carry it.{capped}</p>\
             <ul class=\"cloud\">{items}</ul>",
            n = tags.len(),
            s = plural(tags.len() as i64),
        )
    };
    layout(
        urls,
        "Tags \u{2014} YANuget",
        Nav {
            active: "tags",
            admin,
        },
        &body,
    )
}

/// What an empty feed shows instead of "no packages": the three commands that
/// take someone from a running server to a restored package, already carrying
/// this server's own service-index URL.
///
/// This is the first page most people ever see, and the thing they need at that
/// moment is not an apology for being empty — it is the URL to point a client
/// at, which they would otherwise have to go and find.
fn first_run_panel(urls: &UrlBuilder) -> String {
    // Assembled raw and escaped once at output, like `render_install`: escaping
    // twice would put a literal `&amp;` on the clipboard.
    let idx = urls.service_index();
    let steps = [
        (
            "Add this feed",
            format!("dotnet nuget add source {idx} -n yanuget"),
        ),
        (
            "Push a package",
            "dotnet nuget push MyPackage.1.0.0.nupkg --source yanuget --api-key <your-api-key>"
                .to_string(),
        ),
        ("Restore from it", format!("dotnet restore --source {idx}")),
    ];

    // A real ordered list: these are steps, so the numbers are the list's own
    // rather than text in each heading. The stylesheet draws the numbers
    // itself, and a list without markers stops being announced as a list in
    // Safari unless it says so.
    let mut snippets = String::new();
    for (label, cmd) in steps {
        snippets.push_str(&format!(
            "<li><h2>{label}</h2><div class=\"snip\">\
             <button type=\"button\" class=\"copy\" aria-label=\"Copy the command to {what}\" \
             hidden>Copy</button>\
             <pre><code>{cmd}</code></pre></div></li>",
            what = label.to_lowercase(),
            cmd = command_html(&cmd),
        ));
    }

    format!(
        "<div class=\"hero\"><h1>Your feed is live</h1>\
         <p>Nothing published to it yet. Three commands change that.</p></div>\
         <ol class=\"card steps\" role=\"list\">{snippets}</ol>\
         <p class=\"muted\">Using Chocolatey, <code>nuget.exe</code> or Visual Studio? \
         The same service-index URL works for all of them \u{2014} see \
         <a href=\"{docs}\">the documentation</a>.</p>",
        docs = escape_html(&urls.app("/docs/")),
    )
}

/// The page sizes the pager offers: a fixed ladder, plus the configured default
/// and the size in use, so the select always shows the real size. There is no
/// "all": `take` is capped at 1000.
fn page_sizes(take: i64, default_take: i64) -> Vec<i64> {
    let mut sizes = vec![20, 50, 100, take, default_take.max(1)];
    sizes.sort_unstable();
    sizes.dedup();
    sizes
}

/// Pagination for the gallery: previous/next links around the range shown,
/// and two small forms, one to go to a page and one to change the page size.
///
/// Both are plain GET forms, so they work without JavaScript and need nothing
/// the CSP would have to allow. "Go to page" sends `page`. The page-size form
/// sends the current `skip`, which the handler snaps to the start of the page
/// holding it at the new size, so the first package on screen stays there.
fn pager(urls: &UrlBuilder, view: &GalleryView, shown: i64, total: i64) -> String {
    let (skip, take) = (view.skip, view.take);
    let sizes = page_sizes(take, view.default_take);
    // Paging needs a second page. A page size is worth offering whenever it
    // would change what is shown: without that, choosing 100 on a feed of 60
    // left no way back to 20 a page.
    let paged = total > take || skip > 0;
    let sizable = total > sizes[0];
    if !paged && !sizable {
        return String::new();
    }
    let hidden = view.hidden_fields();
    let action = escape_html(&urls.app("/packages"));
    let mut out = String::from("<nav class=\"pager\" aria-label=\"Pagination\">");
    if paged {
        let link = |target: i64, enabled: bool, label: &str| {
            if enabled {
                format!(
                    "<a class=\"btn\" href=\"{}\">{label}</a>",
                    view.href(urls, target)
                )
            } else {
                // A link without an href, still announced as one, and disabled.
                format!("<a class=\"btn\" role=\"link\" aria-disabled=\"true\">{label}</a>")
            }
        };
        let from = if shown == 0 { 0 } else { skip + 1 };
        out.push_str(&format!(
            "{prev}<span class=\"muted\">{from}\u{2013}{to} of {total}</span>{next}",
            prev = link(
                (skip - take).max(0),
                skip > 0,
                "<span aria-hidden=\"true\">\u{2190}</span> Previous"
            ),
            next = link(
                skip + take,
                skip + shown < total,
                "Next <span aria-hidden=\"true\">\u{2192}</span>"
            ),
            to = skip + shown,
        ));
    }
    out.push_str("<div class=\"pager-go\">");
    if paged {
        let pages = (total + take - 1) / take;
        let current = skip / take + 1;
        out.push_str(&format!(
            "<form method=\"get\" action=\"{action}\">{hidden}\
             <input type=\"hidden\" name=\"take\" value=\"{take}\">\
             <label for=\"pg-page\">Page</label>\
             <input id=\"pg-page\" name=\"page\" type=\"number\" inputmode=\"numeric\" min=\"1\" \
             max=\"{pages}\" value=\"{current}\" required aria-describedby=\"pg-of\">\
             <span id=\"pg-of\" class=\"muted\">of {pages}</span>\
             <button type=\"submit\">Go<span class=\"vh\"> to page</span></button></form>"
        ));
    }
    if sizable {
        let options: String = sizes
            .iter()
            .map(|n| {
                let sel = if *n == take { " selected" } else { "" };
                format!("<option value=\"{n}\"{sel}>{n}</option>")
            })
            .collect();
        out.push_str(&format!(
            "<form method=\"get\" action=\"{action}\">{hidden}\
             <input type=\"hidden\" name=\"skip\" value=\"{skip}\">\
             <label for=\"pg-take\">Per page</label>\
             <select id=\"pg-take\" name=\"take\">{options}</select>\
             <button type=\"submit\">Apply<span class=\"vh\"> page size</span></button></form>"
        ));
    }
    out.push_str("</div></nav>");
    out
}

/// The root feed index, shown when more than one feed is hosted. Each entry is
/// `(feed name, path prefix)` where the prefix is e.g. `/stable`.
///
/// Only feeds anyone may read are listed. A read-gated feed's name is itself
/// something its key withholds (`internal-security-fixes` says plenty), and
/// its users already have its address; `some_hidden` adds one line saying
/// such feeds exist, without naming or counting them.
pub fn feeds_index_page(feeds: &[(String, String)], some_hidden: bool) -> String {
    let urls = UrlBuilder::new("");
    let mut list = String::from("<ul class=\"rank\">");
    for (name, prefix) in feeds {
        list.push_str(&format!(
            "<li><a href=\"{prefix}\">{name}</a> \
             <span class=\"muted\"><a href=\"{prefix}/v3/index.json\">service index</a></span></li>",
            prefix = escape_html(prefix),
            name = escape_html(name),
        ));
    }
    list.push_str("</ul>");
    let hidden = if some_hidden {
        "<p class=\"muted\">Feeds that require credentials are not listed here; \
         ask whoever runs this server for their address.</p>"
    } else {
        ""
    };
    let body = format!(
        "<h1 class=\"title\">Feeds</h1>\
         <p class=\"muted\">This server hosts several NuGet feeds. Pick one:</p>\
         <div class=\"card\">{list}</div>{hidden}"
    );
    layout_with_chrome(
        &urls,
        "Feeds \u{2014} YANuget",
        "",
        Nav::default(),
        &body,
        Chrome::Root,
        "",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::ui::fixtures::{page_of, sample, text_of, view};

    #[test]
    fn gallery_lists_cards_and_paginates() {
        let urls = UrlBuilder::new("https://host");
        // Two of three results shown -> pager with a Next link.
        let mut page = page_of(&["Pkg.A", "Pkg.B"]);
        page.total_hits = 3;
        let html = gallery_page(&urls, &page, &view("", 0, 2));
        assert!(html.contains("Pkg.A"));
        assert!(html.contains("/packages/pkg.b"));
        assert!(html.contains("class=\"pager\""));
        assert!(html.contains("skip=2")); // next page

        // Empty result for a query offers a "clear search" link.
        let empty = gallery_page(&urls, &page_of(&[]), &view("zzz", 0, 20));
        assert!(empty.contains("Clear search"));
    }

    #[test]
    fn the_feed_index_offers_only_links_that_exist_at_the_root() {
        // In multi-feed mode the root router mounts `/`, `/health` and nothing
        // else — every feed route lives under `/{name}`. The shared chrome
        // pointed the search form and three footer links at feed routes, so the
        // first page a visitor saw had a search box returning a bare 404.
        let html = feeds_index_page(
            &[
                ("stable".into(), "/stable".into()),
                ("dev".into(), "/dev".into()),
            ],
            false,
        );
        assert!(
            !html.contains("<form"),
            "no search form at the root: {html}"
        );
        for dead in [
            "/packages",
            "/stats",
            "/settings",
            "/docs/",
            "/v3/index.json\"",
        ] {
            assert!(
                !html.contains(&format!("\"{dead}")),
                "root page links {dead}, which is not mounted there: {html}"
            );
        }
        // The feed links themselves are the point of the page.
        assert!(html.contains("href=\"/stable\""), "{html}");
        assert!(html.contains("href=\"/dev/v3/index.json\""), "{html}");
    }

    #[test]
    fn the_gallery_headlines_the_version_a_client_would_offer() {
        // `/v3/search` excludes pre-releases unless asked, so headlining the
        // newest version outright made the card — and the install command under
        // its copy button — offer `2.0.0-beta` while Visual Studio showed
        // `1.9.0`.
        let urls = UrlBuilder::new("https://host");
        let versioned = |v: &str| {
            let mut p = sample();
            p.id = "Mixed".into();
            p.version = crate::version::NuGetVersion::parse(v).unwrap();
            p
        };
        let mut group = crate::database::SearchGroup {
            packages: vec![versioned("1.9.0"), versioned("2.0.0-beta")],
            total_downloads: 0,
        };
        assert_eq!(group.latest().normalized_version(), "2.0.0-beta");
        assert_eq!(group.headline().normalized_version(), "1.9.0");

        // A package that has only ever shipped pre-releases still shows one.
        group.packages = vec![versioned("0.1.0-alpha")];
        assert_eq!(group.headline().normalized_version(), "0.1.0-alpha");

        let page = crate::database::SearchPage {
            groups: vec![crate::database::SearchGroup {
                packages: vec![versioned("1.9.0"), versioned("2.0.0-beta")],
                total_downloads: 0,
            }],
            total_hits: 1,
        };
        let html = gallery_page(&urls, &page, &view("", 0, 20));
        assert!(html.contains("1.9.0"), "{html}");
    }

    #[test]
    fn the_landing_page_has_a_heading_and_searches_have_distinct_titles() {
        let urls = UrlBuilder::new("https://host");
        let mut page = page_of(&["A"]);
        page.total_hits = 1;
        let html = gallery_page(&urls, &page, &view("", 0, 20));
        assert!(html.contains("<h1"), "no h1 on the landing page: {html}");
        // "1 package", not "1 package(s)".
        assert!(html.contains("1 package<"), "{html}");
        assert!(html.contains("<title>YANuget</title>"), "{html}");

        let searched = gallery_page(&urls, &page, &view("logging", 0, 20));
        assert!(searched.contains("<title>Search:"), "{searched}");
        assert!(searched.contains("logging"), "{searched}");
    }

    #[test]
    fn an_empty_page_of_a_non_empty_feed_is_not_the_onboarding_panel() {
        // A bookmarked `skip`, or one that outlived a delete, lands here. The
        // onboarding panel told an operator with a full feed it was empty.
        let urls = UrlBuilder::new("https://host");
        let mut page = page_of(&[]);
        page.total_hits = 5000;
        let html = gallery_page(&urls, &page, &view("", 99_999, 20));
        assert!(!html.contains("Your feed is live"), "{html}");
        assert!(html.contains("nothing on this page"), "{html}");
        assert!(html.contains("Back to the first page"), "{html}");
        assert!(
            html.contains("skip=4980&amp;take=20\">Go to the last page (250)</a>"),
            "{html}"
        );
    }

    #[test]
    fn a_search_past_its_last_page_is_not_a_search_without_matches() {
        // `?q=git&skip=100` on a feed where git has four matches said "No
        // packages match". The way back keeps the search and the page size.
        let urls = UrlBuilder::new("https://host");
        let mut page = page_of(&[]);
        page.total_hits = 4;
        let html = gallery_page(&urls, &page, &view("git", 100, 2));
        assert!(!html.contains("No packages match"), "{html}");
        assert!(
            html.contains("<h1 class=\"title\">There is nothing on this page</h1>"),
            "{html}"
        );
        assert!(
            html.contains("?q=git&amp;skip=2&amp;take=2\">Go to the last page (2)</a>"),
            "{html}"
        );
        assert!(
            html.contains("?q=git&amp;skip=0&amp;take=2\">Back to the first page</a>"),
            "{html}"
        );

        // With one page of matches the last page is the first: one link.
        let html = gallery_page(&urls, &page, &view("git", 100, 20));
        assert!(!html.contains("Go to the last page"), "{html}");
        assert_eq!(html.matches("Back to the first page").count(), 1, "{html}");
    }

    #[test]
    fn paging_keeps_the_page_size_and_escapes_the_separator() {
        let urls = UrlBuilder::new("https://host");
        let mut page = page_of(&["A", "B"]);
        page.total_hits = 40;
        let html = gallery_page(&urls, &page, &view("", 0, 5));
        // Carrying `take` is what keeps the "1-5 of 40" counter honest on the
        // next page.
        assert!(html.contains("skip=5&amp;take=5"), "{html}");
        // A bare `&` in an attribute is invalid HTML.
        assert!(!html.contains("?q=&skip="), "{html}");
    }

    #[test]
    fn the_pager_offers_a_page_to_go_to_and_a_page_size() {
        let urls = UrlBuilder::new("https://host");
        let mut page = page_of(&["A", "B", "C", "D", "E"]);
        page.total_hits = 42;
        // The third page at five a page, on a server whose default is seven.
        let html = gallery_page(
            &urls,
            &page,
            &GalleryView {
                query: "a\"b<c",
                skip: 10,
                take: 5,
                default_take: 7,
                ..Default::default()
            },
        );
        assert!(html.contains("11\u{2013}15 of 42"), "{html}");
        // One pager, holding two plain GET forms back to the gallery.
        assert_eq!(html.matches("class=\"pager\"").count(), 1, "{html}");
        let form = "<form method=\"get\" action=\"/packages\">";
        assert_eq!(html.matches(form).count(), 2, "{html}");
        assert!(
            html.contains("max=\"9\" value=\"3\" required aria-describedby=\"pg-of\""),
            "{html}"
        );
        assert!(
            html.contains("<span id=\"pg-of\" class=\"muted\">of 9</span>"),
            "{html}"
        );
        // The size form sends the offset on screen, for the server to snap.
        assert!(html.contains("name=\"skip\" value=\"10\""), "{html}");
        // The size in use and the configured default both stay on offer.
        for n in [5, 7, 20, 50, 100] {
            assert!(
                html.contains(&format!("<option value=\"{n}\"")),
                "{n}: {html}"
            );
        }
        assert!(html.contains("<option value=\"5\" selected>"), "{html}");
        // The search text rides along in both forms, escaped, and without an
        // id: the header's search box owns `id="q"`.
        let q = format!(
            "<input type=\"hidden\" name=\"q\" value=\"{}\">",
            escape_html("a\"b<c")
        );
        assert_eq!(html.matches(q.as_str()).count(), 2, "{html}");
        assert!(!html.contains("a\"b<c"), "{html}");
        // A later page is named in the title.
        assert!(
            html.contains(
                "<title>Search: \u{201c}a&quot;b&lt;c\u{201d}, page 3 of 9 \u{2014} YANuget</title>"
            ),
            "{html}"
        );
        // A new search from the header keeps the chosen page size.
        assert!(
            html.contains(
                "<input type=\"hidden\" name=\"take\" value=\"5\"><button type=\"submit\">Search"
            ),
            "{html}"
        );
    }

    #[test]
    fn the_page_size_stays_on_offer_when_everything_fits() {
        // After choosing 100 on a feed of 60, there has to be a way back.
        let urls = UrlBuilder::new("https://host");
        let mut page = page_of(&["A", "B"]);
        page.total_hits = 60;
        let html = gallery_page(&urls, &page, &view("", 0, 100));
        assert!(html.contains("<select id=\"pg-take\""), "{html}");
        // With one page there is nothing to page through.
        assert!(!html.contains("pg-page"), "{html}");
        assert!(!html.contains("Previous"), "{html}");

        // A list shorter than the smallest page size needs no pager at all.
        page.total_hits = 2;
        let html = gallery_page(&urls, &page, &view("", 0, 20));
        assert!(!html.contains("class=\"pager\""), "{html}");
    }

    #[test]
    fn paging_carries_the_filters_and_names_later_pages() {
        let urls = UrlBuilder::new("https://host");
        let mut page = page_of(&["A", "B"]);
        page.total_hits = 6;
        let html = gallery_page(
            &urls,
            &page,
            &GalleryView {
                skip: 2,
                take: 2,
                default_take: 20,
                prerelease: Some(false),
                package_type: Some("Dependency"),
                ..Default::default()
            },
        );
        assert!(
            html.contains("skip=4&amp;take=2&amp;prerelease=false&amp;packageType=Dependency"),
            "{html}"
        );
        assert_eq!(
            html.matches("<input type=\"hidden\" name=\"packageType\" value=\"Dependency\">")
                .count(),
            2,
            "{html}"
        );
        assert!(
            html.contains("<title>Packages, page 2 of 3 \u{2014} YANuget</title>"),
            "{html}"
        );

        // On the first page Previous stays a link to assistive technology,
        // announced as disabled; the arrows are decoration.
        let first = gallery_page(&urls, &page, &view("", 0, 2));
        assert!(
            first.contains(
                "<a class=\"btn\" role=\"link\" aria-disabled=\"true\">\
                 <span aria-hidden=\"true\">\u{2190}</span> Previous</a>"
            ),
            "{first}"
        );
    }

    #[test]
    fn an_empty_feed_shows_the_commands_that_fill_it() {
        // The first page anyone sees. It has to carry *this* server's service
        // index, not a placeholder host, or it is just decoration.
        let urls = UrlBuilder::new("https://nuget.example.com");
        let html = gallery_page(&urls, &page_of(&[]), &view("", 0, 20));
        assert!(html.contains("Your feed is live"), "{html}");
        assert!(
            html.contains("dotnet nuget add source https://nuget.example.com/v3/index.json"),
            "{html}"
        );
        assert!(html.contains("dotnet nuget push"), "{html}");
        assert!(
            text_of(&html)
                .contains("dotnet restore --source https://nuget.example.com/v3/index.json"),
            "{html}"
        );
        // Each command gets a copy button, which reads `innerText` — so the
        // command must be escaped exactly once or the clipboard gets entities.
        assert_eq!(html.matches("class=\"copy\"").count(), 3, "{html}");
        assert!(!html.contains("&amp;amp;"), "{html}");

        // A search that finds nothing is a different situation and must not be
        // answered with onboarding instructions.
        let no_match = gallery_page(&urls, &page_of(&[]), &view("zzz", 0, 20));
        assert!(!no_match.contains("Your feed is live"), "{no_match}");
    }

    #[test]
    fn the_gallery_sorts_by_a_link_and_every_page_keeps_the_order() {
        let urls = UrlBuilder::new("https://host");
        let mut page = page_of(&["A", "B"]);
        page.total_hits = 6;
        let by_name = GalleryView {
            sort: SearchSort::Name,
            ..view("", 2, 2)
        };
        let html = gallery_page(&urls, &page, &by_name);
        // Three orders, the current one marked, each from the first page.
        assert!(
            html.contains(
                "<a href=\"/packages?q=&amp;skip=0&amp;take=2&amp;sort=name\" \
                 aria-current=\"true\">Name</a>"
            ),
            "{html}"
        );
        assert!(
            html.contains("<a href=\"/packages?q=&amp;skip=0&amp;take=2\">Downloads</a>"),
            "{html}"
        );
        assert!(
            html.contains("sort=updated\">Recently updated</a>"),
            "{html}"
        );
        // Paging, the pager's forms and a new search all keep the order.
        assert!(html.contains("skip=4&amp;take=2&amp;sort=name"), "{html}");
        assert_eq!(
            html.matches("<input type=\"hidden\" name=\"sort\" value=\"name\">")
                .count(),
            3,
            "{html}"
        );

        // The default order stays out of every URL, so old bookmarks and the
        // addresses the tests above pin keep their meaning.
        let html = gallery_page(&urls, &page, &view("", 2, 2));
        assert!(!html.contains("name=\"sort\""), "{html}");
        assert!(!html.contains("sort=downloads"), "{html}");

        // A single package has nothing to sort.
        let one = gallery_page(&urls, &page_of(&["A"]), &view("", 0, 20));
        assert!(!one.contains("Sort by"), "{one}");
    }

    #[test]
    fn tags_link_to_a_filtered_gallery_that_every_page_keeps() {
        let urls = UrlBuilder::new("https://host");
        let mut page = page_of(&["A", "B"]);
        page.groups[0].packages[0].tags = vec!["Logging".into(), "a&b<c".into()];
        page.total_hits = 6;
        // A row's tags link to the tag, lower-cased and escaped.
        let html = gallery_page(&urls, &page, &view("", 0, 2));
        assert!(
            html.contains("<a class=\"tag\" href=\"/packages?tag=logging\">Logging</a>"),
            "{html}"
        );
        assert!(
            html.contains("<a class=\"tag\" href=\"/packages?tag=a%26b%3Cc\">a&amp;b&lt;c</a>"),
            "{html}"
        );

        let tagged = GalleryView {
            tag: Some("logging"),
            ..view("", 2, 2)
        };
        let html = gallery_page(&urls, &page, &tagged);
        assert!(
            html.contains("<h1 class=\"title\">6 packages tagged \u{201c}logging\u{201d}</h1>"),
            "{html}"
        );
        assert!(
            html.contains(
                "<title>Tagged \u{201c}logging\u{201d}, page 2 of 3 \u{2014} YANuget</title>"
            ),
            "{html}"
        );
        // Paging, the pager's forms and a new search all keep the tag…
        assert!(html.contains("skip=4&amp;take=2&amp;tag=logging"), "{html}");
        assert_eq!(
            html.matches("<input type=\"hidden\" name=\"tag\" value=\"logging\">")
                .count(),
            3,
            "{html}"
        );
        // …and clearing it keeps the rest.
        assert!(
            html.contains("<a href=\"/packages?q=&amp;skip=0&amp;take=2\">Clear the tag</a>"),
            "{html}"
        );

        // A tag with no packages says so and offers the way back.
        let none = gallery_page(&urls, &page_of(&[]), &tagged);
        assert!(
            none.contains("No packages tagged \u{201c}logging\u{201d}"),
            "{none}"
        );
        assert!(!none.contains("Your feed is live"), "{none}");
    }

    #[test]
    fn the_landing_page_offers_popular_tags_and_the_cloud_scales_with_use() {
        let urls = UrlBuilder::new("https://host");
        let counts = vec![
            TagCount {
                tag: "logging".into(),
                packages: 40,
            },
            TagCount {
                tag: "build".into(),
                packages: 3,
            },
            TagCount {
                tag: "zeta".into(),
                packages: 1,
            },
        ];
        let landing = gallery_page(
            &urls,
            &page_of(&["A", "B"]),
            &GalleryView {
                popular: &counts,
                ..view("", 0, 20)
            },
        );
        assert!(
            landing.contains("<nav class=\"popular\" aria-label=\"Popular tags\">"),
            "{landing}"
        );
        assert!(landing.contains("<a class=\"all\" href=\"/tags\">All tags</a>"));

        let cloud = tags_page(&urls, &counts, false);
        // Alphabetical, the most used largest, the least smallest, and every
        // count written out rather than left to the size.
        let at = |needle: &str| {
            cloud
                .find(needle)
                .unwrap_or_else(|| panic!("{needle}: {cloud}"))
        };
        assert!(at(">build<") < at(">logging<") && at(">logging<") < at(">zeta<"));
        assert!(cloud.contains("<a class=\"t5\" href=\"/packages?tag=logging\">logging</a>"));
        assert!(cloud.contains("<a class=\"t1\" href=\"/packages?tag=zeta\">zeta</a>"));
        assert!(cloud.contains("<span class=\"n\">40<span class=\"vh\"> packages</span></span>"));
        assert!(cloud.contains("<span class=\"n\">1<span class=\"vh\"> package</span></span>"));
        assert!(cloud.contains("<a href=\"/tags\" aria-current=\"page\">Tags</a>"));

        // Small counts step once per package instead of jumping to the top.
        let small = tags_page(
            &urls,
            &[
                TagCount {
                    tag: "a".into(),
                    packages: 2,
                },
                TagCount {
                    tag: "b".into(),
                    packages: 1,
                },
            ],
            false,
        );
        assert!(
            small.contains("<a class=\"t2\" href=\"/packages?tag=a\">"),
            "{small}"
        );
        assert!(
            small.contains("<a class=\"t1\" href=\"/packages?tag=b\">"),
            "{small}"
        );

        let empty = tags_page(&urls, &[], false);
        assert!(empty.contains("No tags yet"), "{empty}");
    }

    #[test]
    fn feeds_index_lists_feeds() {
        let html = feeds_index_page(
            &[
                ("stable".into(), "/stable".into()),
                ("dev".into(), "/dev".into()),
            ],
            false,
        );
        assert!(html.contains("href=\"/stable\""));
        assert!(html.contains("/dev/v3/index.json"));
        assert!(!html.contains("require credentials"));
        assert!(feeds_index_page(&[], true).contains("require credentials"));
    }
}
