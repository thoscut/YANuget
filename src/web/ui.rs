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
:root{--bg:#0d1117;--card:#161b22;--border:#30363d;--fg:#e6edf3;--muted:#9aa4af;\
--accent:#58a6ff;--accent2:#1f6feb;--code:#010409;--warn:#d29922}\
*{box-sizing:border-box}\
body{margin:0;background:var(--bg);color:var(--fg);\
font:15px/1.55 -apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif}\
a{color:var(--accent);text-decoration:none}a:hover{text-decoration:underline}\
a:focus-visible,button:focus-visible,input:focus-visible{outline:2px solid var(--accent);outline-offset:2px}\
.vh{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);border:0}\
.skip{position:absolute;left:-999px;top:0;background:var(--accent2);color:#fff;padding:8px 12px;border-radius:0 0 6px 0;z-index:10}\
.skip:focus{left:0}\
header{background:var(--card);border-bottom:1px solid var(--border);padding:14px 0}\
.wrap{max-width:980px;margin:0 auto;padding:0 20px}\
header .wrap{display:flex;align-items:center;gap:16px}\
.logo{font-weight:700;font-size:20px;color:var(--fg)}\
.logo span{color:var(--accent)}\
form.search{flex:1;display:flex;gap:8px}\
input[type=search]{flex:1;padding:9px 12px;border-radius:6px;border:1px solid var(--border);\
background:var(--bg);color:var(--fg);font-size:15px;min-height:44px}\
input[type=search]:focus{border-color:var(--accent)}\
button{padding:9px 16px;border-radius:6px;border:1px solid var(--accent2);\
background:var(--accent2);color:#fff;font-size:15px;cursor:pointer;min-height:44px}\
button:hover{background:#2d76f0}\
@media(max-width:560px){header .wrap{flex-wrap:wrap}form.search{flex:1 0 100%}}\
main{padding:26px 0 60px}\
.card{background:var(--card);border:1px solid var(--border);border-radius:10px;\
padding:18px 20px;margin:0 0 14px}\
.card h2{margin:0 0 4px;font-size:18px;overflow-wrap:anywhere}\
.meta{color:var(--muted);font-size:13px;margin:2px 0}\
.crumbs{font-size:13px;color:var(--muted);margin:0 0 6px}\
.tags{margin-top:8px;list-style:none;padding:0;display:flex;flex-wrap:wrap}\
.tag{display:inline-block;background:#21262d;border:1px solid var(--border);border-radius:20px;\
padding:1px 10px;font-size:12px;color:var(--muted);margin:0 4px 4px 0}\
.badge{display:inline-block;font-size:11px;padding:0 7px;border-radius:20px;border:1px solid var(--border);vertical-align:middle}\
.badge.pre{color:var(--warn);border-color:var(--warn)}\
.badge.un{color:var(--muted)}\
.muted{color:var(--muted)}\
.grid{display:grid;grid-template-columns:1fr 280px;gap:22px}\
@media(max-width:760px){.grid{grid-template-columns:1fr}}\
@media(min-width:761px){.side .install{position:sticky;top:20px}}\
h1.title{font-size:26px;margin:0 0 2px;overflow-wrap:anywhere}\
pre{background:var(--code);border:1px solid var(--border);border-radius:8px;padding:12px 14px;\
overflow:auto;font-size:13px;margin:6px 0}\
code{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}\
.install h3{margin:14px 0 4px;font-size:13px;text-transform:uppercase;letter-spacing:.4px;color:var(--muted)}\
.install .primary h3{color:var(--accent)}\
.install pre{white-space:pre-wrap;overflow-wrap:anywhere}\
.snip{position:relative}\
.snip .copy{position:absolute;top:6px;right:6px;padding:3px 10px;font-size:12px;min-height:0;\
background:#21262d;border:1px solid var(--border);color:var(--fg)}\
.snip .copy:hover{background:#30363d}\
.snip pre{padding-right:64px}\
.versions{list-style:none;margin:0;padding:0;max-height:340px;overflow:auto}\
.versions li{display:flex;justify-content:space-between;align-items:center;gap:8px;padding:5px 0;border-bottom:1px solid var(--border)}\
.versions a{min-height:32px;display:inline-flex;align-items:center}\
.versions a.sel{font-weight:700}\
table.deps{width:100%;border-collapse:collapse;font-size:13px}\
table.deps td{padding:3px 8px 3px 0}\
.readme{white-space:pre-wrap;word-wrap:break-word;overflow-wrap:anywhere}\
.links a[rel~=nofollow]::after{content:\" \u{2197}\";color:var(--muted);font-size:11px}\
.empty{text-align:center;color:var(--muted);padding:60px 0}\
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
.badge.ok{color:#3fb950;border-color:#3fb950}\
.atbl{width:100%;border-collapse:collapse}\
.atbl th,.atbl td{text-align:left;padding:8px 10px;border-bottom:1px solid var(--border);font-size:14px;vertical-align:middle}\
.atbl th{color:var(--muted);font-weight:500;font-size:12px;text-transform:uppercase;letter-spacing:.4px}\
.actions{display:flex;gap:8px;flex-wrap:wrap}\
.actions form{margin:0}\
.actions button{padding:5px 12px;font-size:13px;min-height:0;background:#21262d;border:1px solid var(--border);color:var(--fg)}\
.actions button:hover{background:#30363d}\
.actions button.danger{border-color:#b62324;color:#ff7b72}\
.actions button.danger:hover{background:#b62324;color:#fff}\
footer{border-top:1px solid var(--border);color:var(--muted);font-size:13px;padding:18px 0}\
footer a[aria-current=page]{color:var(--fg);font-weight:600}\
";

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
    let cur = |name: &str| {
        if name == active {
            " aria-current=\"page\""
        } else {
            ""
        }
    };
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<link rel=\"icon\" href=\"data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'%20viewBox='0%200%2032%2032'%3E%3Crect%20width='32'%20height='32'%20rx='6'%20fill='%23512bd4'/%3E%3Ctext%20x='16'%20y='22'%20font-size='15'%20font-family='sans-serif'%20font-weight='700'%20fill='white'%20text-anchor='middle'%3EYN%3C/text%3E%3C/svg%3E\">\
<title>{title}</title><style>{STYLE}</style></head><body>\
<a class=\"skip\" href=\"#main\">Skip to content</a>\
<header><div class=\"wrap\">\
<a class=\"logo\" href=\"{home}\">YA<span>NuGet</span></a>\
<form class=\"search\" action=\"{packages}\" method=\"get\" role=\"search\">\
<label for=\"q\" class=\"vh\">Search packages</label>\
<input id=\"q\" type=\"search\" name=\"q\" placeholder=\"Search packages\u{2026}\" value=\"{q}\" autocomplete=\"off\">\
<button type=\"submit\">Search</button></form>\
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

/// The gallery / search-results page. `skip`/`take` drive pagination.
pub fn gallery_page(
    urls: &UrlBuilder,
    page: &crate::database::SearchPage,
    query: &str,
    skip: i64,
    take: i64,
) -> String {
    let body = if page.groups.is_empty() {
        let what = if query.trim().is_empty() {
            "No packages have been published yet.".to_string()
        } else {
            format!("No packages match \u{201c}{}\u{201d}.", escape_html(query))
        };
        let clear = if query.trim().is_empty() {
            "<p class=\"muted\">Push one with <code>dotnet nuget push</code> or \
             <code>choco push</code>.</p>"
                .to_string()
        } else {
            format!(
                "<p><a href=\"{}\">Clear search and browse all packages</a></p>",
                escape_html(&urls.app("/packages"))
            )
        };
        format!("<div class=\"empty\"><p>{what}</p>{clear}</div>")
    } else {
        let mut cards = String::new();
        let heading = if query.trim().is_empty() {
            format!("{} package(s)", page.total_hits)
        } else {
            format!(
                "{} result(s) for \u{201c}{}\u{201d}",
                page.total_hits,
                escape_html(query)
            )
        };
        cards.push_str(&format!("<p class=\"muted\">{heading}</p>"));
        for group in &page.groups {
            let p = group.latest();
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
                 <div class=\"meta\">{dl} downloads{authors}</div>\
                 <p>{desc}</p>{tags}</div>",
                ver = escape_html(&p.normalized_version()),
                dl = group.total_downloads(),
                desc = escape_html(&truncate(&p.description, 240)),
                tags = render_tags(&p.tags),
            ));
        }
        cards.push_str(&pager(
            urls,
            query,
            skip,
            take,
            page.groups.len() as i64,
            page.total_hits,
        ));
        cards
    };
    layout(urls, "YANuget", query, "", &body)
}

/// Previous/next pagination control for the gallery.
fn pager(urls: &UrlBuilder, query: &str, skip: i64, take: i64, shown: i64, total: i64) -> String {
    let take = take.max(1);
    let skip = skip.max(0);
    // Only render when there is more than one page worth of results.
    if total <= take && skip == 0 {
        return String::new();
    }
    let q = enc_path(query);
    let base = urls.app("/packages");
    let prev = (skip - take).max(0);
    let has_prev = skip > 0;
    let has_next = skip + shown < total;
    let next = skip + take;
    let from = if shown == 0 { 0 } else { skip + 1 };
    let to = skip + shown;
    let link = |target: i64, enabled: bool, label: &str| {
        if enabled {
            format!("<a class=\"btn\" href=\"{base}?q={q}&skip={target}\">{label}</a>")
        } else {
            format!("<span class=\"btn\" aria-disabled=\"true\">{label}</span>")
        }
    };
    format!(
        "<nav class=\"pager\" aria-label=\"Pagination\">{prev_l}\
         <span class=\"muted\">{from}\u{2013}{to} of {total}</span>{next_l}</nav>",
        prev_l = link(prev, has_prev, "\u{2190} Previous"),
        next_l = link(next, has_next, "Next \u{2192}"),
    )
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
            policy.push_str(&kv("Upstream", m.upstream()));
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
            dl = p.downloads,
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
    layout(&urls, "Feeds \u{2014} YANuget", "", "", &body)
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
             <span class=\"muted\">{dls} dl</span></li>",
            href =
                escape_html(&urls.app(&format!("/packages/{}/{}", enc_path(&lower), enc_path(&v)))),
            dv = escape_html(&v),
            badges = status_badges(p),
            dls = p.downloads,
        ));
    }
    versions.push_str("</ul>");

    let main = format!(
        "<nav class=\"crumbs\" aria-label=\"Breadcrumb\">\
         <a href=\"{packages}\">Packages</a> <span aria-hidden=\"true\">/</span> <span>{id}</span></nav>\
         <h1 class=\"title\">{id}</h1>\
         <div class=\"meta\">{version}{badges} \u{2022} {dl} downloads \u{2022} published {pub}</div>\
         <p>{desc}</p>{tags}{links}{deps}{symbols}{readme}",
        packages = escape_html(&urls.app("/packages")),
        version = escape_html(&version),
        badges = status_badges(selected),
        dl = selected.downloads,
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
        readme = render_readme(readme),
    );

    let side = format!(
        "<div class=\"card install\">{install}</div>\
         <div class=\"card\"><h3 class=\"muted\">Info</h3>{info}</div>\
         <div class=\"card\"><h3 class=\"muted\">Versions</h3>{versions}</div>",
        install = render_install(urls, selected, primary_client),
        info = render_info(selected),
    );

    let body = format!(
        "<div class=\"grid\"><div class=\"content\">{main}</div><div class=\"side\">{side}</div></div>"
    );
    layout(urls, &format!("{} {}", selected.id, version), "", "", &body)
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
    let idx = escape_html(&urls.service_index());
    let id = escape_html(&p.id);
    let ver = escape_html(&p.normalized_version());

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

fn render_readme(readme: Option<&str>) -> String {
    match readme {
        Some(text) if !text.trim().is_empty() => {
            format!(
                "<h3 class=\"muted\">Readme</h3><div class=\"card readme\">{}</div>",
                escape_html(text)
            )
        }
        _ => String::new(),
    }
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

    #[test]
    fn gallery_lists_cards_and_paginates() {
        let urls = UrlBuilder::new("https://host");
        // Two of three results shown -> pager with a Next link.
        let mut page = page_of(&["Pkg.A", "Pkg.B"]);
        page.total_hits = 3;
        let html = gallery_page(&urls, &page, "", 0, 2);
        assert!(html.contains("Pkg.A"));
        assert!(html.contains("/packages/pkg.b"));
        assert!(html.contains("class=\"pager\""));
        assert!(html.contains("skip=2")); // next page

        // Empty result for a query offers a "clear search" link.
        let empty = gallery_page(&urls, &page_of(&[]), "zzz", 0, 20);
        assert!(empty.contains("Clear search"));
    }

    #[test]
    fn gallery_chrome_loads_no_external_assets() {
        // An empty gallery page (no package-provided links) must reference no
        // external assets: all CSS/JS is inline and the favicon is a data URI.
        let urls = UrlBuilder::new("https://host");
        let html = gallery_page(&urls, &page_of(&[]), "", 0, 20);
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
