//! Parsing of the `.nuspec` manifest embedded in every `.nupkg`.
//!
//! The nuspec is small XML, so it is parsed in full with an event-based reader.
//! We match on *local* element names so the parser is agnostic to the (several)
//! XML namespaces NuGet has used over the years.

use quick_xml::events::{BytesRef, Event};
use quick_xml::{Reader, XmlVersion};

use crate::error::Error;
use crate::models::{Dependency, DependencyGroup, PackageType};

/// The parsed contents of a `.nuspec` `<metadata>` element.
#[derive(Debug, Clone, Default)]
pub struct Nuspec {
    pub id: String,
    pub version: String,
    pub title: Option<String>,
    pub authors: Option<String>,
    pub description: Option<String>,
    pub summary: Option<String>,
    pub release_notes: Option<String>,
    pub language: Option<String>,
    pub tags: Option<String>,
    pub icon_url: Option<String>,
    pub icon: Option<String>,
    pub readme: Option<String>,
    pub license_url: Option<String>,
    pub license_expression: Option<String>,
    pub license_file: Option<String>,
    pub project_url: Option<String>,
    pub repository_url: Option<String>,
    pub repository_type: Option<String>,
    pub min_client_version: Option<String>,
    pub require_license_acceptance: bool,
    pub development_dependency: bool,
    pub package_types: Vec<PackageType>,
    pub dependency_groups: Vec<DependencyGroup>,
}

