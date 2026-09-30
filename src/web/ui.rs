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
use crate::database::{PackageFile, SearchSort, TagCount};
use crate::models::{Package, PackageType};
use crate::nuget::UrlBuilder;

/// Where the embedded gallery font is served from, as a literal so the
/// stylesheet below can be one compile-time constant (its CSP hash depends on
/// it). The name carries the font's version: the file is cached as immutable,
/// so a different font has to arrive under a different URL.
macro_rules! font_url {
    () => {
        "/_assets/fonts/atkinson-hyperlegible-next-2.001-latin-wght.woff2"
    };
}
pub(super) const FONT_URL: &str = font_url!();

/// The gallery's styling, inlined so a page needs nothing but itself and the
/// one font file this server also serves. It works fully offline.
///
/// The look is a warehouse's: pages sit on a grey floor, the package page is a
/// shipping label (die-cut corners, heavy carbon rules, one cell per field),
/// and the one colour is floor-marking yellow, spent on the action that
/// matters on a page and on the highlighter a hovered link gets. The typeface,
/// Atkinson Hyperlegible Next, was drawn to keep `l`, `1` and `I` (and `0` and
/// `O`) apart, which is most of what reading a package id or a version asks.
const STYLE: &str = concat!(
    "@font-face{font-family:'Atkinson Hyperlegible Next';font-style:normal;font-weight:200 800;\
font-display:swap;src:url(",
    font_url!(),
    ") format('woff2');\
unicode-range:U+0000-00FF,U+0131,U+0152-0153,U+02BB-02BC,U+02C6,U+02DA,U+02DC,U+0304,U+0308,\
U+0329,U+2000-206F,U+20AC,U+2122,U+2191,U+2193,U+2212,U+2215,U+FEFF,U+FFFD}\
:root{color-scheme:dark light;\
--floor:#131920;--stock:#1b232c;--ink:#e6eaee;--pencil:#9ca8b4;--rule:#2e3945;--code:#0e1318;\
--hivis:#ffd100;--onhivis:#16191d;--focus:#ffd100;--ctl:#6b7885;\
--warn:#e0a526;--ok:#4cc06a;--danger:#e5484d;--ondanger:#16191d;--dangerfg:#ff8a85}\
@media(prefers-color-scheme:light){:root{\
--floor:#e6e9ec;--stock:#fff;--ink:#16191d;--pencil:#56606b;--rule:#c5cbd2;--code:#f2f4f6;\
--hivis:#ffd100;--onhivis:#16191d;--focus:#16191d;--ctl:#7a8591;\
--warn:#8a5a00;--ok:#1b7a3a;--danger:#c0262d;--ondanger:#fff;--dangerfg:#c0262d}}\
*{box-sizing:border-box}\
html{-webkit-text-size-adjust:100%;text-size-adjust:100%}\
button,input,select{font-family:inherit}\
body{margin:0;background:var(--floor);color:var(--ink);\
font:16px/1.55 'Atkinson Hyperlegible Next',system-ui,-apple-system,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;\
font-variant-numeric:tabular-nums}\
a{color:inherit;text-decoration:underline;text-decoration-thickness:1px;text-underline-offset:.2em}\
a:hover{background:var(--hivis);color:var(--onhivis);text-decoration:none;\
-webkit-box-decoration-break:clone;box-decoration-break:clone}\
a:focus-visible,button:focus-visible,input:focus-visible,select:focus-visible{outline:3px solid var(--focus);outline-offset:2px}\
.vh{position:absolute;width:1px;height:1px;padding:0;margin:-1px;overflow:hidden;clip:rect(0,0,0,0);border:0}\
.skip{position:absolute;left:-999px;top:0;z-index:10;padding:10px 14px;background:var(--hivis);color:var(--onhivis);font-weight:700}\
.skip:focus{left:0}\
header{background:var(--stock);border-bottom:3px solid var(--ink)}\
.wrap{max-width:1120px;margin:0 auto;padding:0 24px}\
header .wrap{display:flex;flex-wrap:wrap;align-items:center;gap:12px 28px;padding-top:12px;padding-bottom:12px}\
.logo{display:inline-flex;align-items:center;gap:10px;font-size:21px;font-weight:800;letter-spacing:-.01em;text-decoration:none}\
.logo:hover{background:none;color:inherit}\
.logo .box{fill:none;stroke:currentColor;stroke-width:3}.logo .tape{fill:var(--hivis)}\
form.search{flex:1 1 300px;display:flex;gap:8px}\
input[type=search]{flex:1;min-width:0;padding:9px 14px;border-radius:6px;border:2px solid var(--ctl);\
background:var(--floor);color:var(--ink);font-size:16px;min-height:44px}\
input[type=search]:focus{border-color:var(--ink)}\
button{padding:9px 18px;border-radius:6px;border:2px solid var(--ink);background:var(--ink);color:var(--stock);\
font-size:16px;font-weight:700;cursor:pointer;min-height:44px}\
button:hover{background:var(--hivis);color:var(--onhivis)}\
nav.site{display:flex;flex-wrap:wrap;gap:4px 20px;font-size:15px}\
nav.site a{padding:8px 0 5px;border-bottom:3px solid transparent;text-decoration:none}\
nav.site a:hover{background:none;color:inherit;border-bottom-color:var(--hivis)}\
nav.site a[aria-current=page]{font-weight:700;border-bottom-color:var(--ink)}\
@media(max-width:760px){form.search{order:3;flex-basis:100%}nav.site{margin-left:auto}}\
@media(max-width:560px){.wrap{padding:0 16px}header .wrap{gap:8px 16px}nav.site{gap:4px 14px}}\
main{padding:32px 0 72px}main:focus{outline:none}\
h1,h2,h3{line-height:1.2}\
h1.title{margin:0 0 6px;font-size:30px;font-weight:800;letter-spacing:-.01em;overflow-wrap:anywhere}\
h2{margin:0 0 10px;font-size:19px;font-weight:750}\
.card{margin:0 0 36px}\
.meta,.muted{color:var(--pencil)}.meta{font-size:14px}\
.crumbs{margin:0 0 12px;font-size:14px;color:var(--pencil)}\
.bar{display:flex;flex-wrap:wrap;align-items:flex-end;justify-content:space-between;gap:12px 24px;margin:0 0 16px}\
.bar h1{margin:0}\
.sort{display:flex;flex-wrap:wrap;align-items:center;gap:8px 12px;font-size:15px;color:var(--pencil)}\
.seg{display:inline-flex;border:2px solid var(--ink);border-radius:6px;overflow:hidden;background:var(--stock);color:var(--ink)}\
.seg a{display:inline-flex;align-items:center;min-height:40px;padding:0 14px;font-weight:700;text-decoration:none}\
.seg a+a{border-left:2px solid var(--ink)}\
.seg a[aria-current]{background:var(--ink);color:var(--stock)}\
.seg a:focus-visible{outline-offset:-6px}\
.manifest{list-style:none;margin:0;padding:0;border-top:2px solid var(--ink)}\
.pkg{display:grid;grid-template-columns:minmax(0,1fr) auto;gap:2px 40px;padding:18px 0 20px;border-bottom:1px solid var(--rule)}\
.pkg>*{grid-column:1}\
.pkg h2{margin:0;font-size:20px;overflow-wrap:anywhere}\
.pkg h2 a{text-decoration:none}\
.pkg .ver{font-weight:500;color:var(--pencil)}\
.pkg p{margin:4px 0 0;max-width:75ch}\
.pkg .figs{grid-column:2;grid-row:1/span 3;text-align:right;font-size:14px;line-height:1.6;color:var(--pencil)}\
@media(max-width:640px){.pkg{grid-template-columns:minmax(0,1fr)}.pkg .figs{grid-column:1;grid-row:auto;margin-top:6px;text-align:left}}\
.tags{display:flex;flex-wrap:wrap;gap:6px;margin:12px 0 0;padding:0;list-style:none}\
.tag{padding:0 8px;border:1px solid var(--rule);border-radius:4px;background:var(--stock);color:var(--pencil);font-size:13px;line-height:20px}\
a.tag{text-decoration:none}a.tag:hover{border-color:var(--ink)}\
.filter{display:flex;flex-wrap:wrap;gap:4px 20px;margin:-6px 0 16px;font-size:15px}\
.popular{display:flex;flex-wrap:wrap;align-items:center;gap:6px;margin:0 0 18px;font-size:14px;color:var(--pencil)}\
.popular>span{margin-right:6px}.popular .all{margin-left:6px}\
.cloud{display:flex;flex-wrap:wrap;align-items:baseline;gap:8px 22px;max-width:960px;margin:20px 0 0;padding:0;list-style:none}\
.cloud a{font-weight:600;line-height:1.2;text-decoration:none}\
.cloud .n{margin-left:5px;color:var(--pencil);font-size:13px;font-weight:400}\
.cloud .t1{font-size:15px}.cloud .t2{font-size:18px}.cloud .t3{font-size:22px;font-weight:700}\
.cloud .t4{font-size:27px;font-weight:750}.cloud .t5{font-size:33px;font-weight:800;letter-spacing:-.01em}\
.badge{display:inline-block;margin-left:2px;padding:0 6px;border:1.5px solid currentColor;border-radius:4px;\
color:var(--pencil);font-size:12px;font-weight:700;line-height:18px;vertical-align:.15em;white-space:nowrap}\
.badge.pre{color:var(--warn);border-style:dashed}\
.badge.pin{color:var(--ink)}\
.doomed{color:var(--dangerfg)}\
.notice{margin:0 0 24px;padding:12px 16px;background:var(--stock);border:2px solid var(--ink);border-radius:8px;font-weight:600}\
.notice.warn{border-color:var(--warn)}\
.run{margin:18px 0 0}\
.badge.ok{color:var(--ok)}\
.grid{display:grid;grid-template-columns:minmax(0,1fr) 340px;gap:0 40px}\
.detail{grid-template-areas:\"main side\" \"readme side\";grid-template-rows:auto 1fr}\
.detail>.content{grid-area:main;min-width:0}.detail>.side{grid-area:side;min-width:0}.detail>.readme-area{grid-area:readme;min-width:0}\
@media(max-width:880px){.detail{grid-template-columns:minmax(0,1fr);grid-template-areas:\"main\" \"side\" \"readme\";grid-template-rows:auto}\
.detail>.side{margin-top:36px}}\
.label{margin:0 0 22px;background:var(--stock);border:3px solid var(--ink);border-radius:14px;overflow:hidden}\
.label-head{display:flex;align-items:center;gap:16px;padding:20px 24px 18px}\
.label h1{flex:1 1 0;min-width:0;margin:0;font-size:40px;line-height:1.08;letter-spacing:-.02em}\
.label .acts{flex:0 0 auto;display:flex;flex-direction:column;align-items:flex-end;gap:2px;font-size:15px;font-weight:700}\
img.picon{flex:0 0 auto;width:48px;height:48px;object-fit:contain;border-radius:8px}\
.fields{display:flex;flex-wrap:wrap;margin:0 0 0 -2px}\
.fields>div{flex:1 1 9.5rem;min-width:0;padding:10px 16px 12px;border-left:2px solid var(--ink);border-top:2px solid var(--ink)}\
.fields dt{font-size:13px;color:var(--pencil)}\
.fields dd{margin:0;font-size:17px;font-weight:700;overflow-wrap:anywhere}\
.fields .wide{flex-basis:100%}\
@media(max-width:560px){.label-head{flex-wrap:wrap;gap:10px 14px;padding:16px 16px 14px}.label h1{font-size:28px}\
.label .acts{flex-basis:100%;flex-direction:row;flex-wrap:wrap;gap:4px 18px}img.picon{width:36px;height:36px}.fields>div{padding:8px 12px 10px}}\
.lede{max-width:65ch;margin:0 0 14px;font-size:18px;line-height:1.5}\
.links{display:flex;flex-wrap:wrap;gap:4px 20px;margin:16px 0 0}\
.links a[rel~=nofollow]::after{content:\" \u{2197}\";font-size:.8em}\
.content h2,.readme-area>h2{margin:32px 0 10px}\
.tfm{margin:16px 0 4px;font-size:15px;font-weight:700}\
table.deps{width:100%;border-collapse:collapse;border-top:2px solid var(--ink);font-size:15px}\
table.deps td{padding:8px 12px 8px 0;border-bottom:1px solid var(--rule);vertical-align:top;overflow-wrap:anywhere}\
table.deps td+td{padding-right:0;text-align:right;white-space:nowrap}\
.install>div,ol.steps>li{display:grid;grid-template-columns:minmax(0,1fr) auto;align-items:end;column-gap:12px}\
.install>div+div{margin-top:20px}\
.install h3{margin:0;padding-bottom:2px;font-size:15px}\
.snip{display:contents}.snip pre{grid-column:1/-1}\
pre{margin:8px 0 0;padding:12px 14px;overflow:auto;background:var(--code);border:1px solid var(--rule);border-radius:6px;font-size:14px;line-height:1.55}\
code{font-family:'Cascadia Mono','Cascadia Code',Consolas,ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.93em}\
pre code{font-size:inherit}\
.install pre,.steps pre{white-space:pre-wrap;overflow-wrap:break-word}\
.install .primary pre{background:var(--stock);border:2px solid var(--ink)}\
.nw{white-space:nowrap}\
.attached{margin:0;padding:0;list-style:none;border-top:2px solid var(--ink)}\
.attached li{padding:10px 0;border-bottom:1px solid var(--rule)}\
.attached .id{font-weight:700;text-decoration:none}\
.attached .sha{margin-top:2px;font-size:13px;overflow-wrap:anywhere}\
.script{display:grid;grid-template-columns:minmax(0,1fr) auto;align-items:end;column-gap:12px;margin-top:18px}\
.script h3{margin:0;font-size:15px}\
.script pre{white-space:pre}\
.copy{min-height:36px;padding:0 14px;background:var(--stock);color:var(--ink);font-size:14px}\
.primary .copy{background:var(--hivis);color:var(--onhivis)}\
.versions{list-style:none;margin:0;padding:0;max-height:360px;overflow:auto;border-top:2px solid var(--ink)}\
.versions li{display:flex;justify-content:space-between;align-items:center;gap:8px;padding:0 2px 0 14px;border-bottom:1px solid var(--rule);font-size:15px}\
.versions li.sel{background:var(--stock);box-shadow:inset 6px 0 0 var(--hivis)}\
.versions a{display:inline-flex;align-items:center;min-height:42px;font-weight:600;text-decoration:none}\
.versions a[aria-current]{font-weight:800}\
.readme{padding:22px 26px;background:var(--stock);border:1px solid var(--rule);border-radius:6px;\
font-size:15px;line-height:1.6;white-space:pre-wrap;overflow-wrap:anywhere}\
@media(max-width:560px){.readme{padding:16px}}\
.kv{border-top:2px solid var(--ink);font-size:15px}\
.kv div{display:flex;gap:16px;padding:9px 0;border-bottom:1px solid var(--rule)}\
.kv b{flex:0 0 14em;color:var(--pencil);font-weight:500}\
.kv a,.kv code{overflow-wrap:anywhere}\
@media(max-width:560px){.kv div{flex-direction:column;gap:0}.kv b{flex:0 0 auto}}\
.stats{display:grid;grid-template-columns:repeat(4,1fr);margin:0 0 40px;background:var(--stock);\
border:3px solid var(--ink);border-radius:14px;overflow:hidden}\
.stat{min-width:0;margin:-2px 0 0 -2px;padding:12px 20px 16px;border-left:2px solid var(--ink);border-top:2px solid var(--ink)}\
.stat .l{font-size:14px;color:var(--pencil)}\
.stat .n{font-size:30px;font-weight:800;line-height:1.15;overflow-wrap:anywhere}\
@media(max-width:760px){.stats{grid-template-columns:repeat(2,1fr)}}@media(max-width:560px){.stat{padding:10px 14px 12px}.stat .n{font-size:24px}}\
.lists{display:grid;grid-template-columns:1fr 1fr;gap:0 40px}\
.lists>.card{margin:0}\
@media(max-width:760px){.lists{grid-template-columns:1fr;gap:28px}}\
.rank{list-style:none;margin:0;padding:0;border-top:2px solid var(--ink)}\
.rank li{display:flex;flex-wrap:wrap;justify-content:space-between;gap:2px 12px;padding:9px 0;border-bottom:1px solid var(--rule)}\
.rank li>a,.rank .id{font-weight:600;text-decoration:none;overflow-wrap:anywhere}\
.pager{display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:12px;margin:24px 0 0}\
.btn{display:inline-flex;align-items:center;gap:6px;min-height:44px;padding:0 16px;background:var(--stock);\
border:2px solid var(--ink);border-radius:6px;font-weight:700;text-decoration:none}\
.btn[aria-disabled=true]{opacity:.35;pointer-events:none}\
.pager-go{flex:1 0 100%;display:flex;flex-wrap:wrap;align-items:center;justify-content:space-between;gap:12px}\
.pager-go form{display:flex;align-items:center;gap:8px;margin:0}\
.pager-go label{font-size:15px;color:var(--pencil)}\
select,.pager-go input{min-height:44px;padding:0 10px;border:2px solid var(--ctl);border-radius:6px;\
background:var(--stock);color:var(--ink);font-size:16px}\
.pager-go input{width:6em}\
.pager-go button,.actions button,.bulk button{padding:0 14px;background:var(--stock);color:var(--ink)}\
.pager-go button:hover,.actions button:hover,.bulk button:hover{background:var(--hivis);color:var(--onhivis)}\
.hero{max-width:62ch;margin:0 0 28px}\
.hero h1{margin:0 0 8px;font-size:40px;font-weight:800;line-height:1.08;letter-spacing:-.02em}\
.hero p{margin:0;font-size:18px;color:var(--pencil)}\
ol.steps{max-width:860px;margin:0 0 28px;padding:0;list-style:none;counter-reset:step}\
ol.steps>li{position:relative;grid-template-rows:40px auto;padding:0 0 26px 58px;counter-increment:step}\
ol.steps>li::before{content:counter(step);position:absolute;left:0;top:0;display:grid;place-items:center;\
width:40px;height:40px;border-radius:6px;background:var(--hivis);color:var(--onhivis);font-size:20px;font-weight:800}\
.steps h2{align-self:center;margin:0;font-size:18px}\
.empty{max-width:62ch;padding:16px 0 40px}\
.empty h1.title{font-size:36px}\
.scroll{overflow-x:auto}\
.atbl{width:100%;border-collapse:collapse;border-top:2px solid var(--ink)}\
.atbl th,.atbl td{padding:10px 14px 10px 0;border-bottom:1px solid var(--rule);text-align:left;font-size:15px;vertical-align:middle}\
.atbl th{color:var(--pencil);font-size:14px;font-weight:600}\
.atbl .pick{width:2.75em;padding-left:6px}\
input[type=checkbox]{width:20px;height:20px;margin:0;accent-color:var(--ink)}\
.actions{display:flex;flex-wrap:wrap;gap:6px}\
.actions form{margin:0}\
.actions button,.bulk button{min-height:38px;font-size:14px;white-space:nowrap}\
.actions .btn{min-height:38px;padding:0 12px;font-size:14px}\
button.danger{background:var(--stock);border-color:var(--danger);color:var(--dangerfg)}\
button.danger:hover{background:var(--danger);border-color:var(--danger);color:var(--ondanger)}\
.bulk{display:flex;flex-wrap:wrap;align-items:center;gap:10px 12px;margin:18px 0 0;padding:14px 16px;\
background:var(--stock);border:2px solid var(--ink);border-radius:10px}\
.bulk [role=status]{flex-basis:100%;color:var(--dangerfg);font-weight:700}.bulk [role=status]:empty{display:none}\
.bulk .to{display:flex;flex-wrap:wrap;align-items:center;gap:8px;padding-left:12px;border-left:1px solid var(--rule)}\
.bulk select{min-height:38px;font-size:15px}\
@media(max-width:640px){.vers,.vers tbody,.plan,.plan tbody,.files,.files tbody{display:block}\
.files thead{display:none}.files tbody tr{display:block;padding:10px 0;border-bottom:1px solid var(--rule)}\
.files td{display:inline;padding:0 10px 0 0;border:0}.files td:last-child{display:block;padding:6px 0 0}\
.vers thead tr{display:flex;align-items:center;padding:8px 0;border-bottom:1px solid var(--rule)}\
.vers thead th{padding:0 0 0 6px;border:0}.vers thead th:not(.pick){display:none}\
.vers tbody tr{display:grid;grid-template-columns:2.75em minmax(0,1fr) auto auto;align-items:center;gap:10px 10px;\
padding:12px 0;border-bottom:1px solid var(--rule)}\
.vers td{padding:0;border:0}.vers td:last-child{grid-column:2/-1}\
.plan thead{display:none}.plan tbody tr{display:block;padding:10px 0;border-bottom:1px solid var(--rule)}\
.plan td{display:inline;padding:0 10px 0 0;border:0}.plan td:nth-child(4){display:block;padding:2px 0}\
.bulk .to{flex-basis:100%;padding-left:0;border-left:0}.bulk .danger{margin-left:0}}\
.title+.card{margin-top:22px}\
.bulk .danger{margin-left:auto}\
footer{padding:20px 0 32px;border-top:1px solid var(--rule);color:var(--pencil);font-size:14px}\
footer .wrap{display:flex;flex-wrap:wrap;justify-content:space-between;gap:6px 24px}\
"
);

