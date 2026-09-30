//! The package page: a version's metadata, install commands, readme,
//! dependencies, attached files and the version list.

use super::escape::{enc_path, escape_html, safe_href};
use super::format::{group_digits, human_size, plural};
use super::gallery::tag_href;
use super::layout::{layout, Nav};
use crate::database::PackageFile;
use crate::models::Package;
use crate::models::PackageType;
use crate::nuget::UrlBuilder;

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
pub(super) fn command_html(cmd: &str) -> String {
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
pub(super) fn render_tags(urls: &UrlBuilder, tags: &[String]) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::ui::fixtures::{sample, text_of};
    use crate::web::ui::layout::STYLE;

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
}
