//! Server-rendered HTML for the human-facing package gallery.
//!
//! These are pure functions that turn domain types into HTML strings, mirroring
//! how [`crate::nuget`] turns them into JSON. There is no template engine; HTML
//! is assembled with `format!` and **every** value derived from package data is
//! run through [`escape_html`] to prevent stored XSS.
//!
//! The gallery is geared towards Chocolatey: the install snippet shown first is
//! the `choco install` command (configurable via `primary_client`).

use crate::config::Config;
use crate::models::{Package, PackageType};
use crate::nuget::UrlBuilder;

/// Minimal, dependency-free styling, inlined so the UI needs no static assets
/// and works fully offline.
const STYLE: &str = "\
:root{color-scheme:dark light;\
--bg:#0d1117;--card:#161b22;--border:#30363d;--fg:#e6edf3;--muted:#9aa4af;\
--accent:#58a6ff;--accent2:#1f6feb;--accent2h:#1a64d6;--onaccent:#fff;\
--code:#010409;--warn:#d29922;--subtle:#21262d;--subtleh:#30363d;\
--ok:#3fb950;--danger:#b62324;--dangerfg:#ff7b72;--ctl:#656c76}\
@media(prefers-color-scheme:light){:root{\
--bg:#f6f8fa;--card:#fff;--border:#d0d7de;--fg:#1f2328;--muted:#59636e;\
--accent:#0969da;--accent2:#0969da;--accent2h:#0a5fc2;--onaccent:#fff;\
--code:#f6f8fa;--warn:#9a6700;--subtle:#eaeef2;--subtleh:#dde3ea;\
--ok:#1a7f37;--danger:#cf222e;--dangerfg:#cf222e;--ctl:#818b98}}\
*{box-sizing:border-box}\
button,input,select{font-family:inherit}\
body{margin:0;background:var(--bg);color:var(--fg);\
font:15px/1.55 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif}\
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}\
a:focus-visible,button:focus-visible,input:focus-visible,select:focus-visible{outline:2px solid var(--accent);outline-offset:2px}\
.vh{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);border:0}\
.skip{position:absolute;left:-999px;top:0;background:var(--accent2);color:var(--onaccent);padding:8px 12px;border-radius:0 0 6px 0;z-index:10}\
.skip:focus{left:0}\
header{background:var(--card);border-bottom:1px solid var(--border);padding:14px 0}\
.wrap{max-width:980px;margin:0 auto;padding:0 20px}\
header .wrap{display:flex;align-items:center;gap:16px}\
.logo{font-weight:700;font-size:20px;color:var(--fg)}\
.logo span{color:var(--accent)}\
form.search{flex:1;display:flex;gap:8px}\
input[type=search]{flex:1;padding:9px 12px;border-radius:6px;border:1px solid var(--ctl);\
background:var(--bg);color:var(--fg);font-size:15px;min-height:44px}\
input[type=search]:focus{border-color:var(--accent)}\
button{padding:9px 16px;border-radius:6px;border:1px solid var(--accent2);\
background:var(--accent2);color:var(--onaccent);font-size:15px;cursor:pointer;min-height:44px}\
button:hover{background:var(--accent2h)}\
@media(max-width:560px){header .wrap{flex-wrap:wrap}form.search{flex:1 0 100%}}\
main{padding:26px 0 60px}\
.card{background:var(--card);border:1px solid var(--border);border-radius:10px;\
padding:18px 20px;margin:0 0 14px}\
.card h2{margin:0 0 4px;font-size:18px;overflow-wrap:anywhere}\
.meta{color:var(--muted);font-size:13px;margin:2px 0}\
.crumbs{font-size:13px;color:var(--muted);margin:0 0 6px}\
.tags{margin-top:8px;list-style:none;padding:0;display:flex;flex-wrap:wrap}\
.tag{display:inline-block;background:var(--subtle);border:1px solid var(--border);border-radius:20px;\
padding:1px 10px;font-size:12px;color:var(--muted);margin:0 4px 4px 0}\
.badge{display:inline-block;font-size:11px;padding:0 7px;border-radius:20px;border:1px solid var(--border);vertical-align:middle}\
.badge.pre{color:var(--warn);border-color:var(--warn)}\
.badge.un{color:var(--muted)}\
.muted{color:var(--muted)}\
.grid{display:grid;grid-template-columns:1fr 340px;gap:22px}\
@media(max-width:760px){.grid{grid-template-columns:1fr}}\
.detail{grid-template-columns:1fr 340px;grid-template-areas:\"main side\" \"readme side\";grid-template-rows:auto 1fr}\
.detail>.content{grid-area:main}.detail>.side{grid-area:side}.detail>.readme-area{grid-area:readme;min-width:0}\
@media(max-width:760px){.detail{grid-template-columns:1fr;grid-template-areas:\"main\" \"side\" \"readme\";grid-template-rows:auto}}\
h1.title{font-size:26px;margin:0 0 2px;overflow-wrap:anywhere}\
pre{background:var(--code);border:1px solid var(--border);border-radius:8px;padding:12px 14px;\
overflow:auto;font-size:13px;margin:6px 0}\
code{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}\
.install h3{margin:14px 0 4px;font-size:13px;text-transform:uppercase;letter-spacing:.4px;color:var(--muted)}\
.install .primary h3{color:var(--accent)}\
.install pre{white-space:pre-wrap;overflow-wrap:break-word}\
.snip{display:flex;flex-direction:column;align-items:flex-end}\
.snip .copy{padding:6px 12px;font-size:12px;min-height:32px;margin-bottom:-4px;\
background:var(--subtle);border:1px solid var(--border);color:var(--fg)}\
.snip .copy:hover{background:var(--subtleh)}\
.snip pre{width:100%}\
.versions{list-style:none;margin:0;padding:0;max-height:340px;overflow:auto}\
.versions li{display:flex;justify-content:space-between;align-items:center;gap:8px;padding:5px 0;border-bottom:1px solid var(--border)}\
.versions a{min-height:40px;display:inline-flex;align-items:center}\
.versions a.sel{font-weight:700}\
table.deps{width:100%;border-collapse:collapse;font-size:13px}\
table.deps td{padding:3px 8px 3px 0}\
.readme{white-space:pre-wrap;word-wrap:break-word;overflow-wrap:anywhere}\
img.picon{width:32px;height:32px;object-fit:contain;vertical-align:-6px;margin-right:10px;border-radius:6px;background:var(--subtle)}\
.links a[rel~=nofollow]::after{content:\" \u{2197}\";color:var(--muted);font-size:11px}\
.empty{text-align:center;color:var(--muted);padding:60px 0}\
.hero{text-align:center;padding:30px 0 4px}\
.hero h1{font-size:30px;margin:0 0 8px;letter-spacing:-.4px}\
.hero p{margin:0 auto;max-width:46ch;color:var(--muted)}\
.steps h3{margin:16px 0 4px;font-size:13px;text-transform:uppercase;letter-spacing:.4px;color:var(--muted)}\
.steps h3:first-child{margin-top:0}\
.steps pre{white-space:pre-wrap;overflow-wrap:break-word}\
.kv{font-size:13px}.kv div{display:flex;gap:10px;padding:3px 0;border-bottom:1px solid var(--border)}\
.kv b{color:var(--muted);font-weight:500;min-width:120px;flex:0 0 auto}\
@media(max-width:480px){.kv div{flex-direction:column;gap:0}.kv b{min-width:0}}\
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:14px;margin:0 0 8px}\
.stat{background:var(--card);border:1px solid var(--border);border-radius:10px;padding:16px 18px}\
.stat .n{font-size:26px;font-weight:700}\
.stat .l{color:var(--muted);font-size:13px}\
.rank{list-style:none;margin:0;padding:0}\
.rank li{display:flex;justify-content:space-between;gap:10px;flex-wrap:wrap;padding:6px 0;border-bottom:1px solid var(--border)}\
.rank li>a{overflow-wrap:anywhere}\
.pager{display:flex;align-items:center;justify-content:space-between;gap:12px;margin:18px 0;flex-wrap:wrap}\
.btn{border:1px solid var(--border);border-radius:6px;padding:8px 14px;color:var(--fg)}\
.btn[aria-disabled=true]{opacity:.4;pointer-events:none}\
.pager .btn{min-height:44px;display:inline-flex;align-items:center;gap:6px}\
.pager-go{flex:1 0 100%;display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:12px}\
.pager-go form{display:flex;align-items:center;gap:8px;margin:0}\
.pager-go label{color:var(--muted);font-size:14px}\
.pager-go input,.pager-go select{min-height:44px;padding:0 10px;border:1px solid var(--ctl);border-radius:6px;\
background:var(--bg);color:var(--fg);font-size:15px}\
.pager-go input{width:6em}\
.pager-go button{padding:0 14px;background:var(--subtle);border-color:var(--border);color:var(--fg)}\
.pager-go button:hover{background:var(--subtleh)}\
.badge.ok{color:var(--ok);border-color:var(--ok)}\
.atbl{width:100%;border-collapse:collapse}\
.atbl th,.atbl td{text-align:left;padding:8px 10px;border-bottom:1px solid var(--border);font-size:14px;vertical-align:middle}\
.atbl th{color:var(--muted);font-weight:500;font-size:12px;text-transform:uppercase;letter-spacing:.4px}\
.actions{display:flex;gap:8px;flex-wrap:wrap}\
.actions form{margin:0}\
.actions button{padding:8px 14px;font-size:13px;min-height:40px;background:var(--subtle);border:1px solid var(--border);color:var(--fg)}\
.actions button:hover{background:var(--subtleh)}\
.actions button.danger{border-color:var(--danger);color:var(--dangerfg)}\
.actions button.danger:hover{background:var(--danger);color:var(--onaccent)}\
footer{border-top:1px solid var(--border);color:var(--muted);font-size:13px;padding:18px 0}\
footer a[aria-current=page]{color:var(--fg);font-weight:600}\
";