/// The inline SVG favicon, as a data URI so the page loads no external asset:
/// the header's taped box, filled.
const FAVICON: &str = "data:image/svg+xml,%3Csvg%20xmlns='http://www.w3.org/2000/svg'\
%20viewBox='0%200%2032%2032'%3E%3Crect%20width='32'%20height='32'%20rx='6'%20fill='%2316191d'/%3E\
%3Crect%20x='12'%20width='8'%20height='32'%20fill='%23ffd100'/%3E%3C/svg%3E";

/// The header's mark: a box, taped shut. Coloured by the stylesheet (`.box`,
/// `.tape`), since the CSP allows no inline `style`.
const LOGO_MARK: &str =
    "<svg width=\"26\" height=\"26\" viewBox=\"0 0 26 26\" aria-hidden=\"true\" \
focusable=\"false\"><rect class=\"tape\" x=\"10\" y=\"1.5\" width=\"6\" height=\"23\"/>\
<rect class=\"box\" x=\"1.5\" y=\"1.5\" width=\"23\" height=\"23\" rx=\"4\"/></svg>";

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
/// behaviour. The buttons are rendered `hidden` and shown only when the
/// clipboard API exists: without JavaScript, or on a feed served over plain
/// HTTP (where browsers withhold `navigator.clipboard`), a visible button did
/// nothing. The `<pre>` stays selectable either way. A copy is announced
/// through the page's one `role="status"` region, because the button's own
/// "Copied" text is hidden behind its `aria-label`.
///
/// Kept separate from its `<script>` wrapper because the CSP hash below must be
/// taken over exactly this text — the element's content, not the tags.
/// It also carries the admin area's destructive-action confirmation. That used
/// to be an inline `onsubmit=` attribute, which the CSP below cannot whitelist
/// by hash — so the prompt is delegated from here off a `data-confirm`
/// attribute instead, keeping the guard rail and the policy both intact. The
/// attribute is read off the button that submitted first, because the admin
/// page's selection form has a Delete button beside harmless ones.
///
/// The rest is the admin selection: a "select every version" box (hidden
/// without JavaScript, where it could do nothing), and a note instead of a
/// round trip when an action is chosen with nothing selected.
const COPY_SCRIPT_BODY: &str = "if(navigator.clipboard)\
document.querySelectorAll('.copy').forEach(function(b){b.hidden=false});\
document.querySelectorAll('.all').forEach(function(a){a.hidden=false});\
document.addEventListener('click',function(e){\
var b=e.target.closest('.copy');if(!b)return;\
var c=b.parentNode.querySelector('code');if(!c||!navigator.clipboard)return;\
navigator.clipboard.writeText(c.innerText).then(function(){\
var s=document.getElementById('copied');if(s)s.textContent='Copied to the clipboard';\
var o=b.textContent;b.textContent='Copied';\
setTimeout(function(){b.textContent=o;if(s)s.textContent=''},1200)})});\
document.addEventListener('change',function(e){\
var a=e.target;if(!a.classList||!a.classList.contains('all'))return;\
document.querySelectorAll('input[name=v]').forEach(function(c){c.checked=a.checked})});\
document.addEventListener('submit',function(e){\
var f=e.target,s=e.submitter;\
if(f.id==='bulk'&&!document.querySelector('input[name=v]:checked')){e.preventDefault();\
var n=document.getElementById('bulk-note');if(n)n.textContent='Select at least one version first.';return}\
var m=(s&&s.getAttribute('data-confirm'))||(f.getAttribute&&f.getAttribute('data-confirm'));\
if(m&&!confirm(m))e.preventDefault()});";