impl Nuspec {
    /// Split the `tags` field into individual, non-empty tags.
    pub fn tag_list(&self) -> Vec<String> {
        self.tags
            .as_deref()
            .map(|t| {
                t.split_whitespace()
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Split `authors` (comma-separated) into individual authors.
    pub fn author_list(&self) -> Vec<String> {
        self.authors
            .as_deref()
            .map(|a| {
                a.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Bounds on a single manifest. A real nuspec is a few KiB with a handful of
/// dependency groups; these are far above anything legitimate and exist only so
/// a hostile manifest cannot turn its (already capped) 16 MiB of XML into an
/// unbounded pile of allocations, database rows and rendered HTML.
const MAX_ELEMENT_DEPTH: usize = 64;
const MAX_DEPENDENCY_GROUPS: usize = 512;
const MAX_DEPENDENCIES: usize = 10_000;
const MAX_PACKAGE_TYPES: usize = 64;

/// Parse a `.nuspec` document. Returns [`Error::InvalidPackage`] when the XML is
/// malformed, exceeds the structural limits above, or is missing the mandatory
/// `id`/`version` fields.
pub fn parse_nuspec(xml: &str) -> Result<Nuspec, Error> {
    let mut reader = Reader::from_str(xml);
    // Text is *not* trimmed per event, because an element's text can arrive as
    // several events (see `text` below) and trimming each one would eat the
    // spaces between them. The accumulated value is trimmed once, at the end.
    reader.config_mut().trim_text(false);

    let mut nuspec = Nuspec::default();
    // Lower-cased local-name stack of currently open elements.
    let mut path: Vec<String> = Vec::new();
    // Index into `dependency_groups` for the currently open `<group>`, if any.
    let mut current_group: Option<usize> = None;
    // Text accumulated for the element currently open.
    //
    // quick-xml reports the content of one element as *one event per run
    // between entity references*: `Alice &amp; Bob` arrives as "Alice ", "&",
    // " Bob". Assigning each event in turn would keep only the last, silently
    // truncating every field that contains an entity — and `&` is ordinary in a
    // description or an author list. So the runs are joined and assigned once,
    // when the element closes.
    let mut text = String::new();

    loop {
        match reader.read_event() {
            // Self-closing elements (`<dependency/>`, `<group/>`, ...) have no
            // matching `End`, so they must not touch the path stack or leave a
            // group "open".
            Ok(Event::Empty(e)) => {
                let name = local_name(e.name().as_ref());
                handle_attr_element(&name, &e, &mut nuspec, current_group);
                check_limits(&nuspec)?;
            }
            Ok(Event::Start(e)) => {
                let name = local_name(e.name().as_ref());

                // `<license type="...">` carries its value as following text, so
                // remember which flavour we are inside via a synthetic path entry.
                if name == "license" {
                    let kind = attr(&e, "type").unwrap_or_default();
                    if kind.eq_ignore_ascii_case("file") {
                        path.push("license:file".into());
                    } else {
                        path.push("license:expression".into());
                    }
                    continue;
                }

                handle_attr_element(&name, &e, &mut nuspec, current_group);
                if name == "group" {
                    // `handle_attr_element` always pushes a group for this name,
                    // so the list is non-empty here.
                    current_group = nuspec.dependency_groups.len().checked_sub(1);
                }
                if path.len() >= MAX_ELEMENT_DEPTH {
                    return Err(Error::InvalidPackage(format!(
                        "nuspec nests deeper than {MAX_ELEMENT_DEPTH} elements"
                    )));
                }
                path.push(name);
                // Any text seen before this child belongs to the parent, which
                // in a nuspec is never a scalar field — drop it rather than let
                // it bleed into the child's value.
                text.clear();
                check_limits(&nuspec)?;
            }
            Ok(Event::Text(e)) => {
                // `xml10_content` decodes the bytes and normalizes EOLs. It does
                // not resolve entities — quick-xml 0.41 reports those separately,
                // as `GeneralRef` events, which is why an element's content
                // arrives as several events and has to be reassembled.
                match e.xml10_content() {
                    Ok(decoded) => text.push_str(&decoded),
                    Err(_) => continue,
                }
            }
            // `&amp;`, `&lt;`, `&#233;` … — the entity between two text runs.
            Ok(Event::GeneralRef(e)) => {
                if let Some(resolved) = resolve_reference(&e) {
                    text.push(resolved);
                }
            }
            // A `<description><![CDATA[...]]></description>` is how a manifest
            // carries markup without escaping it. Ignoring the event dropped the
            // field entirely.
            Ok(Event::CData(e)) => match e.decode() {
                Ok(decoded) => text.push_str(&decoded),
                Err(_) => continue,
            },
            Ok(Event::End(_)) => {
                let value = std::mem::take(&mut text);
                let value = value.trim();
                if !value.is_empty() {
                    assign_text(&mut nuspec, &path, value.to_string());
                }
                if let Some(top) = path.pop() {
                    if top == "group" {
                        current_group = None;
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(Error::InvalidPackage(format!("malformed nuspec: {e}")));
            }
            _ => {}
        }
    }

    if nuspec.id.trim().is_empty() {
        return Err(Error::InvalidPackage("nuspec is missing <id>".into()));
    }
    if nuspec.version.trim().is_empty() {
        return Err(Error::InvalidPackage("nuspec is missing <version>".into()));
    }
    Ok(nuspec)
}

/// Resolve one entity reference to its character.
///
/// A nuspec is a standalone document with no DTD, so the only references that
/// can legitimately appear are numeric character references and the five XML
/// predefined entities. Anything else is undefined and is dropped rather than
/// reproduced literally, which would silently turn `&foo;` into text that looks
/// like markup.
fn resolve_reference(e: &BytesRef) -> Option<char> {
    if let Ok(Some(ch)) = e.resolve_char_ref() {
        return Some(ch);
    }
    match e.decode().ok()?.as_ref() {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => None,
    }
}

/// Reject a manifest that has grown past the structural limits.
fn check_limits(n: &Nuspec) -> Result<(), Error> {
    if n.dependency_groups.len() > MAX_DEPENDENCY_GROUPS {
        return Err(Error::InvalidPackage(format!(
            "nuspec declares more than {MAX_DEPENDENCY_GROUPS} dependency groups"
        )));
    }
    if n.package_types.len() > MAX_PACKAGE_TYPES {
        return Err(Error::InvalidPackage(format!(
            "nuspec declares more than {MAX_PACKAGE_TYPES} package types"
        )));
    }
    let deps: usize = n
        .dependency_groups
        .iter()
        .map(|g| g.dependencies.len())
        .sum();
    if deps > MAX_DEPENDENCIES {
        return Err(Error::InvalidPackage(format!(
            "nuspec declares more than {MAX_DEPENDENCIES} dependencies"
        )));
    }
    Ok(())
}

/// Process an element whose data lives entirely in its attributes. Shared by
/// the `Start` and `Empty` event arms. For `<group>` this only *creates* the
/// group; marking it as the currently-open group is the caller's job (it only
/// applies to a non-empty `Start`).
fn handle_attr_element(
    name: &str,
    e: &quick_xml::events::BytesStart,
    nuspec: &mut Nuspec,
    current_group: Option<usize>,
) {
    match name {
        "metadata" => {
            if let Some(v) = attr(e, "minclientversion") {
                nuspec.min_client_version = Some(v);
            }
        }
        "repository" => {
            if let Some(v) = attr(e, "url") {
                nuspec.repository_url = Some(v);
            }
            if let Some(v) = attr(e, "type") {
                nuspec.repository_type = Some(v);
            }
        }
        "group" => {
            nuspec.dependency_groups.push(DependencyGroup {
                target_framework: attr(e, "targetframework"),
                dependencies: Vec::new(),
            });
        }
        "dependency" => {
            let dep = Dependency {
                id: attr(e, "id").unwrap_or_default(),
                version_range: attr(e, "version"),
                include: attr(e, "include"),
                exclude: attr(e, "exclude"),
            };
            if !dep.id.is_empty() {
                let idx = match current_group {
                    Some(i) => i,
                    None => ungrouped_index(nuspec),
                };
                nuspec.dependency_groups[idx].dependencies.push(dep);
            }
        }
        "packagetype" => {
            if let Some(n) = attr(e, "name") {
                nuspec.package_types.push(PackageType {
                    name: n,
                    version: attr(e, "version"),
                });
            }
        }
        _ => {}
    }
}

/// Find or create the "ungrouped" dependency group (no target framework).
fn ungrouped_index(nuspec: &mut Nuspec) -> usize {
    if let Some(i) = nuspec
        .dependency_groups
        .iter()
        .position(|g| g.target_framework.is_none())
    {
        return i;
    }
    nuspec.dependency_groups.push(DependencyGroup::default());
    nuspec.dependency_groups.len() - 1
}

/// Assign a text value to the right field based on the open-element path.
fn assign_text(nuspec: &mut Nuspec, path: &[String], text: String) {
    let Some(top) = path.last() else { return };
    // Only assign metadata-scalar fields when inside <metadata>.
    let in_metadata = path.iter().any(|p| p == "metadata");
    if !in_metadata {
        return;
    }
    match top.as_str() {
        "id" => nuspec.id = text,
        "version" => nuspec.version = text,
        "title" => nuspec.title = Some(text),
        "authors" => nuspec.authors = Some(text),
        "description" => nuspec.description = Some(text),
        "summary" => nuspec.summary = Some(text),
        "releasenotes" => nuspec.release_notes = Some(text),
        "language" => nuspec.language = Some(text),
        "tags" => nuspec.tags = Some(text),
        "iconurl" => nuspec.icon_url = Some(text),
        "icon" => nuspec.icon = Some(text),
        "readme" => nuspec.readme = Some(text),
        "projecturl" => nuspec.project_url = Some(text),
        "licenseurl" => nuspec.license_url = Some(text),
        "requirelicenseacceptance" => {
            nuspec.require_license_acceptance = text.eq_ignore_ascii_case("true")
        }
        "developmentdependency" => {
            nuspec.development_dependency = text.eq_ignore_ascii_case("true")
        }
        "license:expression" => nuspec.license_expression = Some(text),
        "license:file" => nuspec.license_file = Some(text),
        _ => {}
    }
}

/// Extract the local (namespace-stripped) name and lower-case it.
fn local_name(raw: &[u8]) -> String {
    let s = std::str::from_utf8(raw).unwrap_or("");
    let local = s.rsplit(':').next().unwrap_or(s);
    local.to_ascii_lowercase()
}

/// Look up an attribute by its lower-cased local name.
fn attr(e: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    e.attributes().flatten().find_map(|a| {
        let key = local_name(a.key.as_ref());
        if key == name {
            // `normalized_value` replaces the deprecated `unescape_value` and
            // applies XML 1.0 attribute-value normalization plus entity resolution.
            a.normalized_value(XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities_do_not_truncate_a_field() {
        // quick-xml reports one text event per run between entity references.
        // Keeping only the last one silently truncated every field containing an
        // entity — and `&` is ordinary in a description or an author list.
        let xml = r#"<package><metadata>
            <id>P</id><version>1.0.0</version>
            <description>Foo &amp; Bar &lt;T&gt; tail</description>
            <authors>Alice &amp; Bob, Carol</authors>
            <title>A &quot;quoted&quot; title</title>
            <tags>a&amp;b c</tags>
        </metadata></package>"#;
        let n = parse_nuspec(xml).unwrap();
        assert_eq!(n.description.as_deref(), Some("Foo & Bar <T> tail"));
        assert_eq!(n.authors.as_deref(), Some("Alice & Bob, Carol"));
        assert_eq!(n.author_list(), vec!["Alice & Bob", "Carol"]);
        assert_eq!(n.title.as_deref(), Some(r#"A "quoted" title"#));
        assert_eq!(n.tag_list(), vec!["a&b", "c"]);
    }

    #[test]
    fn cdata_content_is_kept() {
        // CDATA is how a manifest carries markup without escaping it; the event
        // used to be ignored, dropping the field.
        let xml = r#"<package><metadata>
            <id>P</id><version>1.0.0</version>
            <description><![CDATA[Raw <b>markup</b> & symbols]]></description>
        </metadata></package>"#;
        let n = parse_nuspec(xml).unwrap();
        assert_eq!(
            n.description.as_deref(),
            Some("Raw <b>markup</b> & symbols")
        );
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_but_interior_is_kept() {
        let xml = "<package><metadata>\n  <id>P</id>\n  <version>1.0.0</version>\n  \
                   <description>\n    one &amp; two   three\n  </description>\n\
                   </metadata></package>";
        let n = parse_nuspec(xml).unwrap();
        assert_eq!(n.description.as_deref(), Some("one & two   three"));
        assert_eq!(n.id, "P");
    }

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata minClientVersion="2.12">
    <id>Contoso.Utils</id>
    <version>1.2.3-beta.1+build.7</version>
    <title>Contoso Utilities</title>
    <authors>Alice, Bob</authors>
    <description>Handy helpers.</description>
    <summary>Helpers</summary>
    <releaseNotes>First beta.</releaseNotes>
    <projectUrl>https://example.com</projectUrl>
    <license type="expression">MIT</license>
    <icon>images/icon.png</icon>
    <readme>docs/README.md</readme>
    <tags>utils helpers dotnet</tags>
    <repository type="git" url="https://github.com/contoso/utils" />
    <packageTypes>
      <packageType name="DotnetTool" version="1.0.0" />
    </packageTypes>
    <dependencies>
      <group targetFramework="net8.0">
        <dependency id="Newtonsoft.Json" version="[13.0.1, )" />
        <dependency id="Serilog" version="3.0.0" include="all" />
      </group>
      <group targetFramework="netstandard2.0" />
    </dependencies>
  </metadata>
</package>"#;

    #[test]
    fn parses_full_manifest() {
        let n = parse_nuspec(SAMPLE).unwrap();
        assert_eq!(n.id, "Contoso.Utils");
        assert_eq!(n.version, "1.2.3-beta.1+build.7");
        assert_eq!(n.title.as_deref(), Some("Contoso Utilities"));
        assert_eq!(n.author_list(), vec!["Alice", "Bob"]);
        assert_eq!(n.description.as_deref(), Some("Handy helpers."));
        assert_eq!(n.min_client_version.as_deref(), Some("2.12"));
        assert_eq!(n.license_expression.as_deref(), Some("MIT"));
        assert_eq!(n.icon.as_deref(), Some("images/icon.png"));
        assert_eq!(n.readme.as_deref(), Some("docs/README.md"));
        assert_eq!(n.tag_list(), vec!["utils", "helpers", "dotnet"]);
        assert_eq!(n.repository_type.as_deref(), Some("git"));
        assert_eq!(
            n.repository_url.as_deref(),
            Some("https://github.com/contoso/utils")
        );
        assert_eq!(n.package_types.len(), 1);
        assert_eq!(n.package_types[0].name, "DotnetTool");

        assert_eq!(n.dependency_groups.len(), 2);
        let net8 = &n.dependency_groups[0];
        assert_eq!(net8.target_framework.as_deref(), Some("net8.0"));
        assert_eq!(net8.dependencies.len(), 2);
        assert_eq!(net8.dependencies[0].id, "Newtonsoft.Json");
        assert_eq!(
            net8.dependencies[0].version_range.as_deref(),
            Some("[13.0.1, )")
        );
        assert_eq!(net8.dependencies[1].include.as_deref(), Some("all"));
        // Empty group is preserved with no dependencies.
        assert_eq!(n.dependency_groups[1].dependencies.len(), 0);
    }

    #[test]
    fn parses_ungrouped_dependencies() {
        let xml = r#"<package><metadata>
            <id>A</id><version>1.0.0</version>
            <dependencies>
              <dependency id="B" version="1.0.0" />
            </dependencies>
        </metadata></package>"#;
        let n = parse_nuspec(xml).unwrap();
        assert_eq!(n.dependency_groups.len(), 1);
        assert!(n.dependency_groups[0].target_framework.is_none());
        assert_eq!(n.dependency_groups[0].dependencies[0].id, "B");
    }

    #[test]
    fn rejects_missing_id_or_version() {
        assert!(
            parse_nuspec("<package><metadata><version>1.0.0</version></metadata></package>")
                .is_err()
        );
        assert!(parse_nuspec("<package><metadata><id>A</id></metadata></package>").is_err());
        assert!(parse_nuspec("not xml at <<<").is_err());
    }
}