/// The inline SVG favicon, as a data URI so the page loads no external asset.
const FAVICON: &str = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'\
%20viewBox='0%200%2032%2032'%3E%3Crect%20width='32'%20height='32'%20rx='6'%20fill='%23512bd4'/%3E\
%3Ctext%20x='16'%20y='22'%20font-size='15'%20font-family='sans-serif'%20font-weight='700'\
%20fill='white'%20text-anchor='middle'%3EYN%3C/text%3E%3C/svg%3E";

/// The form field (and header) carrying the admin CSRF token.
pub const CSRF_FIELD: &str = "_csrf";

/// Escape the five HTML-significant characters.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Return `url` only if it carries a safe, expected scheme (`http`, `https` or
/// `mailto`). Package metadata (project/repository/license/icon URLs) is
/// attacker-controlled, so an unfiltered value like `javascript:alert(1)` or a
/// `data:` URI placed into an `href`/`src` attribute would be a stored-XSS hole
/// that HTML-escaping alone does not close (the scheme contains no escapable
/// characters). The scheme is compared case-insensitively with ASCII whitespace
/// and control characters stripped, because browsers ignore those when
/// resolving it. The caller must still HTML-escape the returned value.
pub fn safe_href(url: &str) -> Option<&str> {
    let trimmed = url.trim();
    let scheme: String = trimmed
        .split(':')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_ascii_control())
        .flat_map(char::to_lowercase)
        .collect();
    match scheme.as_str() {
        "http" | "https" | "mailto" => Some(trimmed),
        _ => None,
    }
}

/// Tiny inline script giving the install-command "Copy" buttons their
/// behaviour. It degrades gracefully: without JS the `<pre>` stays selectable
/// and the button simply does nothing.
///
/// Kept separate from its `<script>` wrapper because the CSP hash below must be
/// taken over exactly this text — the element's content, not the tags.
/// It also carries the admin area's destructive-action confirmation. That used
/// to be an inline `onsubmit=` attribute, which the CSP below cannot whitelist
/// by hash — so the prompt is delegated from here off a `data-confirm`
/// attribute instead, keeping the guard rail and the policy both intact.
const COPY_SCRIPT_BODY: &str = "document.addEventListener('click',function(e){\
var b=e.target.closest('.copy');if(!b)return;\
var c=b.parentNode.querySelector('code');if(!c||!navigator.clipboard)return;\
navigator.clipboard.writeText(c.innerText).then(function(){\
var o=b.textContent;b.textContent='Copied';setTimeout(function(){b.textContent=o},1200)})});\
document.addEventListener('submit',function(e){\
var m=e.target.getAttribute&&e.target.getAttribute('data-confirm');\
if(m&&!confirm(m))e.preventDefault()});";

/// The `Content-Security-Policy` served with every gallery/admin page.
///
/// The gallery renders package-supplied metadata (descriptions, readmes, links,
/// dependency ids). [`escape_html`] and [`safe_href`] are the primary defence;
/// this policy is the backstop that keeps an escaping bug from becoming script
/// execution. `default-src 'none'` denies everything not listed, and the only
/// inline style/script permitted are the two the server itself emits, pinned by
/// SHA-256 — an injected `<script>` has a different hash and will not run.
pub static CSP: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "default-src 'none'; img-src 'self' data:; style-src '{style}'; script-src '{script}'; \
         base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        style = csp_hash(STYLE),
        script = csp_hash(COPY_SCRIPT_BODY),
    )
});

/// The `sha256-<base64>` source expression for an inline element's content.
fn csp_hash(content: &str) -> String {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(content.as_bytes());
    format!(
        "sha256-{}",
        base64::engine::general_purpose::STANDARD.encode(digest)
    )
}

/// Wrap a page body in the shared layout (head, header bar, footer).
///
/// `active` marks the current footer nav item (`"stats"`, `"settings"`, or `""`).
fn layout(urls: &UrlBuilder, title: &str, query: &str, active: &str, body: &str) -> String {
    layout_with_chrome(urls, title, query, active, body, Chrome::Feed, "")
}

/// Which navigation a page can offer.
///
/// Search, the service index, the docs and the stats/settings pages are all
/// *feed-scoped* routes: in multi-feed mode they exist only under `/{feed}`.
/// The feed-index page at the root has none of them, so offering them there
/// gives a first-time visitor a search box that returns a bare 404 and three
/// dead links — on the very first page they see.
#[derive(Clone, Copy, PartialEq)]
enum Chrome {
    /// Inside a feed: everything is reachable.
    Feed,
    /// The multi-feed root: only the feed list itself.
    Root,
}

/// `search_hidden` is extra hidden inputs for the header's search form: the
/// gallery uses it to keep a chosen page size across a new search.
fn layout_with_chrome(
    urls: &UrlBuilder,
    title: &str,
    query: &str,
    active: &str,
    body: &str,
    chrome: Chrome,
    search_hidden: &str,
) -> String {
    let cur = |name: &str| {
        if name == active {
            " aria-current=\"page\""
        } else {
            ""
        }
    };
    if chrome == Chrome::Root {
        return format!(
            "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<link rel=\"icon\" href=\"{FAVICON}\">\
<title>{title}</title><style>{STYLE}</style></head><body>\
<a class=\"skip\" href=\"#main\">Skip to content</a>\
<header><div class=\"wrap\">\
<a class=\"logo\" href=\"/\">YA<span>NuGet</span></a></div></header>\
<main id=\"main\" tabindex=\"-1\"><div class=\"wrap\">{body}</div></main>\
<footer><div class=\"wrap\">Served by YANuget</div></footer>\
<script>{COPY_SCRIPT_BODY}</script></body></html>",
            title = escape_html(title),
        );
    }
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<link rel=\"icon\" href=\"{FAVICON}\">\
<title>{title}</title><style>{STYLE}</style></head><body>\
<a class=\"skip\" href=\"#main\">Skip to content</a>\
<header><div class=\"wrap\">\
<a class=\"logo\" href=\"{home}\">YA<span>NuGet</span></a>\
<form class=\"search\" action=\"{packages}\" method=\"get\" role=\"search\">\
<label for=\"q\" class=\"vh\">Search packages</label>\
<input id=\"q\" type=\"search\" name=\"q\" placeholder=\"Search packages\u{2026}\" value=\"{q}\" autocomplete=\"off\">\
{search_hidden}<button type=\"submit\">Search</button></form>\
</div></header>\
<main id=\"main\" tabindex=\"-1\"><div class=\"wrap\">{body}</div></main>\
<footer><div class=\"wrap\"><nav aria-label=\"Site\">Served by YANuget \u{2014} \
<a href=\"{idx}\">v3 service index</a> \u{2022} <a href=\"{docs}\">Docs</a> \u{2022} \
<a href=\"{stats}\"{cs}>Stats</a> \u{2022} \
<a href=\"{settings}\"{cg}>Settings</a></nav></div></footer>\
<script>{COPY_SCRIPT_BODY}</script></body></html>",
        title = escape_html(title),
        q = escape_html(query),
        home = escape_html(&urls.app("/")),
        packages = escape_html(&urls.app("/packages")),
        idx = escape_html(&urls.service_index()),
        docs = escape_html(&urls.app("/docs/")),
        stats = escape_html(&urls.app("/stats")),
        settings = escape_html(&urls.app("/settings")),
        cs = cur("stats"),
        cg = cur("settings"),
    )
}