/// The `Content-Security-Policy` served with every gallery/admin page.
///
/// The gallery renders package-supplied metadata (descriptions, readmes, links,
/// dependency ids). [`escape_html`] and [`safe_href`] are the primary defence;
/// this policy is the backstop that keeps an escaping bug from becoming script
/// execution. `default-src 'none'` denies everything not listed, and the only
/// inline style/script permitted are the two the server itself emits, pinned by
/// SHA-256 — an injected `<script>` has a different hash and will not run. The
/// one font comes from this origin, like everything else.
pub static CSP: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "default-src 'none'; img-src 'self' data:; font-src 'self'; style-src '{style}'; \
         script-src '{script}'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
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

/// The running server's version, shown in every page's footer.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the shared chrome needs to know about the page it wraps.
#[derive(Clone, Copy, Default)]
struct Nav<'a> {
    /// The current header item (`"stats"`, `"settings"`, `"admin"`, or `""`).
    active: &'a str,
    /// Whether this feed has an admin area to link to.
    admin: bool,
}

/// Wrap a page body in the shared layout (head, header bar, footer).
fn layout(urls: &UrlBuilder, title: &str, nav: Nav, body: &str) -> String {
    layout_with_chrome(urls, title, "", nav, body, Chrome::Feed, "")
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
/// gallery uses it to keep a chosen page size and order across a new search.
fn layout_with_chrome(
    urls: &UrlBuilder,
    title: &str,
    query: &str,
    nav: Nav,
    body: &str,
    chrome: Chrome,
    search_hidden: &str,
) -> String {
    let head = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
<link rel=\"icon\" href=\"{FAVICON}\">\
<link rel=\"preload\" href=\"{FONT_URL}\" as=\"font\" type=\"font/woff2\" crossorigin>\
<title>{title}</title><style>{STYLE}</style></head><body>\
<a class=\"skip\" href=\"#main\">Skip to content</a>",
        title = escape_html(title),
    );
    if chrome == Chrome::Root {
        return format!(
            "{head}<header><div class=\"wrap\">\
<a class=\"logo\" href=\"/\">{LOGO_MARK}YANuget</a></div></header>\
<main id=\"main\" tabindex=\"-1\"><div class=\"wrap\">{body}</div></main>\
<footer><div class=\"wrap\"><span>Served by YANuget {VERSION}</span></div></footer>\
<script>{COPY_SCRIPT_BODY}</script></body></html>"
        );
    }
    let item = |name: &str, path: &str, label: &str| {
        let current = if name == nav.active {
            " aria-current=\"page\""
        } else {
            ""
        };
        format!(
            "<a href=\"{}\"{current}>{label}</a>",
            escape_html(&urls.app(path))
        )
    };
    let mut links = item("tags", "/tags", "Tags");
    links.push_str(&item("stats", "/stats", "Stats"));
    links.push_str(&item("settings", "/settings", "Settings"));
    links.push_str(&item("docs", "/docs/", "Docs"));
    if nav.admin {
        links.push_str(&item("admin", "/admin", "Admin"));
    }
    format!(
        "{head}<header><div class=\"wrap\">\
<a class=\"logo\" href=\"{home}\">{LOGO_MARK}YANuget</a>\
<form class=\"search\" action=\"{packages}\" method=\"get\" role=\"search\">\
<label for=\"q\" class=\"vh\">Search packages</label>\
<input id=\"q\" type=\"search\" name=\"q\" placeholder=\"Search packages\u{2026}\" value=\"{q}\" autocomplete=\"off\">\
{search_hidden}<button type=\"submit\">Search</button></form>\
<nav class=\"site\" aria-label=\"Site\">{links}</nav>\
</div></header>\
<main id=\"main\" tabindex=\"-1\"><div class=\"wrap\">{body}</div></main>\
<footer><div class=\"wrap\"><span>Served by YANuget {VERSION}</span>\
<span>Package source: <a href=\"{idx}\">{idx}</a></span></div></footer>\
<div id=\"copied\" class=\"vh\" role=\"status\"></div>\
<script>{COPY_SCRIPT_BODY}</script></body></html>",
        q = escape_html(query),
        home = escape_html(&urls.app("/")),
        packages = escape_html(&urls.app("/packages")),
        idx = escape_html(&urls.service_index()),
    )
}

