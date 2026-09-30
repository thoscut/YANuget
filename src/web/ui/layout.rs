//! The shared page chrome: the inline stylesheet and script, the CSP that
//! pins them by hash, the header/footer layout every page is wrapped in, and
//! the error page.
//!
//! The inline `<style>` and `<script>` must stay byte-identical to what the
//! CSP hashes; the tests below check it on a rendered page.

use super::escape::escape_html;
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

pub(in crate::web) const FONT_URL: &str = font_url!();

/// The gallery's styling, inlined so a page needs nothing but itself and the
/// one font file this server also serves. It works fully offline.
///
/// The look is a warehouse's: pages sit on a grey floor, the package page is a
/// shipping label (die-cut corners, heavy carbon rules, one cell per field),
/// and the one colour is floor-marking yellow, spent on the action that
/// matters on a page and on the highlighter a hovered link gets. The typeface,
/// Atkinson Hyperlegible Next, was drawn to keep `l`, `1` and `I` (and `0` and
/// `O`) apart, which is most of what reading a package id or a version asks.
pub(super) const STYLE: &str = concat!(
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
pub(super) const COPY_SCRIPT_BODY: &str = "if(navigator.clipboard)\
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
pub(super) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What the shared chrome needs to know about the page it wraps.
#[derive(Clone, Copy, Default)]
pub(super) struct Nav<'a> {
    /// The current header item (`"stats"`, `"settings"`, `"admin"`, or `""`).
    pub(super) active: &'a str,
    /// Whether this feed has an admin area to link to.
    pub(super) admin: bool,
}

/// Wrap a page body in the shared layout (head, header bar, footer).
pub(super) fn layout(urls: &UrlBuilder, title: &str, nav: Nav, body: &str) -> String {
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
pub(super) enum Chrome {
    /// Inside a feed: everything is reachable.
    Feed,
    /// The multi-feed root: only the feed list itself.
    Root,
}

/// `search_hidden` is extra hidden inputs for the header's search form: the
/// gallery uses it to keep a chosen page size and order across a new search.
pub(super) fn layout_with_chrome(
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
pub fn error_page(
    urls: &UrlBuilder,
    status: axum::http::StatusCode,
    admin: bool,
    admin_login: bool,
) -> String {
    use axum::http::StatusCode;
    let (heading, detail) = match status {
        StatusCode::NOT_FOUND => (
            "Not found",
            "This feed does not have that package, version or page. It may never \
             have been published here, or it may have been deleted.",
        ),
        // The admin area asks for the admin key, not the read key: sending an
        // operator hunting for the wrong one is the whole failure here.
        StatusCode::UNAUTHORIZED if admin_login => (
            "Sign-in required",
            "The admin area needs the admin key. Sign in with any user name and \
             the admin key as the password.",
        ),
        StatusCode::UNAUTHORIZED => (
            "Sign-in required",
            "This feed requires credentials to browse. Use the API key configured \
             for reading it.",
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
            "The server cannot reach its database right now. It should recover on \
             its own.",
        ),
        s if s.is_server_error() => (
            "Something went wrong",
            "The server hit an unexpected error. The details are in its log.",
        ),
        _ => ("That did not work", "The request could not be completed."),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::web::ui::fixtures::{feed_ctx, page_of, sample, view};
    use crate::web::ui::gallery::gallery_page;
    use crate::web::ui::package::{detail_page, Detail};
    use crate::web::ui::settings::settings_page;

    #[test]
    fn error_pages_read_as_prose_and_name_the_right_key() {
        use axum::http::StatusCode;
        let urls = UrlBuilder::new("https://host");
        for status in [
            StatusCode::NOT_FOUND,
            StatusCode::UNAUTHORIZED,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::BAD_REQUEST,
        ] {
            let html = error_page(&urls, status, false, false);
            let text = html
                .split("<p>")
                .nth(1)
                .unwrap()
                .split("</p>")
                .next()
                .unwrap();
            assert!(!text.contains("  "), "{status}: {text:?}");
        }
        let admin = error_page(&urls, StatusCode::UNAUTHORIZED, true, true);
        assert!(admin.contains("admin key"), "{admin}");
        assert!(!admin.contains("for reading"), "{admin}");
        let read = error_page(&urls, StatusCode::UNAUTHORIZED, false, false);
        assert!(read.contains("for reading"), "{read}");
    }

    /// The CSP pins the inline `<style>` and `<script>` by SHA-256, so a page
    /// whose inline content drifts from the policy silently loses its styling
    /// and its copy buttons. Extract both from a real rendered page and check
    /// the policy actually covers them.
    #[test]
    fn csp_hashes_cover_the_inline_assets_the_page_emits() {
        let urls = super::UrlBuilder::new("https://host");
        let html = settings_page(
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
}