/// A styled error page for the gallery, so a browser never sees a bare JSON
/// error body.
///
/// The text is derived from the status alone. The JSON body it replaces is
/// deliberately not parsed through: a 5xx message is generic on purpose (the
/// underlying I/O, SQL or upstream detail goes to the log), and re-rendering an
/// error string into HTML is a needless place to get escaping wrong.
pub fn error_page(urls: &UrlBuilder, status: axum::http::StatusCode) -> String {
    use axum::http::StatusCode;
    let (heading, detail) = match status {
        StatusCode::NOT_FOUND => (
            "Not found",
            "This feed does not have that package, version or page. It may never              have been published here, or it may have been deleted.",
        ),
        StatusCode::UNAUTHORIZED => (
            "Sign-in required",
            "This feed requires credentials to browse. Use the API key configured              for reading it.",
        ),
        StatusCode::BAD_REQUEST => (
            "That request did not make sense",
            "Check the address for a typo.",
        ),
        StatusCode::TOO_MANY_REQUESTS => (
            "Too many requests",
            "This client has been throttled. Wait a moment and try again.",
        ),
        StatusCode::SERVICE_UNAVAILABLE => (
            "Temporarily unavailable",
            "The server cannot reach its database right now. It should recover on              its own.",
        ),
        s if s.is_server_error() => (
            "Something went wrong",
            "The server hit an unexpected error. The details are in its log.",
        ),
        _ => (
            "That did not work",
            "The request could not be completed.",
        ),
    };
    let body = format!(
        "<div class=\"empty\"><h1 class=\"title\">{heading}</h1>\
         <p>{detail}</p>\
         <p><a href=\"{home}\">Back to the package list</a></p>\
         <p class=\"muted\">HTTP {code}</p></div>",
        home = escape_html(&urls.app("/")),
        code = status.as_u16(),
    );
    layout(urls, &format!("{heading} \u{2014} YANuget"), "", "", &body)
}

/// What the gallery was asked to show: the search, the page, and the filters
/// that every paging link and form has to carry.
#[derive(Debug, Clone, Copy, Default)]
pub struct GalleryView<'a> {
    pub query: &'a str,
    pub skip: i64,
    pub take: i64,
    /// The configured page size (`gallery_page_size`).
    pub default_take: i64,
    pub prerelease: Option<bool>,
    pub package_type: Option<&'a str>,
}