/// A styled error page for the gallery, so a browser never sees a bare JSON
/// error body.
///
/// The text is derived from the status alone. The JSON body it replaces is
/// deliberately not parsed through: a 5xx message is generic on purpose (the
/// underlying I/O, SQL or upstream detail goes to the log), and re-rendering an
/// error string into HTML is a needless place to get escaping wrong.
pub fn error_page(urls: &UrlBuilder, status: axum::http::StatusCode, admin: bool) -> String {
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
    layout(
        urls,
        &format!("{heading} \u{2014} YANuget"),
        Nav {
            admin,
            ..Nav::default()
        },
        &body,
    )
}

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
fn tag_href(urls: &UrlBuilder, tag: &str) -> String {
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

    let mut server = String::from("<div class=\"kv wide\">");
    server.push_str(&kv("YANuget version", VERSION));
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
    let mut policy = String::from("<div class=\"kv wide\">");
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
    let mut retention = String::from("<div class=\"kv wide\">");
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
    if feed.admin.is_enabled() {
        retention.push_str(&format!(
            "<p><a href=\"{}\">See what the next cleanup would delete</a></p>",
            escape_html(&urls.app("/admin/retention"))
        ));
    }

    let fc = &config.files;
    let mut files = String::from("<div class=\"kv wide\">");
    files.push_str(&kv("Attached files", on_off(fc.enabled)));
    if fc.enabled {
        files.push_str(&kv(
            "Uploads",
            if feed.auth.is_enabled() {
                "Accepted with the push key"
            } else {
                "Refused: this feed has no push key"
            },
        ));
        files.push_str(&kv(
            "Largest file",
            &match config.max_file_size_bytes() {
                Some(n) => human_size(n),
                None => "Unlimited".to_string(),
            },
        ));
        files.push_str(&kv("File types", &fc.allowed_extensions.join(", ")));
        files.push_str(&kv(
            "Unfinished uploads kept",
            &format!("{} h", fc.upload_expiry_hours),
        ));
        // Whether there is an inbox, not where: the path is infrastructure.
        files.push_str(&kv(
            "SSH inbox",
            if fc.inbox_dir.is_some() {
                "Enabled"
            } else {
                "Disabled"
            },
        ));
    }
    files.push_str("</div>");

    // Said even when the admin area is off: otherwise nothing on any page
    // tells an operator that disabling, deleting and moving versions exist.
    let admin = if feed.admin.is_enabled() {
        format!(
            "<div class=\"card\"><h2>Administration</h2>\
             <p>Approve, disable, delete and move package versions between feeds in the \
             <a href=\"{}\">admin area</a>. Sign in with the admin key.</p></div>",
            escape_html(&urls.app("/admin"))
        )
    } else {
        "<div class=\"card\"><h2>Administration</h2>\
         <p>Administration is turned off for this feed. Set <code>admin_api_key</code> \
         (or <code>YANUGET_ADMIN_API_KEY</code>) to approve, disable, delete and move \
         package versions from the gallery.</p></div>"
            .to_string()
    };

    let body = format!(
        "<h1 class=\"title\">Settings</h1>\
         <p class=\"muted\">Read-only overview of this feed's policy. \
         Secrets and storage paths are not shown.</p>\
         <div class=\"card\"><h2>Server</h2>{server}</div>\
         <div class=\"card\"><h2>Mirror &amp; policy</h2>{policy}</div>\
         <div class=\"card\"><h2>Retention</h2>{retention}</div>\
         <div class=\"card\"><h2>Attached files</h2>{files}</div>\
         <div class=\"card\"><h2>Endpoints</h2><div class=\"kv wide\">\
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
    layout(
        urls,
        "Settings \u{2014} YANuget",
        Nav {
            active: "settings",
            admin: feed.admin.is_enabled(),
        },
        &body,
    )
}

/// The admin area's place in the header: current, and present.
const ADMIN_NAV: Nav<'static> = Nav {
    active: "admin",
    admin: true,
};

/// The admin dashboard: every package id, linking to its management page.
pub fn admin_dashboard_page(urls: &UrlBuilder, ids: &[String]) -> String {
    let retention = format!(
        "<a href=\"{}\">Retention: preview and clean up</a>",
        escape_html(&urls.app("/admin/retention"))
    );
    let body = if ids.is_empty() {
        format!(
            "<div class=\"bar\"><h1 class=\"title\">Admin</h1>{retention}</div>\
             <p class=\"muted\">No packages published yet.</p>"
        )
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
            "<div class=\"bar\"><h1 class=\"title\">Admin</h1>{retention}</div>\
             <p class=\"muted\">Pick a package to approve, disable, pin, delete or move its \
             versions.</p>\
             <div class=\"card\">{list}</div>"
        )
    };
    layout(urls, "Admin \u{2014} YANuget", ADMIN_NAV, &body)
}

/// What the per-package admin page offers besides the versions themselves.
#[derive(Debug, Default, Clone, Copy)]
pub struct AdminPackageExtras<'a> {
    /// The next release ring, when this feed has one.
    pub promote_target: Option<&'a str>,
    /// The other feeds this admin may copy or move versions into; with none,
    /// the page offers neither.
    pub transfer_targets: &'a [String],
    /// What the next retention cleanup would delete of this package.
    pub retention_plan: &'a [crate::retention::Pruned],
    /// Whether files can be attached (`[files].enabled`).
    pub files_enabled: bool,
    /// The files attached to any of the versions.
    pub files: &'a [PackageFile],
}

/// The per-package admin page: every version (incl. disabled, pending and
/// flagged) with moderation actions.
///
/// Each row has its own buttons for the common one-version case. Below the
/// table, one form acts on every ticked version at once — which is also how a
/// whole package is disabled, deleted or moved: tick them all. The boxes sit
/// in the table but belong to that form through their `form` attribute,
/// because a form cannot wrap the rows' own forms.
pub fn admin_package_page(
    urls: &UrlBuilder,
    id: &str,
    versions: &[crate::database::FeedVersion],
    extras: &AdminPackageExtras,
    csrf_token: &str,
) -> String {
    let mut ordered: Vec<&crate::database::FeedVersion> = versions.iter().collect();
    ordered.sort_by(|a, b| b.package.version.cmp(&a.package.version));

    let lower = id.to_lowercase();
    let action = |v: &str, op: &str| {
        escape_html(&urls.app(&format!(
            "/admin/packages/{}/{}/{}",
            enc_path(&lower),
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
    let button = |v: &str, op: &str, label: &str| {
        format!(
            "<form method=\"post\" action=\"{}\">{csrf}<button type=\"submit\">{label}</button></form>",
            action(v, op)
        )
    };

    let mut rows = String::new();
    for fv in &ordered {
        let p = &fv.package;
        let v = p.normalized_version();
        let dv = escape_html(&v);
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
        if fv.pinned {
            status.push_str(" <span class=\"badge pin\">pinned</span>");
        }

        let mut actions = String::new();
        // Only what clients could fetch too: a disabled or pending version is
        // withheld by the download endpoint, and a link to a 404 helps nobody.
        if p.enabled && !fv.pending {
            actions.push_str(&format!(
                "<a class=\"btn\" href=\"{}\" aria-label=\"Download {dv}\">Download</a>",
                escape_html(&urls.package_download(&lower, &v)),
            ));
        }
        if fv.pending {
            actions.push_str(&button(&v, "approve", "Approve"));
        }
        if let Some(target) = extras.promote_target {
            actions.push_str(&button(
                &v,
                "promote",
                &format!("Promote \u{2192} {}", escape_html(target)),
            ));
        }
        // Enable/disable and pin/unpin toggle with the current state.
        if p.enabled {
            actions.push_str(&button(&v, "disable", "Disable"));
        } else {
            actions.push_str(&button(&v, "enable", "Enable"));
        }
        if fv.pinned {
            actions.push_str(&button(&v, "unpin", "Unpin"));
        } else {
            actions.push_str(&button(&v, "pin", "Pin"));
        }
        // A pin keeps a version from retention, not from an admin: say so
        // before deleting one.
        let confirm = if fv.pinned {
            format!(
                "{id} {v} is pinned, which only keeps it from retention. Remove it from this \
                 feed anyway? If no other feed uses it, the files are deleted."
            )
        } else {
            format!(
                "Remove {id} {v} from this feed? If no other feed uses it, the files are deleted."
            )
        };
        actions.push_str(&format!(
            "<form method=\"post\" action=\"{a}\" data-confirm=\"{confirm}\">{csrf}\
             <button type=\"submit\" class=\"danger\">Delete</button></form>",
            a = action(&v, "delete"),
            confirm = escape_html(&confirm),
        ));

        let mut notes = match (fv.flagged, fv.flag_reason.as_deref()) {
            (true, Some(r)) => format!("<div class=\"meta\">{}</div>", escape_html(r)),
            _ => String::new(),
        };
        if let Some(planned) = extras
            .retention_plan
            .iter()
            .find(|planned| planned.version == p.version)
        {
            notes.push_str(&format!(
                "<div class=\"meta doomed\">The next cleanup deletes this: {}.</div>",
                escape_html(&planned.reason.describe())
            ));
        }
        rows.push_str(&format!(
            "<tr><td class=\"pick\"><input type=\"checkbox\" name=\"v\" value=\"{dv}\" \
             form=\"bulk\" aria-label=\"Select {dv}\"></td>\
             <td>{pre}<span class=\"nw\">{dv}</span>{notes}</td><td>{status}</td>\
             <td class=\"muted\">{dl}</td>\
             <td><div class=\"actions\">{actions}</div></td></tr>",
            pre = if p.is_prerelease() {
                "<span class=\"badge pre\">pre</span> "
            } else {
                ""
            },
            dl = group_digits(p.downloads as i64),
        ));
    }

    // The selection form. "Enable" comes first on purpose: pressing Enter in a
    // form submits it with its first button, and that must never be Delete.
    let approve = if ordered.iter().any(|fv| fv.pending) {
        "<button type=\"submit\" name=\"op\" value=\"approve\">Approve</button>"
    } else {
        ""
    };
    let transfer = if extras.transfer_targets.is_empty() {
        String::new()
    } else {
        let options: String = extras
            .transfer_targets
            .iter()
            .map(|t| format!("<option value=\"{t}\">{t}</option>", t = escape_html(t)))
            .collect();
        format!(
            "<span class=\"to\"><label for=\"bulk-target\">Feed</label>\
             <select id=\"bulk-target\" name=\"target\">{options}</select>\
             <button type=\"submit\" name=\"op\" value=\"copy\">Copy to feed</button>\
             <button type=\"submit\" name=\"op\" value=\"move\">Move to feed</button></span>"
        )
    };
    let bulk = format!(
        "<form id=\"bulk\" class=\"bulk\" method=\"post\" action=\"{act}\" \
         aria-labelledby=\"bulk-l\">{csrf}\
         <span id=\"bulk-l\"><b>With the selected versions</b></span>\
         <button type=\"submit\" name=\"op\" value=\"enable\">Enable</button>\
         <button type=\"submit\" name=\"op\" value=\"disable\">Disable</button>{approve}\
         <button type=\"submit\" name=\"op\" value=\"pin\">Pin</button>\
         <button type=\"submit\" name=\"op\" value=\"unpin\">Unpin</button>{transfer}\
         <button type=\"submit\" name=\"op\" value=\"delete\" class=\"danger\" \
         data-confirm=\"{confirm}\">Delete</button>\
         <span id=\"bulk-note\" role=\"status\"></span></form>",
        act = escape_html(&urls.app(&format!("/admin/packages/{}", enc_path(&lower)))),
        confirm = escape_html(&format!(
            "Remove the selected versions of {id} from this feed, pinned or not? \
             Versions no other feed holds are deleted for good."
        )),
    );

    let body = format!(
        "<nav class=\"crumbs\" aria-label=\"Breadcrumb\">\
         <a href=\"{admin}\">Admin</a> <span aria-hidden=\"true\">/</span> <span>{eid}</span></nav>\
         <div class=\"bar\"><h1 class=\"title\">{eid}</h1>\
         <a href=\"{gallery}\">Open in the gallery</a></div>\
         <p class=\"muted\">Disabled and pending versions are hidden from clients and not \
         downloadable. A pinned version is never deleted by retention. Delete removes this \
         feed's membership; moving keeps the files, since the other feed then holds them.</p>\
         <div class=\"card\"><div class=\"scroll\"><table class=\"atbl vers\">\
         <thead><tr><th class=\"pick\"><input type=\"checkbox\" class=\"all\" \
         aria-label=\"Select every version\" hidden></th>\
         <th>Version</th><th>Status</th><th>Downloads</th><th>Actions</th></tr></thead>\
         <tbody>{rows}</tbody></table></div>{bulk}</div>{files}",
        admin = escape_html(&urls.app("/admin")),
        gallery = escape_html(&urls.app(&format!("/packages/{}", enc_path(&lower)))),
        eid = escape_html(id),
        files = admin_files(urls, id, extras, &csrf),
    );
    layout(urls, &format!("Admin \u{2014} {id}"), ADMIN_NAV, &body)
}

/// The admin page's list of a package's attached files, each downloadable and
/// deletable, and how to attach more.
fn admin_files(urls: &UrlBuilder, id: &str, extras: &AdminPackageExtras, csrf: &str) -> String {
    if !extras.files_enabled {
        return String::new();
    }
    let how = format!(
        "<p class=\"muted\">Attach a file with <code>PUT {put}</code> and the push key, resumably \
         with tus at <code>{tus}</code>, or over SSH through the inbox; \
         <a href=\"{docs}\">the documentation</a> has the details.</p>",
        put = escape_html(&urls.app("/api/v2/files/{id}/{version}/{name}")),
        tus = escape_html(&urls.app("/api/v2/uploads")),
        docs = escape_html(&urls.app("/docs/api/#attached-files")),
    );
    if extras.files.is_empty() {
        return format!("<div class=\"card\"><h2>Files</h2><p>No files attached.</p>{how}</div>");
    }
    let mut rows = String::new();
    for f in extras.files {
        let action = escape_html(&urls.app(&format!(
            "/admin/packages/{}/{}/files/{}/delete",
            enc_path(&f.lower_id),
            enc_path(&f.normalized_version),
            enc_path(&f.name)
        )));
        rows.push_str(&format!(
            "<tr><td><span class=\"nw\">{v}</span></td><td>{name}</td><td class=\"muted\">{size}</td>\
             <td><code title=\"{sha}\">{short}\u{2026}</code></td><td class=\"muted\">{dl}</td>\
             <td><div class=\"actions\"><a class=\"btn\" href=\"{href}\" aria-label=\"Download {name}\">Download</a>\
             <form method=\"post\" action=\"{action}\" data-confirm=\"{confirm}\">{csrf}\
             <button type=\"submit\" class=\"danger\">Delete</button></form></div></td></tr>",
            v = escape_html(&f.normalized_version),
            name = escape_html(&f.name),
            size = human_size(f.size),
            sha = escape_html(&f.sha256),
            short = escape_html(&f.sha256[..f.sha256.len().min(12)]),
            dl = group_digits(f.downloads as i64),
            href = escape_html(&urls.file_download(&f.lower_id, &f.normalized_version, &f.name)),
            confirm = escape_html(&format!(
                "Delete {} from {id} {}? Install scripts that fetch it will fail.",
                f.name, f.normalized_version
            )),
        ));
    }
    format!(
        "<div class=\"card\"><h2>Files</h2><div class=\"scroll\"><table class=\"atbl files\">\
         <thead><tr><th>Version</th><th>File</th><th>Size</th><th>SHA-256</th><th>Downloads</th>\
         <th>Actions</th></tr></thead><tbody>{rows}</tbody></table></div>{how}</div>"
    )
}

/// What the retention page says after a cleanup it was asked for.
#[derive(Debug, Clone, Copy)]
pub enum RetentionNotice {
    None,
    /// A cleanup ran and did this.
    Done(crate::retention::Outcome),
    /// The plan changed between looking and clicking; nothing was deleted.
    Changed,
    /// A cleanup was already running; nothing was deleted.
    Busy,
}

/// Everything the retention page shows.
pub struct RetentionView<'a> {
    pub rules: &'a crate::config::RetentionConfig,
    pub last: Option<crate::retention::Report>,
    pub running: bool,
    /// The next cleanup's plan, when there are rules to plan with.
    pub preview: Option<&'a crate::retention::Preview>,
    pub csrf_token: &'a str,
    pub notice: RetentionNotice,
}

/// How many planned deletions the retention page lists before summarising.
const MAX_PREVIEW_ROWS: usize = 500;

/// The retention page: the rules as configured, what the last cleanup did,
/// and exactly what the next one would delete — with a button that deletes
/// that and nothing else.
pub fn admin_retention_page(urls: &UrlBuilder, view: &RetentionView) -> String {
    use crate::retention::Trigger;
    let rules = view.rules;
    let on_off = |b: bool| if b { "On" } else { "Off" };
    let limit = |n: Option<usize>| match n {
        Some(n) => format!("{n} per package"),
        None => "No limit".to_string(),
    };
    let notice = match view.notice {
        RetentionNotice::None => String::new(),
        RetentionNotice::Done(o) => {
            let errors = if o.errors > 0 {
                format!(
                    " {} could not be deleted; the details are in the server log.",
                    o.errors
                )
            } else {
                String::new()
            };
            format!(
                "<p class=\"notice\" role=\"status\">Deleted {} version{}, freeing {}.{errors}</p>",
                o.deleted,
                plural(o.deleted as i64),
                human_size(o.freed),
            )
        }
        RetentionNotice::Changed => "<p class=\"notice warn\" role=\"status\">The feed changed \
            since you looked, so nothing was deleted. Below is the list as it is now.</p>"
            .to_string(),
        RetentionNotice::Busy => "<p class=\"notice warn\" role=\"status\">A cleanup was already \
            running, so nothing was deleted. Look again once it has finished.</p>"
            .to_string(),
    };

    let mut kv_rows = String::from("<div class=\"kv wide\">");
    kv_rows.push_str(&kv("Retention", on_off(rules.enabled)));
    kv_rows.push_str(&kv(
        "Newest stable versions kept",
        &limit(rules.keep_latest_stable),
    ));
    kv_rows.push_str(&kv(
        "Newest pre-release versions kept",
        &limit(rules.keep_latest_prerelease),
    ));
    kv_rows.push_str(&kv(
        "Maximum age",
        &match rules.max_age_days {
            Some(d) => format!("{d} day{}", plural(d as i64)),
            None => "No limit".to_string(),
        },
    ));
    kv_rows.push_str(&kv(
        "Scheduled cleanup",
        &if rules.enabled && rules.interval_hours > 0 {
            format!("Every {} h", rules.interval_hours)
        } else {
            "Off".to_string()
        },
    ));
    kv_rows.push_str(&kv(
        "After each push",
        on_off(rules.enabled && rules.prune_on_push),
    ));
    kv_rows.push_str("</div>");

    let last = if view.running {
        "<p>A cleanup is running right now.</p>".to_string()
    } else {
        match view.last {
            None => {
                "<p class=\"muted\">No cleanup has run since the server started.</p>".to_string()
            }
            Some(r) => format!(
                "<p>Last cleanup {when} ({how}): deleted {n} version{s}, freed {freed}.</p>",
                when = escape_html(&r.finished.format("%Y-%m-%d %H:%M UTC").to_string()),
                how = match r.trigger {
                    Trigger::Schedule => "scheduled",
                    Trigger::Manual => "from this page",
                },
                n = r.outcome.deleted,
                s = plural(r.outcome.deleted as i64),
                freed = human_size(r.outcome.freed),
            ),
        }
    };

    let next = match view.preview {
        None => "<p>No rules are set, so a cleanup deletes nothing. Set \
                 <code>keep_latest_stable</code>, <code>keep_latest_prerelease</code> or \
                 <code>max_age_days</code> to give it some.</p>"
            .to_string(),
        Some(plan) if plan.planned.is_empty() => {
            "<p>Nothing to delete: every version is within the rules.</p>".to_string()
        }
        Some(plan) => {
            let mut table = String::from(
                "<div class=\"scroll\"><table class=\"atbl plan\"><thead><tr><th>Package</th>\
                 <th>Version</th><th>Published</th><th>Why</th><th>Frees</th></tr></thead><tbody>",
            );
            for p in plan.planned.iter().take(MAX_PREVIEW_ROWS) {
                table.push_str(&format!(
                    "<tr><td><a class=\"id\" href=\"{href}\">{id}</a></td>\
                     <td><span class=\"nw\">{v}</span></td><td class=\"muted\">{when}</td>\
                     <td>{why}</td><td class=\"muted\">{frees}</td></tr>",
                    href = escape_html(&urls.app(&format!(
                        "/admin/packages/{}",
                        enc_path(&p.id.to_lowercase())
                    ))),
                    id = escape_html(&p.id),
                    v = escape_html(&p.version.normalized()),
                    when = escape_html(&p.published.format("%Y-%m-%d").to_string()),
                    why = escape_html(&p.reason.describe()),
                    frees = if p.frees > 0 {
                        human_size(p.frees)
                    } else {
                        "Kept by another feed".to_string()
                    },
                ));
            }
            table.push_str("</tbody></table></div>");
            let more = plan.planned.len().saturating_sub(MAX_PREVIEW_ROWS);
            let more = if more > 0 {
                format!("<p class=\"muted\">And {more} more not listed here.</p>")
            } else {
                String::new()
            };
            let n = plan.planned.len();
            let action = if rules.enabled {
                format!(
                    "<form class=\"run\" method=\"post\" action=\"{act}\" data-confirm=\"{confirm}\">\
                     <input type=\"hidden\" name=\"{CSRF_FIELD}\" value=\"{csrf}\">\
                     <input type=\"hidden\" name=\"plan\" value=\"{fp}\">\
                     <button type=\"submit\" class=\"danger\">{label}</button>\
                     </form>",
                    label = if n == 1 {
                        "Delete this version now".to_string()
                    } else {
                        format!("Delete these {n} versions now")
                    },
                    act = escape_html(&urls.app("/admin/retention/run")),
                    confirm = escape_html(&format!(
                        "Delete {n} version{} from this feed now? Versions no other feed \
                         holds are deleted for good.",
                        plural(n as i64)
                    )),
                    csrf = escape_html(view.csrf_token),
                    fp = escape_html(&plan.fingerprint()),
                )
            } else {
                "<p class=\"muted\">Retention is off (<code>enabled = false</code>), so nothing \
                 is deleted. To clean up only from this page, set <code>enabled = true</code> \
                 and <code>interval_hours = 0</code>.</p>"
                    .to_string()
            };
            format!(
                "<p>{n} version{s} would be deleted, freeing {freed}.</p>{table}{more}{action}",
                s = plural(n as i64),
                freed = human_size(plan.frees()),
            )
        }
    };

    let pinned = match view.preview {
        Some(plan) if !plan.pinned.is_empty() => {
            let mut list = String::from("<ul class=\"rank\">");
            for (id, v) in &plan.pinned {
                list.push_str(&format!(
                    "<li><span><a class=\"id\" href=\"{href}\">{eid}</a> \
                     <span class=\"muted\">{v}</span></span></li>",
                    href = escape_html(
                        &urls.app(&format!("/admin/packages/{}", enc_path(&id.to_lowercase())))
                    ),
                    eid = escape_html(id),
                    v = escape_html(&v.normalized()),
                ));
            }
            list.push_str("</ul>");
            format!("<div class=\"card\"><h2>Pinned, kept regardless</h2>{list}</div>")
        }
        _ => String::new(),
    };

    let body = format!(
        "<nav class=\"crumbs\" aria-label=\"Breadcrumb\">\
         <a href=\"{admin}\">Admin</a> <span aria-hidden=\"true\">/</span> <span>Retention</span></nav>\
         <h1 class=\"title\">Retention</h1>{notice}\
         <div class=\"card\"><h2>Rules</h2>{kv_rows}\
         <p class=\"muted\">Set in the configuration file, under <code>[retention]</code> or a \
         feed's <code>[feeds.retention]</code>. The newest version of every package is always \
         kept, and pinned versions are kept whatever the rules say.</p>{last}</div>\
         <div class=\"card\"><h2>Next cleanup</h2>{next}</div>{pinned}",
        admin = escape_html(&urls.app("/admin")),
    );
    layout(urls, "Retention \u{2014} YANuget", ADMIN_NAV, &body)
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
        Nav::default(),
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

/// What the package page shows about the selected version besides its
/// metadata.
#[derive(Debug, Default, Clone, Copy)]
pub struct Detail<'a> {
    pub readme: Option<&'a str>,
    /// Which install command comes first (`choco`, `dotnet`, `nuget`).
    pub primary_client: &'a str,
    pub has_symbols: bool,
    /// Whether this feed has an admin area: the label then links to the page
    /// that disables, deletes or moves this package's versions.
    pub admin: bool,
    /// Files attached to the selected version.
    pub files: &'a [PackageFile],
}

/// The package detail page for one selected version.
pub fn detail_page(
    urls: &UrlBuilder,
    packages: &[Package],
    selected: &Package,
    detail: &Detail,
) -> String {
    let Detail {
        readme,
        primary_client,
        has_symbols,
        admin,
        files,
    } = *detail;
    let id = escape_html(&selected.id);
    let version = selected.normalized_version();
    let lower = selected.lower_id();

    // Version list (newest first), linking each to its own detail page.
    let mut versions = String::from("<ul class=\"versions\">");
    let mut ordered: Vec<&Package> = packages.iter().collect();
    ordered.sort_by(|a, b| b.version.cmp(&a.version));
    for p in ordered {
        let v = p.normalized_version();
        let (li, current) = if v == version {
            (" class=\"sel\"", " aria-current=\"page\"")
        } else {
            ("", "")
        };
        versions.push_str(&format!(
            "<li{li}><span><a href=\"{href}\"{current}>{dv}</a>{badges}</span>\
             <span class=\"muted\">{dls} download{ds}</span></li>",
            href =
                escape_html(&urls.app(&format!("/packages/{}/{}", enc_path(&lower), enc_path(&v)))),
            dv = escape_html(&v),
            badges = status_badges(p),
            dls = group_digits(p.downloads as i64),
            ds = plural(p.downloads as i64),
        ));
    }
    versions.push_str("</ul>");

    // The package file itself, from the same endpoint clients restore from —
    // so read auth, ranges and caching behave exactly as they do for them.
    let mut manage = format!(
        "<a href=\"{}\">Download .nupkg</a>",
        escape_html(&urls.package_download(&lower, &version))
    );
    if admin {
        manage.push_str(&format!(
            "<a href=\"{}\">Manage versions</a>",
            escape_html(&urls.app(&format!("/admin/packages/{}", enc_path(&lower))))
        ));
    }
    let manage = format!("<div class=\"acts\">{manage}</div>");
    let desc = if selected.description.trim().is_empty() {
        String::new()
    } else {
        format!(
            "<p class=\"lede\">{}</p>",
            escape_html(&selected.description)
        )
    };
    // The label: the id, then one ruled cell per fact about this version. A
    // long id may break after any of its dots, which is where it reads best.
    let main = format!(
        "<nav class=\"crumbs\" aria-label=\"Breadcrumb\">\
         <a href=\"{packages}\">Packages</a> <span aria-hidden=\"true\">/</span> <span>{id}</span></nav>\
         <section class=\"label\" aria-labelledby=\"pkg\"><div class=\"label-head\">{icon}\
         <h1 class=\"title\" id=\"pkg\">{id_breaks}</h1>{manage}</div>\
         <dl class=\"fields\">{fields}</dl></section>\
         {desc}{tags}{links}{attached}{deps}{symbols}",
        packages = escape_html(&urls.app("/packages")),
        icon = render_icon(urls, selected),
        id_breaks = id.replace('.', ".<wbr>"),
        fields = render_fields(selected),
        tags = render_tags(urls, &selected.tags),
        links = render_links(selected),
        attached = render_files(urls, selected, files),
        deps = render_dependencies(urls, selected),
        symbols = if has_symbols {
            "<p class=\"muted\">Debug symbols are available for this package.</p>"
        } else {
            ""
        },
    );

    // Versions right under Install: picking another version is the common
    // next step. The section headings are all `<h2>`, one level under the
    // package name, and the client labels inside Install are `<h3>` under it.
    // A heading that jumps a level reads, to a screen reader user, like a
    // missing section.
    let side = format!(
        "<div class=\"card install\"><h2>Install</h2>{install}</div>\
         <div class=\"card\"><h2>Versions</h2>{versions}</div>",
        install = render_install(urls, selected, primary_client),
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
        Nav {
            admin,
            ..Nav::default()
        },
        &body,
    )
}

/// The files attached to a version, with what a `chocolateyInstall.ps1`
/// needs to fetch and check them.
///
/// The snippet uses BITS, which resumes a dropped transfer by itself and
/// survives a reboot mid-download, and Chocolatey's own `Get-ChecksumValid`,
/// which fails the install on a mismatch. It is assembled raw and escaped
/// once, like the install commands, so the copy button puts exactly the
/// script on the clipboard.
fn render_files(urls: &UrlBuilder, p: &Package, files: &[PackageFile]) -> String {
    if files.is_empty() {
        return String::new();
    }
    let lower = p.lower_id();
    let version = p.normalized_version();
    let mut list = String::from("<ul class=\"attached\">");
    let mut script = format!(
        "$dir = Join-Path $env:TEMP '{}.{}'\nNew-Item -ItemType Directory -Force $dir | Out-Null",
        p.id, version
    );
    for f in files {
        let url = urls.file_download(&lower, &version, &f.name);
        list.push_str(&format!(
            "<li><div><a class=\"id\" href=\"{href}\">{name}</a> \
             <span class=\"muted\">{size}</span></div>\
             <div class=\"sha\"><span class=\"muted\">SHA-256</span> <code>{sha}</code></div></li>",
            href = escape_html(&url),
            name = escape_html(&f.name),
            size = human_size(f.size),
            sha = escape_html(&f.sha256),
        ));
        script.push_str(&format!(
            "\n$file = Join-Path $dir '{name}'\n\
             Start-BitsTransfer -Source '{url}' -Destination $file\n\
             Get-ChecksumValid -File $file -Checksum '{sha}' -ChecksumType sha256",
            name = f.name,
            sha = f.sha256,
        ));
    }
    list.push_str("</ul>");
    format!(
        "<h2>Files</h2>{list}\
         <div class=\"script\"><h3>In chocolateyInstall.ps1</h3><div class=\"snip\">\
         <button type=\"button\" class=\"copy\" aria-label=\"Copy the download script\" hidden>Copy</button>\
         <pre><code>{code}</code></pre></div></div>",
        code = escape_html(&script),
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
             <button type=\"button\" class=\"copy\" aria-label=\"Copy the {label} command\" \
             hidden>Copy</button>\
             <pre><code>{cmd}</code></pre></div></div>",
            cmd = command_html(&cmd),
        ));
    }
    out
}

/// A command for a copyable snippet, as HTML: every token escaped exactly
/// once, and each flag kept on one line with the value after it.
///
/// Snippets wrap (`pre-wrap`) to fit the sidebar, and a browser may break a
/// line after any hyphen, so `--version` came out as `--` / `version`, and
/// `2.0.0-beta` split in two. A `.nw` span holds a flag and its value together.
/// A URL stays breakable, being the one token too long for the sidebar. The
/// text is unchanged, tokens are rejoined with the spaces they were split on,
/// and spans add no whitespace, so the copy button (`innerText`) still puts
/// exactly the command on the clipboard.
fn command_html(cmd: &str) -> String {
    let tokens: Vec<&str> = cmd.split(' ').collect();
    let mut parts = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        if token.starts_with('-') {
            let mut kept = token.to_string();
            if let Some(value) = tokens
                .get(i + 1)
                .filter(|v| !v.is_empty() && !v.starts_with('-') && !v.contains("://"))
            {
                kept.push(' ');
                kept.push_str(value);
                i += 1;
            }
            parts.push(format!("<span class=\"nw\">{}</span>", escape_html(&kept)));
        } else {
            parts.push(escape_html(token));
        }
        i += 1;
    }
    parts.join(" ")
}