impl GalleryView<'_> {
    /// The gallery URL of the page starting at `skip`, escaped for an
    /// attribute, carrying the search, the page size and the filters.
    fn href(&self, urls: &UrlBuilder, skip: i64) -> String {
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
        href
    }

    /// Hidden inputs carrying the search and the filters into a GET form. They
    /// have no ids: the header's search box already owns `id="q"`.
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
        out
    }
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
    let pages = (page.total_hits + view.take - 1) / view.take;
    let current = view.skip / view.take + 1;
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
            format!(
                "<div class=\"empty\"><h1 class=\"title\">There is nothing on this page</h1>\
                 <p><a href=\"{last}\">Go to the last page ({pages})</a></p>\
                 <p><a href=\"{first}\">Back to the first page</a></p></div>",
                last = view.href(urls, (pages - 1) * view.take),
                first = view.href(urls, 0),
            )
        } else if !query.trim().is_empty() {
            format!(
                "<div class=\"empty\"><h1 class=\"title\">No packages match \u{201c}{}\u{201d}</h1>\
                 <p><a href=\"{browse_all}\">Clear search and browse all packages</a></p></div>",
                escape_html(query),
            )
        } else {
            first_run_panel(urls)
        }
    } else {
        let mut cards = String::new();
        // A real `<h1>`, not a muted paragraph: this is the landing page, and
        // without one a screen reader announces no page heading at all — while
        // the *empty* state did have one, so the structure changed with the
        // content.
        let heading = if query.trim().is_empty() {
            format!("{} package{}", page.total_hits, plural(page.total_hits))
        } else {
            format!(
                "{} result{} for \u{201c}{}\u{201d}",
                page.total_hits,
                plural(page.total_hits),
                escape_html(query)
            )
        };
        cards.push_str(&format!("<h1 class=\"title\">{heading}</h1>"));
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
            let authors = if p.authors.is_empty() {
                String::new()
            } else {
                format!(" \u{2022} by {}", escape_html(&p.authors.join(", ")))
            };
            cards.push_str(&format!(
                "<div class=\"card\"><h2><a href=\"{url}\">{id}</a> \
                 <span class=\"muted\">{ver}</span>{pre}</h2>\
                 <div class=\"meta\">{dl} downloads, all versions{authors}</div>\
                 <p>{desc}</p>{tags}</div>",
                ver = escape_html(&p.normalized_version()),
                dl = group_digits(group.total_downloads() as i64),
                desc = escape_html(&truncate(&p.description, 240)),
                tags = render_tags(&p.tags),
            ));
        }
        cards.push_str(&pager(
            urls,
            &view,
            page.groups.len() as i64,
            page.total_hits,
        ));
        cards
    };
    // Searches and later pages get titles of their own. Otherwise every search,
    // and every page of one, shares one <title>, so tabs, bookmarks and history
    // entries look identical.
    let search = (!query.trim().is_empty()).then(|| format!("Search: \u{201c}{query}\u{201d}"));
    let later_page = current > 1 && !page.groups.is_empty();
    let title = match (search, later_page) {
        (None, false) => "YANuget".to_string(),
        (None, true) => format!("Packages, page {current} of {pages} \u{2014} YANuget"),
        (Some(s), false) => format!("{s} \u{2014} YANuget"),
        (Some(s), true) => format!("{s}, page {current} of {pages} \u{2014} YANuget"),
    };
    // A new search starts on page one, but keeps a page size someone chose.
    let search_hidden = if view.take != view.default_take.max(1) {
        format!(
            "<input type=\"hidden\" name=\"take\" value=\"{}\">",
            view.take
        )
    } else {
        String::new()
    };
    layout_with_chrome(urls, &title, query, "", &body, Chrome::Feed, &search_hidden)
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
            "1 \u{2014} Add this feed",
            format!("dotnet nuget add source {idx} -n yanuget"),
        ),
        (
            "2 \u{2014} Push a package",
            "dotnet nuget push MyPackage.1.0.0.nupkg --source yanuget --api-key <your-api-key>"
                .to_string(),
        ),
        (
            "3 \u{2014} Restore from it",
            format!("dotnet restore --source {idx}"),
        ),
    ];

    let mut snippets = String::new();
    for (label, cmd) in steps {
        snippets.push_str(&format!(
            "<h3>{label}</h3><div class=\"snip\">\
             <button type=\"button\" class=\"copy\" aria-label=\"Copy command\">Copy</button>\
             <pre><code>{cmd}</code></pre></div>",
            cmd = escape_html(&cmd),
        ));
    }

    format!(
        "<div class=\"hero\"><h1>Your feed is live</h1>\
         <p>Nothing published to it yet. Three commands change that.</p></div>\
         <div class=\"card steps\">{snippets}</div>\
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

/// The statistics page: feed-wide totals, the most-downloaded packages, and the
/// most recently published versions.
pub fn stats_page(
    urls: &UrlBuilder,
    stats: &crate::database::DatabaseStats,
    top: &crate::database::SearchPage,
    recent: &[Package],
) -> String {
    let cards = [
        (stats.package_count.to_string(), "Packages"),
        (stats.version_count.to_string(), "Versions"),
        (group_digits(stats.total_downloads), "Downloads"),
        (human_size(stats.total_size.max(0) as u64), "Storage"),
        (stats.symbol_count.to_string(), "Symbol files"),
        (stats.listed_count.to_string(), "Listed versions"),
    ];
    let mut tiles = String::from("<div class=\"stats\">");
    for (n, l) in cards {
        tiles.push_str(&format!(
            "<div class=\"stat\"><div class=\"n\">{}</div><div class=\"l\">{}</div></div>",
            escape_html(&n),
            l
        ));
    }
    tiles.push_str("</div>");

    let top_list = if top.groups.is_empty() {
        "<p class=\"muted\">No packages yet.</p>".to_string()
    } else {
        let mut out = String::from("<ul class=\"rank\">");
        for g in &top.groups {
            let p = g.latest();
            out.push_str(&format!(
                "<li><a href=\"{href}\">{id}</a>\
                 <span class=\"muted\">{dl} downloads</span></li>",
                href = escape_html(&urls.app(&format!("/packages/{}", enc_path(&p.lower_id())))),
                id = escape_html(&p.id),
                dl = group_digits(g.total_downloads() as i64),
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
                "<li><a href=\"{href}\">{id} {dv}</a>\
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
         <div class=\"grid\">\
         <div class=\"card\"><h3 class=\"muted\">Most downloaded</h3>{top_list}</div>\
         <div class=\"card\"><h3 class=\"muted\">Recently published</h3>{recent_list}</div>\
         </div>"
    );
    layout(urls, "Statistics \u{2014} YANuget", "", "stats", &body)
}

/// Group a non-negative integer into thousands with `,` separators.
/// `""` or `"s"`, so counts read as "1 package" rather than "1 package(s)".
fn plural(n: i64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn group_digits(n: i64) -> String {
    let s = n.max(0).to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// A read-only overview of the server's relevant settings.
///
/// Deliberately omits secrets and infrastructure details (the API key, data
/// paths and bind address) because the gallery is unauthenticated — it only
/// surfaces policy that affects how clients interact with the feed.
pub fn settings_page(urls: &UrlBuilder, config: &Config, feed: &super::FeedContext) -> String {
    let yes_no = |b: bool| if b { "Yes" } else { "No" };
    let on_off = |b: bool| if b { "Enabled" } else { "Disabled" };

    let auth = if feed.auth.is_enabled() {
        "Required (API key)"
    } else {
        "Open \u{2014} no API key set"
    };
    let read_auth = if feed.read_auth.is_enabled() {
        "Required (credential)"
    } else {
        "Open"
    };
    let max_size = match config.max_package_size_bytes {
        Some(n) => human_size(n),
        None => "Unlimited".to_string(),
    };
    let delete_mode = if feed.hard_delete_enabled {
        "Hard delete (removes files)"
    } else {
        "Unlist (restorable)"
    };

    let mut server = String::from("<div class=\"kv\">");
    server.push_str(&kv("Feed", &feed.name));
    server.push_str(&kv("Push / delete auth", auth));
    server.push_str(&kv("Download auth", read_auth));
    server.push_str(&kv("Max package size", &max_size));
    server.push_str(&kv(
        "Overwrite existing version",
        feed.allow_overwrite.label(),
    ));
    server.push_str(&kv("Delete behaviour", delete_mode));
    server.push_str(&kv("Approval required", yes_no(feed.requires_approval)));
    if let Some(target) = &feed.promotes_to {
        server.push_str(&kv("Promotes to", target));
    }
    server.push_str(&kv("Symbol server", on_off(config.enable_symbol_server)));
    server.push_str(&kv("Web gallery", on_off(config.enable_web_ui)));
    server.push_str(&kv("Preferred client", &config.primary_client));
    server.push_str("</div>");

    // Upstream mirroring + license policy (per feed).
    let mut policy = String::from("<div class=\"kv\">");
    match &feed.mirror {
        Some(m) => {
            policy.push_str(&kv("Upstream mirror", "Enabled"));
            policy.push_str(&kv("Upstream", &redact_userinfo(m.upstream())));
        }
        None => policy.push_str(&kv("Upstream mirror", "Disabled")),
    }
    let lp = &feed.license_policy;
    policy.push_str(&kv("License policy", on_off(lp.enabled)));
    if lp.enabled {
        let action = match lp.action {
            crate::config::PolicyAction::Block => "Block",
            crate::config::PolicyAction::Warn => "Warn (flag)",
        };
        policy.push_str(&kv("On violation", action));
        if !lp.allowed.is_empty() {
            policy.push_str(&kv("Allowed", &lp.allowed.join(", ")));
        }
        if !lp.blocked.is_empty() {
            policy.push_str(&kv("Blocked", &lp.blocked.join(", ")));
        }
        policy.push_str(&kv("Allow unlicensed", yes_no(lp.allow_unlicensed)));
    }
    policy.push_str("</div>");

    let r = &feed.retention;
    let mut retention = String::from("<div class=\"kv\">");
    retention.push_str(&kv("Retention", on_off(r.enabled)));
    if r.enabled {
        retention.push_str(&kv("Prune after each push", yes_no(r.prune_on_push)));
        let sweep = if r.interval_hours > 0 {
            format!("every {} h", r.interval_hours)
        } else {
            "off".to_string()
        };
        retention.push_str(&kv("Scheduled sweep", &sweep));
        retention.push_str(&kv("Keep newest stable", &opt_count(r.keep_latest_stable)));
        retention.push_str(&kv(
            "Keep newest pre-release",
            &opt_count(r.keep_latest_prerelease),
        ));
        retention.push_str(&kv(
            "Max version age",
            &match r.max_age_days {
                Some(d) => format!("{d} days"),
                None => "No limit".to_string(),
            },
        ));
    }
    retention.push_str("</div>");

    let admin = if feed.admin.is_enabled() {
        format!(
            "<div class=\"card\"><h3 class=\"muted\">Administration</h3>\
             <p>Manage package versions (approve / promote / disable / delete) in the \
             <a href=\"{}\">admin area</a>. Sign in with the admin key.</p></div>",
            escape_html(&urls.app("/admin"))
        )
    } else {
        String::new()
    };

    let body = format!(
        "<h1 class=\"title\">Settings</h1>\
         <p class=\"muted\">Read-only overview of this feed's policy. \
         Secrets and storage paths are not shown.</p>\
         <div class=\"card\"><h3 class=\"muted\">Server</h3>{server}</div>\
         <div class=\"card\"><h3 class=\"muted\">Mirror &amp; policy</h3>{policy}</div>\
         <div class=\"card\"><h3 class=\"muted\">Retention</h3>{retention}</div>\
         <div class=\"card\"><h3 class=\"muted\">Endpoints</h3><div class=\"kv\">\
         {svc}{sym}</div></div>{admin}",
        svc = kv_html(
            "Service index",
            &format!(
                "<a href=\"{u}\">{u}</a>",
                u = escape_html(&urls.service_index())
            )
        ),
        sym = if config.enable_symbol_server {
            kv_html(
                "Symbol server",
                &format!("<code>{}</code>", escape_html(&urls.symbol_server())),
            )
        } else {
            String::new()
        },
    );
    layout(urls, "Settings \u{2014} YANuget", "", "settings", &body)
}

/// The admin dashboard: every package id, linking to its management page.
pub fn admin_dashboard_page(urls: &UrlBuilder, ids: &[String]) -> String {
    let body = if ids.is_empty() {
        "<h1 class=\"title\">Admin</h1><p class=\"muted\">No packages published yet.</p>"
            .to_string()
    } else {
        let mut list = String::from("<ul class=\"rank\">");
        for id in ids {
            list.push_str(&format!(
                "<li><a href=\"{href}\">{id}</a></li>",
                href = escape_html(
                    &urls.app(&format!("/admin/packages/{}", enc_path(&id.to_lowercase())))
                ),
                id = escape_html(id),
            ));
        }
        list.push_str("</ul>");
        format!(
            "<h1 class=\"title\">Admin</h1>\
             <p class=\"muted\">Select a package to disable, enable or delete its versions.</p>\
             <div class=\"card\">{list}</div>"
        )
    };
    layout(urls, "Admin \u{2014} YANuget", "", "", &body)
}

/// The per-package admin page: every version (incl. disabled, pending and
/// flagged) with moderation actions. `promote_target`, when set, names the next
/// release ring an admin can promote a version into.
pub fn admin_package_page(
    urls: &UrlBuilder,
    id: &str,
    versions: &[crate::database::FeedVersion],
    promote_target: Option<&str>,
    csrf_token: &str,
) -> String {
    let mut ordered: Vec<&crate::database::FeedVersion> = versions.iter().collect();
    ordered.sort_by(|a, b| b.package.version.cmp(&a.package.version));

    let action = |v: &str, op: &str| {
        escape_html(&urls.app(&format!(
            "/admin/packages/{}/{}/{}",
            enc_path(&id.to_lowercase()),
            enc_path(v),
            op
        )))
    };
    // Every admin form carries the CSRF token; the handlers reject a POST
    // without it, so a cross-site form submission cannot ride the browser's
    // auto-replayed Basic credentials.
    let csrf = format!(
        "<input type=\"hidden\" name=\"{CSRF_FIELD}\" value=\"{}\">",
        escape_html(csrf_token)
    );

    let mut rows = String::new();
    for fv in ordered {
        let p = &fv.package;
        let v = p.normalized_version();
        let mut status = if fv.pending {
            "<span class=\"badge pre\">pending</span>".to_string()
        } else if !p.enabled {
            "<span class=\"badge un\">disabled</span>".to_string()
        } else if p.listed {
            "<span class=\"badge ok\">active</span>".to_string()
        } else {
            "<span class=\"badge\">unlisted</span>".to_string()
        };
        if fv.flagged {
            status.push_str(" <span class=\"badge\" title=\"policy\">flagged</span>");
        }

        let mut actions = String::new();
        if fv.pending {
            actions.push_str(&format!(
                "<form method=\"post\" action=\"{}\">{csrf}<button type=\"submit\">Approve</button></form>",
                action(&v, "approve")
            ));
        }
        if let Some(target) = promote_target {
            actions.push_str(&format!(
                "<form method=\"post\" action=\"{}\">{csrf}<button type=\"submit\">Promote \u{2192} {}</button></form>",
                action(&v, "promote"),
                escape_html(target),
            ));
        }
        // Enable/disable toggle depending on current state.
        if p.enabled {
            actions.push_str(&format!(
                "<form method=\"post\" action=\"{}\">{csrf}<button type=\"submit\">Disable</button></form>",
                action(&v, "disable")
            ));
        } else {
            actions.push_str(&format!(
                "<form method=\"post\" action=\"{}\">{csrf}<button type=\"submit\">Enable</button></form>",
                action(&v, "enable")
            ));
        }
        actions.push_str(&format!(
            "<form method=\"post\" action=\"{a}\" data-confirm=\"{confirm}\">{csrf}\
             <button type=\"submit\" class=\"danger\">Delete</button></form>",
            a = action(&v, "delete"),
            confirm = escape_html(&format!(
                "Remove {id} {v} from this feed? If no other feed uses it, the files are deleted."
            )),
        ));

        let reason = match (fv.flagged, fv.flag_reason.as_deref()) {
            (true, Some(r)) => format!("<div class=\"meta\">{}</div>", escape_html(r)),
            _ => String::new(),
        };
        rows.push_str(&format!(
            "<tr><td>{pre}{dv}{reason}</td><td>{status}</td><td class=\"muted\">{dl}</td>\
             <td><div class=\"actions\">{actions}</div></td></tr>",
            pre = if p.is_prerelease() {
                "<span class=\"badge pre\">pre</span> "
            } else {
                ""
            },
            dv = escape_html(&v),
            dl = group_digits(p.downloads as i64),
        ));
    }

    let body = format!(
        "<nav class=\"crumbs\" aria-label=\"Breadcrumb\">\
         <a href=\"{admin}\">Admin</a> <span aria-hidden=\"true\">/</span> <span>{id}</span></nav>\
         <h1 class=\"title\">{id}</h1>\
         <p class=\"muted\">Disabled and pending versions are hidden from clients and not \
         downloadable. Delete removes this feed's membership.</p>\
         <div class=\"card\"><table class=\"atbl\">\
         <thead><tr><th>Version</th><th>Status</th><th>Downloads</th><th>Actions</th></tr></thead>\
         <tbody>{rows}</tbody></table></div>",
        admin = escape_html(&urls.app("/admin")),
        id = escape_html(id),
    );
    layout(urls, &format!("Admin \u{2014} {id}"), "", "", &body)
}

/// The root feed index, shown when more than one feed is hosted. Each entry is
/// `(feed name, path prefix)` where the prefix is e.g. `/stable`.
pub fn feeds_index_page(feeds: &[(String, String)]) -> String {
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
    let body = format!(
        "<h1 class=\"title\">Feeds</h1>\
         <p class=\"muted\">This server hosts several NuGet feeds. Pick one:</p>\
         <div class=\"card\">{list}</div>"
    );
    layout_with_chrome(
        &urls,
        "Feeds \u{2014} YANuget",
        "",
        "",
        &body,
        Chrome::Root,
        "",
    )
}

/// Replace any `user:password@` in a URL with `***@`.
///
/// The upstream is operator-configured and normally carries its credentials in
/// the separate `[mirror.auth]` settings — but nothing stops someone putting
/// them in the URL, and this page is the one place that URL is displayed. The
/// page is read-auth gated, so this is defence in depth rather than the only
/// guard.
fn redact_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    // Userinfo, if present, is everything before the first `@` of the authority.
    let (authority, tail) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://***@{host}{tail}"),
        None => url.to_string(),
    }
}

/// A key/value row with an escaped text value.
fn kv(label: &str, value: &str) -> String {
    kv_html(label, &escape_html(value))
}

/// A key/value row whose value is already trusted HTML.
fn kv_html(label: &str, value_html: &str) -> String {
    format!("<div><b>{}</b>{}</div>", escape_html(label), value_html)
}

fn opt_count(n: Option<usize>) -> String {
    match n {
        Some(n) => n.to_string(),
        None => "No limit".to_string(),
    }
}

/// The package detail page for one selected version.
pub fn detail_page(
    urls: &UrlBuilder,
    packages: &[Package],
    selected: &Package,
    readme: Option<&str>,
    primary_client: &str,
    has_symbols: bool,
) -> String {
    let id = escape_html(&selected.id);
    let version = selected.normalized_version();
    let lower = selected.lower_id();

    // Version list (newest first), linking each to its own detail page.
    let mut versions = String::from("<ul class=\"versions\">");
    let mut ordered: Vec<&Package> = packages.iter().collect();
    ordered.sort_by(|a, b| b.version.cmp(&a.version));
    for p in ordered {
        let v = p.normalized_version();
        let sel = if v == version { " class=\"sel\"" } else { "" };
        versions.push_str(&format!(
            "<li><span><a{sel} href=\"{href}\">{dv}</a>{badges}</span>\
             <span class=\"muted\">{dls} downloads</span></li>",
            href =
                escape_html(&urls.app(&format!("/packages/{}/{}", enc_path(&lower), enc_path(&v)))),
            dv = escape_html(&v),
            badges = status_badges(p),
            dls = group_digits(p.downloads as i64),
        ));
    }
    versions.push_str("</ul>");

    let main = format!(
        "<nav class=\"crumbs\" aria-label=\"Breadcrumb\">\
         <a href=\"{packages}\">Packages</a> <span aria-hidden=\"true\">/</span> <span>{id}</span></nav>\
         <h1 class=\"title\">{icon}{id}</h1>\
         <div class=\"meta\">{version}{badges} \u{2022} {dl} downloads of this version \u{2022} published {pub}</div>\
         <p>{desc}</p>{tags}{links}{deps}{symbols}",
        packages = escape_html(&urls.app("/packages")),
        icon = render_icon(urls, selected),
        version = escape_html(&version),
        badges = status_badges(selected),
        dl = group_digits(selected.downloads as i64),
        pub = escape_html(&selected.published.format("%Y-%m-%d").to_string()),
        desc = escape_html(&selected.description),
        tags = render_tags(&selected.tags),
        links = render_links(selected),
        deps = render_dependencies(urls, selected),
        symbols = if has_symbols {
            "<p class=\"muted\">\u{1f50e} Debug symbols are available for this package.</p>"
        } else {
            ""
        },
    );

    // Versions right under Install: picking another version is the common
    // next step, and Info repeats much of what the header already says.
    let side = format!(
        "<div class=\"card install\">{install}</div>\
         <div class=\"card\"><h3 class=\"muted\">Versions</h3>{versions}</div>\
         <div class=\"card\"><h3 class=\"muted\">Info</h3>{info}</div>",
        install = render_install(urls, selected, primary_client),
        info = render_info(selected),
    );

    // The readme is a grid item of its own, placed after the sidebar on a
    // narrow screen. Inside the main column it came first there, and a long
    // readme pushed the version list out of reach.
    let readme = render_readme(readme);
    let readme = if readme.is_empty() {
        readme
    } else {
        format!("<div class=\"readme-area\">{readme}</div>")
    };
    let body = format!(
        "<div class=\"grid detail\"><div class=\"content\">{main}</div>\
         <div class=\"side\">{side}</div>{readme}</div>"
    );
    layout(
        urls,
        &format!("{} {} \u{2014} YANuget", selected.id, version),
        "",
        "",
        &body,
    )
}

/// Prerelease / unlisted status badges for a version (empty when stable+listed).
fn status_badges(p: &Package) -> String {
    let mut out = String::new();
    if p.is_prerelease() {
        out.push_str(" <span class=\"badge pre\">prerelease</span>");
    }
    if !p.listed {
        out.push_str(" <span class=\"badge un\">unlisted</span>");
    }
    out
}

fn render_install(urls: &UrlBuilder, p: &Package, primary_client: &str) -> String {
    // Assembled from raw values and escaped once, at output. Escaping here as
    // well would double-encode: the page would show `&amp;amp;` and the copy
    // button — which reads `innerText`, undoing exactly one level — would put a
    // command carrying a literal `&amp;` on the clipboard.
    let idx = urls.service_index();
    let id = &p.id;
    let ver = p.normalized_version();

    let choco = (
        "Chocolatey",
        format!("choco install {id} --version {ver} --source {idx}"),
    );
    let dotnet = (
        "dotnet CLI",
        format!("dotnet add package {id} --version {ver} --source {idx}"),
    );
    let nuget = (
        "nuget.exe",
        format!("nuget install {id} -Version {ver} -Source {idx}"),
    );

    let mut snippets = match primary_client {
        "dotnet" => vec![dotnet, choco, nuget],
        "nuget" => vec![nuget, choco, dotnet],
        _ => vec![choco, dotnet, nuget],
    };
    // The first snippet is highlighted as the primary one.
    let mut out = String::new();
    for (i, (label, cmd)) in snippets.drain(..).enumerate() {
        let cls = if i == 0 { " class=\"primary\"" } else { "" };
        out.push_str(&format!(
            "<div{cls}><h3>{label}</h3><div class=\"snip\">\
             <button type=\"button\" class=\"copy\" aria-label=\"Copy command\">Copy</button>\
             <pre><code>{cmd}</code></pre></div></div>",
            cmd = escape_html(&cmd),
        ));
    }
    out
}

fn render_info(p: &Package) -> String {
    let mut rows = String::from("<div class=\"kv\">");
    rows.push_str(&format!(
        "<div><b>Version</b>{}</div>",
        escape_html(&p.normalized_version())
    ));
    if !p.authors.is_empty() {
        rows.push_str(&format!(
            "<div><b>Authors</b>{}</div>",
            escape_html(&p.authors.join(", "))
        ));
    }
    if let Some(lic) = p.license_expression.as_deref().or(p.license_url.as_deref()) {
        rows.push_str(&format!("<div><b>License</b>{}</div>", escape_html(lic)));
    }
    rows.push_str(&format!(
        "<div><b>Size</b>{}</div>",
        escape_html(&human_size(p.package_size))
    ));
    rows.push_str(&format!("<div><b>Downloads</b>{}</div>", p.downloads));
    let types = package_type_names(&p.package_types);
    if !types.is_empty() {
        rows.push_str(&format!("<div><b>Type</b>{}</div>", escape_html(&types)));
    }
    rows.push_str("</div>");
    rows
}

fn render_links(p: &Package) -> String {
    let mut links = Vec::new();
    let mut add = |url: &str, label: &str| {
        if let Some(safe) = safe_href(url) {
            links.push(format!(
                "<a href=\"{}\" rel=\"nofollow noopener\">{label}</a>",
                escape_html(safe)
            ));
        }
    };
    if let Some(u) = &p.project_url {
        add(u, "Project");
    }
    if let Some(u) = &p.repository_url {
        add(u, "Repository");
    }
    if let Some(u) = &p.license_url {
        add(u, "License");
    }
    if links.is_empty() {
        String::new()
    } else {
        format!("<p class=\"links\">{}</p>", links.join(" \u{2022} "))
    }
}

fn render_dependencies(urls: &UrlBuilder, p: &Package) -> String {
    if p.dependencies.is_empty() {
        return String::new();
    }
    let mut out = String::from("<h3 class=\"muted\">Dependencies</h3>");
    for group in &p.dependencies {
        let tfm = group
            .target_framework
            .as_deref()
            .unwrap_or("All frameworks");
        out.push_str(&format!("<p class=\"meta\">{}</p>", escape_html(tfm)));
        if group.dependencies.is_empty() {
            out.push_str("<p class=\"muted\">No dependencies</p>");
            continue;
        }
        out.push_str("<table class=\"deps\">");
        for d in &group.dependencies {
            out.push_str(&format!(
                "<tr><td><a href=\"{href}\">{id}</a></td><td class=\"muted\">{range}</td></tr>",
                href = escape_html(
                    &urls.app(&format!("/packages/{}", enc_path(&d.id.to_lowercase())))
                ),
                id = escape_html(&d.id),
                range = escape_html(d.version_range.as_deref().unwrap_or("")),
            ));
        }
        out.push_str("</table>");
    }
    out
}

/// The package's embedded icon, when it has one.
///
/// The image is served from this origin by [`crate::web`], which sniffs the
/// bytes and refuses anything that is not a raster format — so the gallery's
/// `img-src 'self'` policy is enough here. `loading="lazy"` keeps a long list
/// of packages from fetching every icon up front.
fn render_icon(urls: &UrlBuilder, p: &Package) -> String {
    if !p.has_embedded_icon {
        return String::new();
    }
    let src = urls.app(&format!(
        "/packages/{}/{}/icon",
        enc_path(&p.lower_id()),
        enc_path(&p.normalized_version().to_lowercase()),
    ));
    format!(
        "<img class=\"picon\" src=\"{}\" alt=\"\" loading=\"lazy\" decoding=\"async\">",
        escape_html(&src)
    )
}

/// How much of a readme the detail page renders inline.
///
/// A readme is package-supplied and highly compressible, so a small upload can
/// carry a very large one — and the detail page is served on every view, to
/// anyone who can read the feed. Rendering it whole turns one cheap push into a
/// permanently expensive response, so the page shows a generous prefix and
/// points at the package itself for the rest.
const MAX_RENDERED_README_BYTES: usize = 64 * 1024;

fn render_readme(readme: Option<&str>) -> String {
    match readme {
        Some(text) if !text.trim().is_empty() => {
            let (shown, truncated) = truncate_bytes(text, MAX_RENDERED_README_BYTES);
            let notice = if truncated {
                "<p class=\"muted\">Readme truncated; the full text is inside the package.</p>"
            } else {
                ""
            };
            format!(
                "<h3 class=\"muted\">Readme</h3><div class=\"card readme\">{}</div>{notice}",
                escape_html(shown)
            )
        }
        _ => String::new(),
    }
}

/// Cut `s` to at most `max` bytes without splitting a character, reporting
/// whether anything was dropped.
fn truncate_bytes(s: &str, max: usize) -> (&str, bool) {
    if s.len() <= max {
        return (s, false);
    }
    // Walk back to the nearest character boundary; `is_char_boundary` is true at
    // 0, so this always terminates.
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

fn render_tags(tags: &[String]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut out = String::from("<div class=\"tags\">");
    for t in tags {
        out.push_str(&format!("<span class=\"tag\">{}</span>", escape_html(t)));
    }
    out.push_str("</div>");
    out
}

fn package_type_names(types: &[PackageType]) -> String {
    types
        .iter()
        .map(|t| t.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Percent-encode a path segment for use in our own `/packages/...` URLs.
fn enc_path(segment: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    const KEEP: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
        .remove(b'.')
        .remove(b'-')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(segment, KEEP).to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('\u{2026}');
        t
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    /// The CSP pins the inline `<style>` and `<script>` by SHA-256, so a page
    /// whose inline content drifts from the policy silently loses its styling
    /// and its copy buttons. Extract both from a real rendered page and check
    /// the policy actually covers them.
    #[test]
    fn install_snippets_are_escaped_exactly_once() {
        // A base URL may legitimately contain `&`. Escaping the parts and then
        // the assembled command again shows `&amp;amp;` on the page, and the
        // copy button (which reads `innerText`, undoing one level) would put a
        // literal `&amp;` on the clipboard — a command that does not work.
        let urls = super::UrlBuilder::new("https://host.test/a&b");
        let mut p = sample();
        p.id = "Contoso.Utils".into();
        let html = super::render_install(&urls, &p, "choco");

        assert!(
            html.contains("https://host.test/a&amp;b/v3/index.json"),
            "expected a singly-escaped URL, got: {html}"
        );
        assert!(
            !html.contains("&amp;amp;"),
            "command was escaped twice: {html}"
        );
    }

    #[test]
    fn a_huge_readme_does_not_become_a_huge_page() {
        // A readme is package-supplied and compresses well, so a small upload
        // can carry a very large one. The detail page is served on every view,
        // so rendering it whole turns one push into a permanent cost.
        let huge = "A".repeat(super::MAX_RENDERED_README_BYTES * 4);
        let rendered = super::render_readme(Some(&huge));
        assert!(
            rendered.len() < super::MAX_RENDERED_README_BYTES * 2,
            "rendered {} bytes from a {} byte readme",
            rendered.len(),
            huge.len()
        );
        assert!(rendered.contains("Readme truncated"));

        // An ordinary readme is untouched and unannotated.
        let small = super::render_readme(Some("# Hello\n\nSome docs."));
        assert!(small.contains("Some docs."));
        assert!(!small.contains("truncated"));
    }

    #[test]
    fn truncation_never_splits_a_character() {
        // Cutting at a byte offset inside a multi-byte character would panic on
        // slicing; the boundary walk has to handle it.
        let s = "\u{00e9}".repeat(100); // two bytes each
        for max in 0..s.len() {
            let (cut, truncated) = super::truncate_bytes(&s, max);
            assert!(cut.len() <= max);
            assert_eq!(truncated, s.len() > max);
            assert!(s.starts_with(cut));
        }
    }

    #[test]
    fn credentials_in_an_upstream_url_are_not_displayed() {
        assert_eq!(
            super::redact_userinfo("https://ci:s3cret@feed.example.com/v3/index.json"),
            "https://***@feed.example.com/v3/index.json"
        );
        // A password containing an `@` still redacts fully (the *last* `@` in
        // the authority separates userinfo from host).
        assert_eq!(
            super::redact_userinfo("https://ci:p@ss@feed.example.com/v3/index.json"),
            "https://***@feed.example.com/v3/index.json"
        );
        // No credentials, no change.
        assert_eq!(
            super::redact_userinfo("https://api.nuget.org/v3/index.json"),
            "https://api.nuget.org/v3/index.json"
        );
        // A path containing `@` is not mistaken for userinfo.
        assert_eq!(
            super::redact_userinfo("https://host/feeds/@scope/index.json"),
            "https://host/feeds/@scope/index.json"
        );
        assert_eq!(super::redact_userinfo("not a url"), "not a url");
    }

    #[test]
    fn csp_hashes_cover_the_inline_assets_the_page_emits() {
        let urls = super::UrlBuilder::new("https://host");
        let html = super::settings_page(
            &urls,
            &crate::config::Config::default(),
            &feed_ctx(None, None),
        );

        for (open, close) in [("<style>", "</style>"), ("<script>", "</script>")] {
            let start = html.find(open).expect("inline block present") + open.len();
            let end = html[start..].find(close).expect("closing tag") + start;
            let hash = super::csp_hash(&html[start..end]);
            assert!(
                super::CSP.contains(&hash),
                "CSP does not cover the emitted {open} block (expected {hash})\nCSP: {}",
                *super::CSP
            );
        }
    }

    use super::*;

    #[test]
    fn escapes_dangerous_characters() {
        assert_eq!(
            escape_html("<script>\"&'"),
            "&lt;script&gt;&quot;&amp;&#39;"
        );
    }

    #[test]
    fn group_digits_inserts_separators() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(42), "42");
        assert_eq!(group_digits(1234), "1,234");
        assert_eq!(group_digits(1234567), "1,234,567");
    }

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(25 * 1024 * 1024 * 1024), "25.0 GB");
    }

    #[test]
    fn install_snippet_orders_primary_first() {
        let urls = UrlBuilder::new("https://nuget.example.com");
        let p = sample();
        let choco_first = render_install(&urls, &p, "choco");
        assert!(choco_first.find("Chocolatey").unwrap() < choco_first.find("dotnet CLI").unwrap());
        assert!(choco_first.contains("choco install Contoso.Utils --version 1.0.0"));
        let dotnet_first = render_install(&urls, &p, "dotnet");
        assert!(
            dotnet_first.find("dotnet CLI").unwrap() < dotnet_first.find("Chocolatey").unwrap()
        );
    }

    #[test]
    fn detail_page_escapes_package_fields() {
        let urls = UrlBuilder::new("https://host");
        let mut p = sample();
        p.description = "<img src=x onerror=alert(1)>".into();
        let html = detail_page(&urls, std::slice::from_ref(&p), &p, None, "choco", false);
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;img src=x"));
    }

    #[test]
    fn safe_href_allows_only_expected_schemes() {
        assert_eq!(
            safe_href("https://example.com/x"),
            Some("https://example.com/x")
        );
        assert_eq!(
            safe_href("  http://example.com  "),
            Some("http://example.com")
        );
        assert_eq!(
            safe_href("HTTPS://Example.com"),
            Some("HTTPS://Example.com")
        );
        assert_eq!(
            safe_href("mailto:dev@example.com"),
            Some("mailto:dev@example.com")
        );
        assert_eq!(safe_href("javascript:alert(1)"), None);
        // Browsers strip control characters before resolving the scheme; so do we.
        assert_eq!(safe_href("java\tscript:alert(1)"), None);
        assert_eq!(safe_href("data:text/html,<script>alert(1)</script>"), None);
        assert_eq!(safe_href("//evil.example.com"), None);
        assert_eq!(safe_href("not a url"), None);
    }

    #[test]
    fn render_links_drops_dangerous_url_schemes() {
        let mut p = sample();
        p.project_url = Some("javascript:alert(1)".into());
        p.repository_url = Some("https://example.com/repo".into());
        p.license_url = Some("data:text/html,<script>alert(1)</script>".into());
        let html = render_links(&p);
        assert!(!html.contains("javascript:"));
        assert!(!html.contains("data:"));
        assert!(html.contains("https://example.com/repo"));
    }

    #[test]
    fn detail_page_marks_prerelease_and_unlisted() {
        let urls = UrlBuilder::new("https://host");
        let mut p = sample();
        p.version = crate::version::NuGetVersion::parse("2.0.0-rc.1").unwrap();
        p.listed = false;
        let html = detail_page(&urls, std::slice::from_ref(&p), &p, None, "choco", true);
        assert!(html.contains("badge pre"));
        assert!(html.contains("badge un"));
        assert!(html.contains("Debug symbols are available"));
    }

    #[test]
    fn the_detail_page_keeps_the_versions_within_reach() {
        // The sticky install card (over 500 px tall) covered Info and Versions
        // while scrolling, and on a phone Versions came after the whole readme.
        let urls = UrlBuilder::new("https://host");
        let p = sample();
        let html = detail_page(
            &urls,
            std::slice::from_ref(&p),
            &p,
            Some("A long readme."),
            "choco",
            false,
        );
        assert!(!STYLE.contains("sticky"));
        let at = |needle: &str| {
            html.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {html}"))
        };
        let (install, versions) = (at("class=\"card install\""), at(">Versions<"));
        let (info, readme) = (at(">Info<"), at("<div class=\"readme-area\">"));
        assert!(
            install < versions && versions < info && info < readme,
            "{html}"
        );
        // The readme is a grid item of its own, not part of the main column.
        let content_end = at("<div class=\"side\">");
        assert!(readme > content_end, "{html}");
    }

    fn page_of(ids: &[&str]) -> crate::database::SearchPage {
        let groups = ids
            .iter()
            .map(|id| crate::database::SearchGroup {
                packages: vec![{
                    let mut p = sample();
                    p.id = (*id).into();
                    p
                }],
            })
            .collect::<Vec<_>>();
        crate::database::SearchPage {
            total_hits: ids.len() as i64,
            groups,
        }
    }

    /// A gallery request on a server whose configured page size is 20.
    fn view(query: &str, skip: i64, take: i64) -> GalleryView<'_> {
        GalleryView {
            query,
            skip,
            take,
            default_take: 20,
            ..Default::default()
        }
    }

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
        let html = feeds_index_page(&[
            ("stable".into(), "/stable".into()),
            ("dev".into(), "/dev".into()),
        ]);
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
        };
        assert_eq!(group.latest().normalized_version(), "2.0.0-beta");
        assert_eq!(group.headline().normalized_version(), "1.9.0");

        // A package that has only ever shipped pre-releases still shows one.
        group.packages = vec![versioned("0.1.0-alpha")];
        assert_eq!(group.headline().normalized_version(), "0.1.0-alpha");

        let page = crate::database::SearchPage {
            groups: vec![crate::database::SearchGroup {
                packages: vec![versioned("1.9.0"), versioned("2.0.0-beta")],
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
            html.contains("dotnet restore --source https://nuget.example.com/v3/index.json"),
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
    fn the_palette_adapts_to_a_light_browser_theme() {
        // Every colour has to come from a custom property, or a light-themed
        // browser gets dark text on dark chrome in whatever was left hardcoded.
        let style = STYLE;
        let vars = style
            .split_once("*{box-sizing:border-box}")
            .expect("variable block precedes the rules")
            .0;
        assert!(vars.contains("prefers-color-scheme:light"), "{vars}");
        let rules = style
            .split_once("*{box-sizing:border-box}")
            .expect("rules follow the variable block")
            .1;
        assert!(
            !rules.contains('#'),
            "hardcoded colour outside the variable block: {rules}"
        );
    }

    #[test]
    fn form_controls_use_the_page_font_and_a_visible_border() {
        // Browsers give form controls a font of their own (Arial on Windows),
        // and the card border token is only ~1.4:1 against the page, too faint
        // to show where a field is. Controls use `--ctl`, set in both themes.
        assert!(STYLE.contains("button,input,select{font-family:inherit}"));
        assert!(STYLE.contains("input[type=search]{flex:1;padding:9px 12px;border-radius:6px;border:1px solid var(--ctl)"));
        let (dark, light) = STYLE
            .split_once("prefers-color-scheme:light")
            .expect("a light block");
        assert!(dark.contains("--ctl:#"), "{dark}");
        let light_vars = light
            .split_once("*{box-sizing:border-box}")
            .expect("variables before rules")
            .0;
        assert!(light_vars.contains("--ctl:#"), "{light_vars}");
    }

    #[test]
    fn gallery_chrome_loads_no_external_assets() {
        // An empty gallery page (no package-provided links) must reference no
        // external assets: all CSS/JS is inline and the favicon is a data URI.
        let urls = UrlBuilder::new("https://host");
        let html = gallery_page(&urls, &page_of(&[]), &view("", 0, 20));
        for needle in [
            "googleapis",
            "gstatic",
            "cdn.",
            "unpkg",
            "jsdelivr",
            "cdnjs",
            "<script src",
            "stylesheet",
        ] {
            assert!(!html.contains(needle), "external asset reference: {needle}");
        }
        // The offline building blocks are present.
        assert!(html.contains("<style>"));
        assert!(html.contains("rel=\"icon\" href=\"data:image/svg+xml,"));
    }

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
        };
        let html = stats_page(&urls, &stats, &page_of(&["Top.Pkg"]), &[sample()]);
        assert!(html.contains("Statistics"));
        assert!(html.contains("1,234")); // grouped downloads
        assert!(html.contains("Top.Pkg"));
        assert!(html.contains("Recently published"));
    }

    fn feed_ctx(api: Option<&str>, admin: Option<&str>) -> super::super::FeedContext {
        super::super::FeedContext {
            name: "default".into(),
            prefix: String::new(),
            auth: crate::auth::ApiKeyAuth::new(api.map(str::to_string)),
            read_auth: crate::auth::ReadAuth::new(None),
            admin: crate::auth::AdminAuth::new(admin.map(str::to_string)),
            allow_overwrite: crate::config::OverwriteMode::Disabled,
            hard_delete_enabled: false,
            requires_approval: false,
            promotes_to: None,
            mirror: None,
            license_policy: crate::config::LicensePolicyConfig::default(),
            retention: crate::config::RetentionConfig::default(),
        }
    }

    fn feed_version(p: Package, pending: bool, flagged: bool) -> crate::database::FeedVersion {
        crate::database::FeedVersion {
            package: p,
            pending,
            flagged,
            flag_reason: flagged.then(|| "license MIT is blocked".to_string()),
        }
    }

    #[test]
    fn settings_page_hides_secrets_and_shows_admin_link() {
        let urls = UrlBuilder::new("https://host");
        let config = crate::config::Config::default();
        let feed = feed_ctx(Some("super-secret"), Some("admin-secret"));
        let html = settings_page(&urls, &config, &feed);
        assert!(html.contains("Required (API key)"));
        assert!(!html.contains("super-secret"));
        assert!(!html.contains("admin-secret"));
        assert!(html.contains("/admin")); // admin link present when configured

        let no_admin = feed_ctx(None, None);
        assert!(!settings_page(&urls, &config, &no_admin).contains("admin area"));
    }

    #[test]
    fn admin_pages_render_actions() {
        let urls = UrlBuilder::new("https://host");
        let dash = admin_dashboard_page(&urls, &["Contoso.Utils".into()]);
        assert!(dash.contains("/admin/packages/contoso.utils"));

        let mut disabled = sample();
        disabled.enabled = false;
        let versions = vec![
            feed_version(sample(), false, false),
            feed_version(disabled, false, false),
        ];
        let pkg = admin_package_page(&urls, "Contoso.Utils", &versions, None, "tok");
        assert!(pkg.contains("/disable"));
        assert!(pkg.contains("/enable"));
        assert!(pkg.contains("/delete"));
        assert!(pkg.contains("badge un")); // the disabled one
        assert!(pkg.contains("badge ok")); // the active one
    }

    #[test]
    fn admin_page_shows_pending_and_promote() {
        let urls = UrlBuilder::new("https://host");
        let versions = vec![feed_version(sample(), true, true)];
        let pkg = admin_package_page(&urls, "Contoso.Utils", &versions, Some("stable"), "tok");
        assert!(pkg.contains("/approve"));
        assert!(pkg.contains("/promote"));
        assert!(pkg.contains("pending"));
        assert!(pkg.contains("flagged"));
    }

    #[test]
    fn feeds_index_lists_feeds() {
        let html = feeds_index_page(&[
            ("stable".into(), "/stable".into()),
            ("dev".into(), "/dev".into()),
        ]);
        assert!(html.contains("href=\"/stable\""));
        assert!(html.contains("/dev/v3/index.json"));
    }

    fn sample() -> Package {
        use crate::version::NuGetVersion;
        use chrono::Utc;
        Package {
            id: "Contoso.Utils".into(),
            version: NuGetVersion::parse("1.0.0").unwrap(),
            listed: true,
            enabled: true,
            authors: vec!["Alice".into()],
            description: "Helpers".into(),
            icon_url: None,
            license_url: None,
            license_expression: Some("MIT".into()),
            project_url: None,
            repository_url: None,
            repository_type: None,
            min_client_version: None,
            release_notes: None,
            language: None,
            title: None,
            summary: None,
            tags: vec!["util".into()],
            has_readme: false,
            has_embedded_icon: false,
            is_development_dependency: false,
            require_license_acceptance: false,
            is_semver2: false,
            package_size: 2048,
            package_hash: "h".into(),
            package_hash_algorithm: "SHA512".into(),
            published: Utc::now(),
            downloads: 5,
            package_types: vec![],
            dependencies: vec![],
        }
    }
}