/// The label's cells: what this one version is. The download count is this
/// version's own, like everything else on the label; the gallery shows the
/// total across versions.
fn render_fields(p: &Package) -> String {
    let field = |term: &str, value: &str| format!("<div><dt>{term}</dt><dd>{value}</dd></div>");
    let mut out = field(
        "Version",
        &format!(
            "{}{}",
            escape_html(&p.normalized_version()),
            status_badges(p)
        ),
    );
    out.push_str(&field(
        "Published",
        &escape_html(&p.published.format("%Y-%m-%d").to_string()),
    ));
    out.push_str(&field("Size", &escape_html(&human_size(p.package_size))));
    out.push_str(&field("Downloads", &group_digits(p.downloads as i64)));
    if let Some(lic) = p.license_expression.as_deref().or(p.license_url.as_deref()) {
        out.push_str(&field("License", &escape_html(lic)));
    }
    let types = package_type_names(&p.package_types);
    if !types.is_empty() {
        out.push_str(&field("Type", &escape_html(&types)));
    }
    if !p.authors.is_empty() {
        out.push_str(&format!(
            "<div class=\"wide\"><dt>Authors</dt><dd>{}</dd></div>",
            escape_html(&p.authors.join(", "))
        ));
    }
    out
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
        format!("<p class=\"links\">{}</p>", links.join(" "))
    }
}

fn render_dependencies(urls: &UrlBuilder, p: &Package) -> String {
    if p.dependencies.is_empty() {
        return String::new();
    }
    // Each target framework is a heading of its own under Dependencies, so a
    // screen reader can jump between them as a sighted reader scans for one.
    let mut out = String::from("<h2>Dependencies</h2>");
    for group in &p.dependencies {
        let tfm = group
            .target_framework
            .as_deref()
            .unwrap_or("All frameworks");
        out.push_str(&format!("<h3 class=\"tfm\">{}</h3>", escape_html(tfm)));
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
                "<h2>Readme</h2><div class=\"card readme\">{}</div>{notice}",
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

/// The most tags a package shows on a page.
const MAX_RENDERED_TAGS: usize = 32;

/// A package's tags, each a link to the packages that share it.
fn render_tags(urls: &UrlBuilder, tags: &[String]) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut out = String::from("<div class=\"tags\">");
    // Tags are capped when a package is pushed; this bounds what rows stored
    // before that cap can put on a page.
    for t in tags.iter().take(MAX_RENDERED_TAGS) {
        out.push_str(&format!(
            "<a class=\"tag\" href=\"{}\">{}</a>",
            tag_href(urls, t),
            escape_html(t)
        ));
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

    /// What a browser's `innerText` gives for `html`: the text with the tags
    /// dropped and the five escapes undone. It is what the copy button puts
    /// on the clipboard.
    fn text_of(html: &str) -> String {
        let mut text = String::new();
        let mut in_tag = false;
        for c in html.chars() {
            match c {
                '<' => in_tag = true,
                '>' if in_tag => in_tag = false,
                _ if !in_tag => text.push(c),
                _ => {}
            }
        }
        text.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
            .replace("&amp;", "&")
    }

    #[test]
    fn install_commands_keep_flags_whole_and_copy_unchanged() {
        // `pre-wrap` let a browser break after any hyphen: `--version` split
        // into `--` and `version` at the end of a line.
        let urls = UrlBuilder::new("https://host.test/a&b");
        let mut p = sample();
        p.version = crate::version::NuGetVersion::parse("2.0.0-beta").unwrap();
        let html = render_install(&urls, &p, "choco");
        assert!(
            html.contains("<span class=\"nw\">--version 2.0.0-beta</span>"),
            "{html}"
        );
        assert!(
            html.contains("<span class=\"nw\">-Version 2.0.0-beta</span>"),
            "{html}"
        );
        // The URL is left free to wrap, and is escaped once.
        assert!(
            html.contains(
                "<span class=\"nw\">--source</span> https://host.test/a&amp;b/v3/index.json"
            ),
            "{html}"
        );
        // Each snippet's text is exactly the command, so the clipboard is too.
        let idx = "https://host.test/a&b/v3/index.json";
        for expected in [
            format!("choco install Contoso.Utils --version 2.0.0-beta --source {idx}"),
            format!("dotnet add package Contoso.Utils --version 2.0.0-beta --source {idx}"),
            format!("nuget install Contoso.Utils -Version 2.0.0-beta -Source {idx}"),
        ] {
            let found = html
                .split("<code>")
                .skip(1)
                .map(|s| text_of(s.split("</code>").next().unwrap()))
                .any(|t| t == expected);
            assert!(found, "no snippet reads {expected:?}: {html}");
        }
        // The helper keeps a lone flag, and the value after it, intact.
        assert_eq!(text_of(&command_html("a -n b --x")), "a -n b --x");
        assert_eq!(text_of(&command_html("a  b")), "a  b");
    }

    #[test]
    fn copy_buttons_show_only_where_they_can_copy() {
        // `navigator.clipboard` exists only in a secure context, so on a feed
        // served over plain HTTP (and without JavaScript) a visible Copy button
        // did nothing. Buttons start hidden and the script shows them when the
        // API is there.
        let urls = UrlBuilder::new("http://feed.example");
        let p = sample();
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
        for label in ["Chocolatey", "dotnet CLI", "nuget.exe"] {
            let button = format!(
                "<button type=\"button\" class=\"copy\" aria-label=\"Copy the {label} command\" \
                 hidden>Copy</button>"
            );
            assert!(html.contains(&button), "{label}: {html}");
        }
        assert!(COPY_SCRIPT_BODY.starts_with(
            "if(navigator.clipboard)document.querySelectorAll('.copy')\
             .forEach(function(b){b.hidden=false});"
        ));
        // One status region announces a copy; the button's "Copied" is hidden
        // behind its aria-label.
        assert_eq!(
            html.matches("<div id=\"copied\" class=\"vh\" role=\"status\"></div>")
                .count(),
            1,
            "{html}"
        );
        assert!(COPY_SCRIPT_BODY.contains("getElementById('copied')"));

        let first_run = gallery_page(&urls, &page_of(&[]), &view("", 0, 20));
        assert!(
            first_run.contains("aria-label=\"Copy the command to push a package\" hidden>"),
            "{first_run}"
        );
    }

    #[test]
    fn install_snippet_orders_primary_first() {
        let urls = UrlBuilder::new("https://nuget.example.com");
        let p = sample();
        let choco_first = render_install(&urls, &p, "choco");
        assert!(choco_first.find("Chocolatey").unwrap() < choco_first.find("dotnet CLI").unwrap());
        assert!(text_of(&choco_first).contains("choco install Contoso.Utils --version 1.0.0"));
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
        let html = detail_page(
            &urls,
            std::slice::from_ref(&p),
            &p,
            &Detail {
                readme: None,
                primary_client: "choco",
                has_symbols: true,
                admin: false,
                files: &[],
            },
        );
        assert!(html.contains("badge pre"));
        assert!(html.contains("badge un"));
        assert!(html.contains("Debug symbols are available"));
    }

    #[test]
    fn the_detail_page_keeps_the_versions_within_reach() {
        // The sticky install card (over 500 px tall) covered Info and Versions
        // while scrolling, and on a phone Versions came after the whole readme.
        // Info has since moved into the label at the top of the page.
        let urls = UrlBuilder::new("https://host");
        let p = sample();
        let html = detail_page(
            &urls,
            std::slice::from_ref(&p),
            &p,
            &Detail {
                readme: Some("A long readme."),
                primary_client: "choco",
                has_symbols: false,
                admin: false,
                files: &[],
            },
        );
        assert!(!STYLE.contains("sticky"));
        let at = |needle: &str| {
            html.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {html}"))
        };
        let (label, install) = (at("<section class=\"label\""), at("class=\"card install\""));
        let (versions, readme) = (at(">Versions<"), at("<div class=\"readme-area\">"));
        assert!(
            label < install && install < versions && versions < readme,
            "{html}"
        );
        assert!(!html.contains(">Info<"), "{html}");
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
                total_downloads: 0,
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
        assert!(STYLE.contains(
            "input[type=search]{flex:1;min-width:0;padding:9px 14px;border-radius:6px;border:2px solid var(--ctl)"
        ));
        assert!(STYLE.contains(
            "select,.pager-go input{min-height:44px;padding:0 10px;border:2px solid var(--ctl)"
        ));
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
        // The one font is this server's own, and the policy allows only that.
        assert!(FONT_URL.starts_with('/'), "{FONT_URL}");
        assert!(STYLE.contains(&format!("src:url({FONT_URL})")));
        assert!(html.contains(&format!("<link rel=\"preload\" href=\"{FONT_URL}\"")));
        assert!(CSP.contains("font-src 'self';"), "{}", *CSP);
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
    fn the_chrome_names_the_version_and_links_the_admin_area_when_there_is_one() {
        let urls = UrlBuilder::new("https://host");
        let p = sample();
        let plain = detail_page(
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
        assert!(
            plain.contains(&format!("Served by YANuget {}", env!("CARGO_PKG_VERSION"))),
            "{plain}"
        );
        assert!(!plain.contains("/admin"), "{plain}");

        let managed = detail_page(
            &urls,
            std::slice::from_ref(&p),
            &p,
            &Detail {
                readme: None,
                primary_client: "choco",
                has_symbols: false,
                admin: true,
                files: &[],
            },
        );
        assert!(
            managed.contains("<a href=\"/admin/packages/contoso.utils\">Manage versions</a>"),
            "{managed}"
        );
        // The package file is linked for everyone, from the client endpoint.
        for html in [&plain, &managed] {
            assert!(
                html.contains(
                    "<a href=\"https://host/v3/package/contoso.utils/1.0.0/contoso.utils.1.0.0.nupkg\">\
                     Download .nupkg</a>"
                ),
                "{html}"
            );
        }
        assert!(
            managed.contains("<a href=\"/admin\">Admin</a>"),
            "{managed}"
        );

        let settings = settings_page(&urls, &Config::default(), &feed_ctx(None, None));
        assert!(
            settings.contains(&format!(
                "<b>YANuget version</b>{}",
                env!("CARGO_PKG_VERSION")
            )),
            "{settings}"
        );
        // With no admin key, the settings say how to get one.
        assert!(settings.contains("admin_api_key"), "{settings}");
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
    fn the_admin_page_acts_on_the_selected_versions() {
        let urls = UrlBuilder::new("https://host");
        let mut beta = sample();
        beta.version = crate::version::NuGetVersion::parse("2.0.0-beta").unwrap();
        let versions = vec![
            feed_version(sample(), false, false),
            feed_version(beta, false, false),
        ];
        let html = admin_package_page(
            &urls,
            "Contoso.Utils",
            &versions,
            &AdminPackageExtras::default(),
            "tok",
        );
        // One box per version, belonging to the form below the table.
        for v in ["1.0.0", "2.0.0-beta"] {
            assert!(
                html.contains(&format!(
                    "<input type=\"checkbox\" name=\"v\" value=\"{v}\" form=\"bulk\""
                )),
                "{v}: {html}"
            );
        }
        assert!(
            html.contains(
                "<form id=\"bulk\" class=\"bulk\" method=\"post\" \
                 action=\"/admin/packages/contoso.utils\""
            ),
            "{html}"
        );
        assert!(html.contains("<input type=\"hidden\" name=\"_csrf\" value=\"tok\">"));
        // Enter submits with the first button, so that one must be harmless;
        // Delete asks first.
        let first = html.split("id=\"bulk\"").nth(1).unwrap();
        let first = first.split("<button").nth(1).unwrap();
        assert!(first.contains("value=\"enable\""), "{first}");
        assert!(html.contains("value=\"delete\" class=\"danger\" data-confirm="));
        // No other feed to hand versions to: no copy or move.
        assert!(!html.contains("value=\"move\""), "{html}");
        // Each servable version links to its package file.
        assert!(
            html.contains(
                "<a class=\"btn\" href=\"https://host/v3/package/contoso.utils/2.0.0-beta/\
                 contoso.utils.2.0.0-beta.nupkg\" aria-label=\"Download 2.0.0-beta\">Download</a>"
            ),
            "{html}"
        );

        let targets = ["stable".to_string()];
        let html = admin_package_page(
            &urls,
            "Contoso.Utils",
            &versions,
            &AdminPackageExtras {
                transfer_targets: &targets,
                ..Default::default()
            },
            "tok",
        );
        assert!(
            html.contains("<option value=\"stable\">stable</option>"),
            "{html}"
        );
        assert!(html.contains("name=\"op\" value=\"copy\""), "{html}");
        assert!(html.contains("name=\"op\" value=\"move\""), "{html}");
        // The confirmation is read off the button that submitted.
        assert!(COPY_SCRIPT_BODY.contains("e.submitter"));
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
            cleanup: Default::default(),
        }
    }

    fn feed_version(p: Package, pending: bool, flagged: bool) -> crate::database::FeedVersion {
        crate::database::FeedVersion {
            package: p,
            pending,
            flagged,
            flag_reason: flagged.then(|| "license MIT is blocked".to_string()),
            pinned: false,
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
        let pkg = admin_package_page(
            &urls,
            "Contoso.Utils",
            &versions,
            &AdminPackageExtras::default(),
            "tok",
        );
        assert!(pkg.contains("/disable"));
        assert!(pkg.contains("/enable"));
        assert!(pkg.contains("/delete"));
        assert!(pkg.contains("badge un")); // the disabled one
        assert!(pkg.contains("badge ok")); // the active one
    }

    #[test]
    fn the_admin_page_pins_and_says_what_the_next_cleanup_deletes() {
        let urls = UrlBuilder::new("https://host");
        let mut old = sample();
        old.version = crate::version::NuGetVersion::parse("0.9.0").unwrap();
        let mut pinned = feed_version(sample(), false, false);
        pinned.pinned = true;
        let versions = vec![pinned, feed_version(old.clone(), false, false)];
        let plan = [crate::retention::Pruned {
            version: old.version.clone(),
            reason: crate::retention::PruneReason {
                beyond_newest: None,
                older_than_days: Some(30),
            },
        }];
        let html = admin_package_page(
            &urls,
            "Contoso.Utils",
            &versions,
            &AdminPackageExtras {
                retention_plan: &plan,
                ..Default::default()
            },
            "tok",
        );
        assert!(
            html.contains("<span class=\"badge pin\">pinned</span>"),
            "{html}"
        );
        assert!(
            html.contains("/admin/packages/contoso.utils/1.0.0/unpin"),
            "{html}"
        );
        assert!(
            html.contains("/admin/packages/contoso.utils/0.9.0/pin"),
            "{html}"
        );
        assert!(
            html.contains("The next cleanup deletes this: older than 30 days."),
            "{html}"
        );
        // Deleting a pinned version says what a pin does and does not do.
        assert!(
            html.contains("is pinned, which only keeps it from retention"),
            "{html}"
        );
        assert!(html.contains("name=\"op\" value=\"pin\""), "{html}");
    }

    #[test]
    fn attached_files_are_listed_with_a_script_and_can_be_deleted() {
        let urls = UrlBuilder::new("https://host");
        let p = sample();
        let file = PackageFile {
            lower_id: "contoso.utils".into(),
            normalized_version: "1.0.0".into(),
            name: "base.wim".into(),
            sha256: "ab".repeat(32),
            size: 4 * 1024 * 1024 * 1024,
            uploaded: chrono::Utc::now(),
            downloads: 3,
        };
        let files = [file];
        let page = detail_page(
            &urls,
            std::slice::from_ref(&p),
            &p,
            &Detail {
                primary_client: "choco",
                files: &files,
                ..Default::default()
            },
        );
        let url = "https://host/files/contoso.utils/1.0.0/base.wim";
        assert!(
            page.contains(&format!("<a class=\"id\" href=\"{url}\">base.wim</a>")),
            "{page}"
        );
        assert!(page.contains("4.0 GB"), "{page}");
        // The script is escaped once, so the clipboard gets exactly this.
        let script = page
            .split("<pre><code>")
            .find(|s| s.contains("Start-BitsTransfer"))
            .and_then(|s| s.split("</code>").next())
            .map(text_of)
            .unwrap_or_else(|| panic!("no script: {page}"));
        assert!(script.contains(&format!(
            "Start-BitsTransfer -Source '{url}' -Destination $file"
        )));
        assert!(script.contains(&format!(
            "-Checksum '{}' -ChecksumType sha256",
            "ab".repeat(32)
        )));
        assert!(page.contains("aria-label=\"Copy the download script\""));

        let admin = admin_package_page(
            &urls,
            "Contoso.Utils",
            &[feed_version(sample(), false, false)],
            &AdminPackageExtras {
                files_enabled: true,
                files: &files,
                ..Default::default()
            },
            "tok",
        );
        assert!(
            admin.contains("action=\"/admin/packages/contoso.utils/1.0.0/files/base.wim/delete\""),
            "{admin}"
        );
        assert!(
            admin.contains("Install scripts that fetch it will fail."),
            "{admin}"
        );
        // With the feature off, the admin page does not mention files.
        let off = admin_package_page(
            &urls,
            "Contoso.Utils",
            &[feed_version(sample(), false, false)],
            &AdminPackageExtras::default(),
            "tok",
        );
        assert!(!off.contains("<h2>Files</h2>"), "{off}");
    }

    #[test]
    fn the_retention_page_shows_the_plan_and_deletes_only_when_enabled() {
        use crate::retention::{Planned, Preview, PruneReason};
        let urls = UrlBuilder::new("https://host");
        let plan = Preview {
            planned: vec![Planned {
                id: "Old.Pkg".into(),
                version: crate::version::NuGetVersion::parse("1.0.0").unwrap(),
                published: chrono::Utc::now(),
                reason: PruneReason {
                    beyond_newest: Some((2, false)),
                    older_than_days: None,
                },
                frees: 2048,
            }],
            pinned: vec![(
                "Lts.Pkg".into(),
                crate::version::NuGetVersion::parse("3.1.0").unwrap(),
            )],
        };
        let mut rules = crate::config::RetentionConfig {
            keep_latest_stable: Some(2),
            ..Default::default()
        };
        let page = |rules: &crate::config::RetentionConfig, notice| {
            admin_retention_page(
                &urls,
                &RetentionView {
                    rules,
                    last: None,
                    running: false,
                    preview: Some(&plan),
                    csrf_token: "tok",
                    notice,
                },
            )
        };
        let off = page(&rules, RetentionNotice::None);
        assert!(off.contains("beyond the newest 2 stable versions"), "{off}");
        assert!(
            off.contains("1 version would be deleted, freeing 2.0 KB."),
            "{off}"
        );
        assert!(off.contains(">Lts.Pkg</a>"), "{off}");
        // Off: the plan is shown, but there is nothing to press.
        assert!(!off.contains("/admin/retention/run"), "{off}");

        rules.enabled = true;
        let on = page(&rules, RetentionNotice::None);
        assert!(on.contains("action=\"/admin/retention/run\""), "{on}");
        assert!(
            on.contains(&format!("name=\"plan\" value=\"{}\"", plan.fingerprint())),
            "{on}"
        );
        assert!(on.contains("<input type=\"hidden\" name=\"_csrf\" value=\"tok\">"));

        let changed = page(&rules, RetentionNotice::Changed);
        assert!(changed.contains("nothing was deleted"), "{changed}");
        let done = page(
            &rules,
            RetentionNotice::Done(crate::retention::Outcome {
                deleted: 3,
                freed: 1024,
                errors: 0,
            }),
        );
        assert!(
            done.contains("Deleted 3 versions, freeing 1.0 KB."),
            "{done}"
        );
    }

    #[test]
    fn admin_page_shows_pending_and_promote() {
        let urls = UrlBuilder::new("https://host");
        let versions = vec![feed_version(sample(), true, true)];
        let pkg = admin_package_page(
            &urls,
            "Contoso.Utils",
            &versions,
            &AdminPackageExtras {
                promote_target: Some("stable"),
                ..Default::default()
            },
            "tok",
        );
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
