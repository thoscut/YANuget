//! Parsing of the `.nuspec` manifest embedded in every `.nupkg`.
//!
//! The nuspec is small XML, so it is parsed in full with an event-based reader.
//!
//! What matters most is reading the *same* manifest the NuGet client reads
//! from the same bytes. The feed indexes an id, a version, dependencies and a
//! license from it; if a crafted document made this parser see one identity and
//! NuGet's `NuspecReader` another, the feed would advertise a package the client
//! does not agree it installed, or evaluate the license policy against a
//! license nobody else sees. So the rules follow `NuspecReader`:
//!
//! * `<metadata>` is a direct child of the root element, matched by local name
//!   whatever its namespace (NuGet does the same, because some legacy packages
//!   put the namespace there rather than on `<package>`).
//! * Fields are *direct* children of `<metadata>`, in the namespace that is the
//!   default at `<metadata>` — which for every real manifest is the one on
//!   `<package>`, one of the `…/packaging/…/nuspec.xsd` URIs, or none. Element
//!   and attribute names are case-sensitive, as in XML and in NuGet.
//! * Dependencies are `metadata/dependencies/group/dependency`, or the legacy
//!   `metadata/dependencies/dependency`; package types are
//!   `metadata/packageTypes/packageType`.
//!
//! Where NuGet would quietly pick one of several readings, the manifest is
//! refused instead of betting on agreeing with it: a duplicate `<id>`,
//! `<version>`, `<license>` or other field (NuGet takes the first), a child
//! element inside a text field (NuGet concatenates its text), a second
//! `<metadata>`, `<dependencies>` mixing groups with ungrouped dependencies
//! (NuGet ignores the ungrouped ones), a `DOCTYPE` or an undefined entity.

use std::collections::HashSet;

use quick_xml::events::{BytesRef, BytesStart, Event};
use quick_xml::name::ResolveResult;
use quick_xml::reader::NsReader;
use quick_xml::XmlVersion;

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
    /// Split the `tags` field into individual tags: whitespace-separated,
    /// de-duplicated case-insensitively (the first spelling wins), each cut to
    /// [`MAX_TAG_CHARS`], and at most [`MAX_TAGS`] of them.
    ///
    /// Nothing else bounds this field but the 1 MiB manifest cap, and a
    /// manifest of `a a a …` is half a million tags — each rendered on every
    /// gallery row, returned in every search result and indexed for the tag
    /// filter. nuget.org's own limits are tighter than these.
    pub fn tag_list(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut tags = Vec::new();
        for tag in self.tags.as_deref().unwrap_or("").split_whitespace() {
            if tags.len() == MAX_TAGS {
                break;
            }
            let tag: String = tag.chars().take(MAX_TAG_CHARS).collect();
            if seen.insert(tag.to_lowercase()) {
                tags.push(tag);
            }
        }
        tags
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
/// a hostile manifest cannot turn its XML into an unbounded pile of
/// allocations, database rows and rendered HTML.
///
/// [`MAX_NUSPEC_BYTES`] bounds the document itself. A real manifest is a few
/// KiB, and even a metapackage listing every target framework is far below
/// 1 MiB; 16 MiB used to be allowed, which let an upload that compresses to a
/// few KiB buy seconds of parsing.
pub const MAX_NUSPEC_BYTES: usize = 1024 * 1024;
const MAX_ELEMENT_DEPTH: usize = 64;
/// Attributes on one element. A real element carries at most four or five.
/// quick-xml's duplicate-attribute check scans the attributes already seen, so
/// an element with tens of thousands of them was quadratic to read.
const MAX_ATTRIBUTES: usize = 64;
const MAX_DEPENDENCY_GROUPS: usize = 512;
const MAX_DEPENDENCIES: usize = 10_000;
const MAX_PACKAGE_TYPES: usize = 64;
/// Tags kept per package, and characters kept per tag (see [`Nuspec::tag_list`]).
pub const MAX_TAGS: usize = 64;
pub const MAX_TAG_CHARS: usize = 64;

/// A text field directly under `<metadata>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Id,
    Version,
    Title,
    Authors,
    Description,
    Summary,
    ReleaseNotes,
    Language,
    Tags,
    IconUrl,
    Icon,
    Readme,
    ProjectUrl,
    LicenseUrl,
    RequireLicenseAcceptance,
    DevelopmentDependency,
    /// `<license type="expression">`.
    LicenseExpression,
    /// `<license type="file">`.
    LicenseFile,
    /// `<license>` of a type NuGet does not know: still a field (it may not
    /// contain elements, or appear twice), but its value is not used.
    LicenseOther,
}

/// The text fields directly under `<metadata>`, by exact (case-sensitive)
/// local name. `<license>` is handled separately, because its kind comes from
/// an attribute.
const FIELDS: &[(&[u8], Field)] = &[
    (b"id", Field::Id),
    (b"version", Field::Version),
    (b"title", Field::Title),
    (b"authors", Field::Authors),
    (b"description", Field::Description),
    (b"summary", Field::Summary),
    (b"releaseNotes", Field::ReleaseNotes),
    (b"language", Field::Language),
    (b"tags", Field::Tags),
    (b"iconUrl", Field::IconUrl),
    (b"icon", Field::Icon),
    (b"readme", Field::Readme),
    (b"projectUrl", Field::ProjectUrl),
    (b"licenseUrl", Field::LicenseUrl),
    (b"requireLicenseAcceptance", Field::RequireLicenseAcceptance),
    (b"developmentDependency", Field::DevelopmentDependency),
];

impl Field {
    /// The element name, for messages.
    fn name(self) -> &'static str {
        FIELDS
            .iter()
            .find(|(_, field)| *field == self)
            .and_then(|(name, _)| std::str::from_utf8(name).ok())
            .unwrap_or("license")
    }
}

/// What an open element is to the manifest. One frame per open element, so the
/// stack depth is the element depth.
#[derive(Debug, Clone, Copy)]
enum Frame {
    /// The document element (`<package>`).
    Root,
    Metadata,
    Field(Field),
    Dependencies,
    /// An open `<group>`, by index into `dependency_groups`.
    Group(usize),
    PackageTypes,
    /// Anything else. Its whole subtree is skipped (but still bounded).
    Ignored,
}

/// Running totals checked as elements arrive, so no limit check has to walk
/// what has already been collected.
#[derive(Default)]
struct Counts {
    groups: usize,
    dependencies: usize,
    package_types: usize,
}

impl Counts {
    fn add_group(&mut self) -> Result<(), Error> {
        self.groups += 1;
        if self.groups > MAX_DEPENDENCY_GROUPS {
            return Err(invalid(format!(
                "nuspec declares more than {MAX_DEPENDENCY_GROUPS} dependency groups"
            )));
        }
        Ok(())
    }

    fn add_dependency(&mut self) -> Result<(), Error> {
        self.dependencies += 1;
        if self.dependencies > MAX_DEPENDENCIES {
            return Err(invalid(format!(
                "nuspec declares more than {MAX_DEPENDENCIES} dependencies"
            )));
        }
        Ok(())
    }

    fn add_package_type(&mut self) -> Result<(), Error> {
        self.package_types += 1;
        if self.package_types > MAX_PACKAGE_TYPES {
            return Err(invalid(format!(
                "nuspec declares more than {MAX_PACKAGE_TYPES} package types"
            )));
        }
        Ok(())
    }
}

/// [`parse_nuspec`] on a blocking thread.
///
/// Parsing is bounded, but it is still CPU work proportional to the manifest,
/// and a push is served on an async worker: a few concurrent pushes of a
/// crafted (and highly compressible) manifest would otherwise stall every
/// request sharing those workers. This is what the push and symbol pipelines
/// call.
pub async fn parse_nuspec_blocking(xml: String) -> Result<Nuspec, Error> {
    tokio::task::spawn_blocking(move || parse_nuspec(&xml))
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("nuspec parse task panicked: {e}")))?
}

/// Parse a `.nuspec` document. Returns [`Error::InvalidPackage`] when the XML is
/// malformed, exceeds the structural limits above, is ambiguous in one of the
/// ways the module docs list, or is missing the mandatory `id`/`version`.
pub fn parse_nuspec(xml: &str) -> Result<Nuspec, Error> {
    if xml.len() > MAX_NUSPEC_BYTES {
        return Err(invalid(format!(
            "nuspec is larger than {} KiB",
            MAX_NUSPEC_BYTES / 1024
        )));
    }
    let mut reader = NsReader::from_str(xml);
    // Text is *not* trimmed per event, because an element's text can arrive as
    // several events (see `text` below) and trimming each one would eat the
    // spaces between them. The accumulated value is trimmed once, at the end.
    reader.config_mut().trim_text(false);
    // The reader records an element's namespace declarations before the event
    // reaches `attributes` below, so the same cap has to apply there.
    reader
        .resolver_mut()
        .set_max_declarations_per_element(MAX_ATTRIBUTES);

    let mut nuspec = Nuspec::default();
    let mut stack: Vec<Frame> = Vec::new();
    let mut root_closed = false;
    let mut metadata_seen = false;
    // The default namespace in scope at `<metadata>`: fields must be in it.
    let mut metadata_ns: Option<Vec<u8>> = None;
    // Single-occurrence elements already seen under `<metadata>`.
    let mut seen: HashSet<&'static [u8]> = HashSet::new();
    // `<dependency>` directly under `<dependencies>` (the legacy, ungrouped
    // form), kept apart until we know whether groups were used as well.
    let mut ungrouped: Vec<Dependency> = Vec::new();
    let mut counts = Counts::default();
    // Text accumulated for the open field.
    //
    // quick-xml reports the content of one element as *one event per run
    // between entity references*: `Alice &amp; Bob` arrives as "Alice ", "&",
    // " Bob". Assigning each event in turn would keep only the last, silently
    // truncating every field that contains an entity — and `&` is ordinary in a
    // description or an author list. So the runs are joined and assigned once,
    // when the element closes.
    let mut text = String::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|e| invalid(format!("malformed nuspec: {e}")))?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(event, Event::Empty(_));

                // Checked before anything is pushed, so every element name is
                // bounded. `<license>` used to push its entry and `continue`
                // past a check further down, which left the depth unbounded for
                // that one name: nesting it thousands of times made an
                // ancestor scan quadratic (834ms for a 1 MiB manifest).
                if stack.len() >= MAX_ELEMENT_DEPTH {
                    return Err(invalid(format!(
                        "nuspec nests deeper than {MAX_ELEMENT_DEPTH} elements"
                    )));
                }

                let (ns, local) = reader.resolver().resolve_element(e.name());
                let in_metadata_ns = namespace(ns)? == metadata_ns;
                let local = local.as_ref();

                let frame = match stack.last().copied() {
                    None => {
                        if root_closed {
                            return Err(invalid("nuspec has more than one root element"));
                        }
                        attributes(e, [])?;
                        Frame::Root
                    }
                    // Matched by local name in any namespace, as NuGet does.
                    Some(Frame::Root) if local == b"metadata" => {
                        if std::mem::replace(&mut metadata_seen, true) {
                            return Err(invalid("nuspec has more than one <metadata>"));
                        }
                        metadata_ns = namespace(reader.resolver().resolve_prefix(None, true))?;
                        let [min_client] = attributes(e, ["minClientVersion"])?;
                        nuspec.min_client_version = min_client;
                        Frame::Metadata
                    }
                    Some(Frame::Metadata) if in_metadata_ns => {
                        metadata_child(local, e, &mut seen, &mut nuspec)?
                    }
                    Some(Frame::Field(field)) => {
                        return Err(invalid(format!(
                            "nuspec field <{}> contains an element",
                            field.name()
                        )));
                    }
                    Some(Frame::Dependencies) if in_metadata_ns && local == b"group" => {
                        counts.add_group()?;
                        let [target_framework] = attributes(e, ["targetFramework"])?;
                        nuspec.dependency_groups.push(DependencyGroup {
                            target_framework,
                            dependencies: Vec::new(),
                        });
                        Frame::Group(nuspec.dependency_groups.len() - 1)
                    }
                    Some(Frame::Dependencies) if in_metadata_ns && local == b"dependency" => {
                        counts.add_dependency()?;
                        ungrouped.push(dependency(e)?);
                        Frame::Ignored
                    }
                    Some(Frame::Group(index)) if in_metadata_ns && local == b"dependency" => {
                        counts.add_dependency()?;
                        let dep = dependency(e)?;
                        nuspec.dependency_groups[index].dependencies.push(dep);
                        Frame::Ignored
                    }
                    Some(Frame::PackageTypes) if in_metadata_ns && local == b"packageType" => {
                        counts.add_package_type()?;
                        let [name, version] = attributes(e, ["name", "version"])?;
                        if let Some(name) = name {
                            nuspec.package_types.push(PackageType { name, version });
                        }
                        Frame::Ignored
                    }
                    Some(_) => {
                        // Still validated: a document NuGet cannot load (a
                        // duplicate attribute anywhere) is refused whole.
                        attributes(e, [])?;
                        Frame::Ignored
                    }
                };

                // A self-closing element has no `End`, so it never becomes the
                // open element. An empty field simply has no value.
                if !empty {
                    stack.push(frame);
                    text.clear();
                }
            }
            Event::Text(e) => {
                // `xml10_content` decodes the bytes and normalizes EOLs. It does
                // not resolve entities — quick-xml reports those separately, as
                // `GeneralRef` events, which is why an element's content arrives
                // as several events and has to be reassembled.
                let decoded = e
                    .xml10_content()
                    .map_err(|e| invalid(format!("malformed nuspec: {e}")))?;
                if in_field(&stack) {
                    text.push_str(&decoded);
                }
            }
            // `&amp;`, `&lt;`, `&#233;` … — the entity between two text runs.
            Event::GeneralRef(e) => {
                let resolved = resolve_reference(&e).ok_or_else(|| {
                    invalid(format!(
                        "nuspec uses an undefined entity &{};",
                        String::from_utf8_lossy(&e)
                    ))
                })?;
                if in_field(&stack) {
                    text.push(resolved);
                }
            }
            // A `<description><![CDATA[...]]></description>` is how a manifest
            // carries markup without escaping it. Ignoring the event dropped the
            // field entirely.
            Event::CData(e) => {
                let decoded = e
                    .decode()
                    .map_err(|e| invalid(format!("malformed nuspec: {e}")))?;
                if in_field(&stack) {
                    text.push_str(&decoded);
                }
            }
            // A DTD could define entities NuGet expands and this parser does
            // not; no real manifest has one.
            Event::DocType(_) => return Err(invalid("nuspec must not contain a DOCTYPE")),
            Event::End(_) => {
                if let Some(Frame::Field(field)) = stack.pop() {
                    let value = std::mem::take(&mut text);
                    let value = value.trim();
                    if !value.is_empty() {
                        assign(&mut nuspec, field, value.to_string());
                    }
                }
                if stack.is_empty() {
                    root_closed = true;
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    if !metadata_seen {
        return Err(invalid("nuspec has no <metadata>"));
    }
    if !ungrouped.is_empty() {
        if !nuspec.dependency_groups.is_empty() {
            return Err(invalid(
                "nuspec <dependencies> mixes <group> elements with ungrouped <dependency> \
                 elements; NuGet ignores the ungrouped ones",
            ));
        }
        nuspec.dependency_groups.push(DependencyGroup {
            target_framework: None,
            dependencies: ungrouped,
        });
    }
    if nuspec.id.trim().is_empty() {
        return Err(invalid("nuspec is missing <id>"));
    }
    if nuspec.version.trim().is_empty() {
        return Err(invalid("nuspec is missing <version>"));
    }
    Ok(nuspec)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidPackage(message.into())
}

/// Whether the open element is a text field, whose content is collected.
fn in_field(stack: &[Frame]) -> bool {
    matches!(stack.last(), Some(Frame::Field(_)))
}

/// The namespace an element resolved to, as an owned URI (`None` for no
/// namespace). A prefix that was never declared makes the document malformed.
fn namespace(ns: ResolveResult<'_>) -> Result<Option<Vec<u8>>, Error> {
    match ns {
        ResolveResult::Bound(ns) if !ns.as_ref().is_empty() => Ok(Some(ns.as_ref().to_vec())),
        ResolveResult::Bound(_) | ResolveResult::Unbound => Ok(None),
        ResolveResult::Unknown(prefix) => Err(invalid(format!(
            "nuspec uses an undeclared namespace prefix {}",
            String::from_utf8_lossy(&prefix)
        ))),
    }
}

/// Classify a direct child of `<metadata>` (already known to be in the
/// manifest's namespace), recording what its attributes carry.
fn metadata_child(
    local: &[u8],
    e: &BytesStart,
    seen: &mut HashSet<&'static [u8]>,
    nuspec: &mut Nuspec,
) -> Result<Frame, Error> {
    // Each of these may appear once. NuGet reads the first and ignores the
    // rest; this parser used to keep the last. Rather than hope two readers
    // agree, a manifest that repeats one is refused.
    let mut once = |name: &'static [u8]| -> Result<(), Error> {
        if seen.insert(name) {
            Ok(())
        } else {
            Err(invalid(format!(
                "nuspec declares <{}> more than once",
                String::from_utf8_lossy(name)
            )))
        }
    };

    if let Some(&(name, field)) = FIELDS.iter().find(|(name, _)| *name == local) {
        once(name)?;
        attributes(e, [])?;
        return Ok(Frame::Field(field));
    }
    Ok(match local {
        b"license" => {
            once(b"license")?;
            // NuGet parses the type case-insensitively.
            let [kind] = attributes(e, ["type"])?;
            match kind.as_deref().map(str::to_ascii_lowercase).as_deref() {
                Some("expression") => Frame::Field(Field::LicenseExpression),
                Some("file") => Frame::Field(Field::LicenseFile),
                _ => Frame::Field(Field::LicenseOther),
            }
        }
        b"repository" => {
            once(b"repository")?;
            let [kind, url] = attributes(e, ["type", "url"])?;
            nuspec.repository_type = kind;
            nuspec.repository_url = url;
            Frame::Ignored
        }
        b"dependencies" => {
            once(b"dependencies")?;
            attributes(e, [])?;
            Frame::Dependencies
        }
        b"packageTypes" => {
            once(b"packageTypes")?;
            attributes(e, [])?;
            Frame::PackageTypes
        }
        _ => {
            attributes(e, [])?;
            Frame::Ignored
        }
    })
}

/// Read a `<dependency>`. One without an id is refused: NuGet cannot construct
/// it, so dropping it would advertise a dependency list the client disagrees
/// with.
fn dependency(e: &BytesStart) -> Result<Dependency, Error> {
    let [id, version_range, include, exclude] =
        attributes(e, ["id", "version", "include", "exclude"])?;
    let id = id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| invalid("nuspec declares a <dependency> without an id"))?;
    Ok(Dependency {
        id,
        version_range,
        include,
        exclude,
    })
}

/// Collect the named attributes of `e` in a single pass, validating all of
/// them on the way (a duplicate attribute, or a value with an undefined
/// entity, makes the document one NuGet cannot load).
///
/// Names are matched exactly and without a namespace prefix, which is how
/// `XElement.Attribute("id")` finds them.
fn attributes<const N: usize>(
    e: &BytesStart,
    names: [&str; N],
) -> Result<[Option<String>; N], Error> {
    let mut values: [Option<String>; N] = std::array::from_fn(|_| None);
    for (count, attribute) in e.attributes().enumerate() {
        // Before the duplicate check on this attribute runs, so that check
        // never scans more than the cap.
        if count == MAX_ATTRIBUTES {
            return Err(invalid(format!(
                "nuspec element has more than {MAX_ATTRIBUTES} attributes"
            )));
        }
        let attribute =
            attribute.map_err(|err| invalid(format!("malformed nuspec attribute: {err}")))?;
        let value = attribute
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|err| invalid(format!("malformed nuspec attribute: {err}")))?;
        let key = attribute.key.as_ref();
        if let Some(slot) = names.iter().position(|n| n.as_bytes() == key) {
            values[slot] = Some(value.into_owned());
        }
    }
    Ok(values)
}

/// Resolve one entity reference to its character.
///
/// A nuspec is a standalone document with no DTD, so the only references that
/// can legitimately appear are numeric character references and the five XML
/// predefined entities. Anything else is undefined, which makes the document
/// one NuGet refuses to load.
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

/// Store a field's text.
fn assign(nuspec: &mut Nuspec, field: Field, text: String) {
    match field {
        Field::Id => nuspec.id = text,
        Field::Version => nuspec.version = text,
        Field::Title => nuspec.title = Some(text),
        Field::Authors => nuspec.authors = Some(text),
        Field::Description => nuspec.description = Some(text),
        Field::Summary => nuspec.summary = Some(text),
        Field::ReleaseNotes => nuspec.release_notes = Some(text),
        Field::Language => nuspec.language = Some(text),
        Field::Tags => nuspec.tags = Some(text),
        Field::IconUrl => nuspec.icon_url = Some(text),
        Field::Icon => nuspec.icon = Some(text),
        Field::Readme => nuspec.readme = Some(text),
        Field::ProjectUrl => nuspec.project_url = Some(text),
        Field::LicenseUrl => nuspec.license_url = Some(text),
        Field::RequireLicenseAcceptance => {
            nuspec.require_license_acceptance = text.eq_ignore_ascii_case("true")
        }
        Field::DevelopmentDependency => {
            nuspec.development_dependency = text.eq_ignore_ascii_case("true")
        }
        Field::LicenseExpression => nuspec.license_expression = Some(text),
        Field::LicenseFile => nuspec.license_file = Some(text),
        Field::LicenseOther => {}
    }
}

#[cfg(test)]
mod tests {
    /// Every element name has to be bounded by the depth cap, including the
    /// ones handled by a special branch.
    ///
    /// `<license>` used to push its synthetic path entry and `continue` past
    /// the check. Depth was then unbounded for that name alone, which made an
    /// ancestor scan quadratic — a 1 MiB manifest of nested `<license>` took
    /// 834ms, on a tokio worker rather than a blocking thread, from an upload
    /// that compresses to a few KiB.
    #[test]
    fn no_element_can_nest_past_the_depth_cap() {
        for (name, wrapper, expected) in [
            // A field may not contain elements at all, so this one stops at
            // the second level.
            (
                "license",
                ("<package><metadata>", "</metadata></package>"),
                "contains an element",
            ),
            ("license", ("<package>", "</package>"), "nests deeper"),
            (
                "group",
                ("<package><metadata>", "</metadata></package>"),
                "nests deeper",
            ),
            (
                "group",
                (
                    "<package><metadata><dependencies>",
                    "</dependencies></metadata></package>",
                ),
                "nests deeper",
            ),
        ] {
            let n = MAX_ELEMENT_DEPTH + 50;
            let mut xml = String::from(wrapper.0);
            xml.push_str(&format!("<{name}>").repeat(n));
            xml.push_str(&format!("</{name}>t").repeat(n));
            xml.push_str(wrapper.1);

            let start = std::time::Instant::now();
            let err = parse_nuspec(&xml).expect_err("{name} nested past the cap must be rejected");
            assert!(
                err.to_string().contains(expected),
                "{name}: unexpected error {err}"
            );
            // Bailing at the cap means the cost cannot scale with the input.
            assert!(
                start.elapsed() < std::time::Duration::from_secs(1),
                "{name}: parsing took {:?}, which suggests the cap was not applied",
                start.elapsed()
            );
        }
    }

    /// Nesting up to the cap still parses, so the check is not off by one in
    /// the direction that would reject ordinary manifests.
    #[test]
    fn nesting_within_the_cap_still_parses() {
        let depth = 8;
        let mut xml = String::from("<package><metadata>");
        xml.push_str("<a>".repeat(depth).as_str());
        xml.push_str("</a>".repeat(depth).as_str());
        xml.push_str("<id>A</id><version>1.0.0</version><description>d</description>");
        xml.push_str("<authors>x</authors><license type=\"expression\">MIT</license>");
        xml.push_str("</metadata></package>");
        let parsed = parse_nuspec(&xml).expect("ordinary nesting must parse");
        assert_eq!(parsed.id, "A");
        assert_eq!(parsed.license_expression.as_deref(), Some("MIT"));
    }

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
    fn tags_are_deduplicated_cut_and_capped() {
        let long = "x".repeat(MAX_TAG_CHARS + 10);
        let many: String = (0..MAX_TAGS * 3).map(|i| format!("t{i} ")).collect();
        let n = Nuspec {
            tags: Some(format!("Logging logging  LOGGING {long} {many}")),
            ..Default::default()
        };
        let tags = n.tag_list();
        assert_eq!(tags.len(), MAX_TAGS);
        // The first spelling wins; later case variants are the same tag.
        assert_eq!(tags[0], "Logging");
        assert_eq!(tags[1], "x".repeat(MAX_TAG_CHARS));
        assert!(!tags[2..].iter().any(|t| t.eq_ignore_ascii_case("logging")));
        // Cutting counts characters, never splitting one.
        let n = Nuspec {
            tags: Some("é".repeat(MAX_TAG_CHARS + 1)),
            ..Default::default()
        };
        assert_eq!(n.tag_list()[0].chars().count(), MAX_TAG_CHARS);
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

    fn manifest(metadata: &str) -> String {
        format!(
            r#"<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd"><metadata>{metadata}</metadata></package>"#
        )
    }

    fn rejection(xml: &str) -> String {
        parse_nuspec(xml)
            .expect_err("manifest must be refused")
            .to_string()
    }

    /// NuGet's `NuspecReader` takes the *first* `<id>`; this parser used to
    /// keep the last. Either way two readers can disagree about which package
    /// the same bytes are, so a repeated field is refused.
    #[test]
    fn a_repeated_field_is_refused() {
        for dup in [
            "<id>A</id><version>1.0.0</version><id>B</id>",
            "<id>A</id><version>1.0.0</version><version>2.0.0</version>",
            r#"<id>A</id><version>1.0.0</version><license type="expression">MIT</license>
               <license type="expression">GPL-3.0-only</license>"#,
            "<id>A</id><version>1.0.0</version><licenseUrl>x</licenseUrl><licenseUrl>y</licenseUrl>",
            "<id>A</id><version>1.0.0</version><dependencies/><dependencies/>",
            r#"<id>A</id><version>1.0.0</version><repository url="a"/><repository url="b"/>"#,
        ] {
            assert!(
                rejection(&manifest(dup)).contains("more than once"),
                "{dup}"
            );
        }
        assert!(rejection(
            "<package><metadata><id>A</id><version>1</version></metadata><metadata/></package>"
        )
        .contains("more than one <metadata>"));
    }

    /// `<id>A<x/>B</id>` is "AB" to NuGet (it concatenates the text of the
    /// whole subtree) and used to be "B" here.
    #[test]
    fn an_element_inside_a_field_is_refused() {
        let err = rejection(&manifest(
            "<id>Real<x>Ignored</x>Id</id><version>1.0.0</version>",
        ));
        assert!(err.contains("<id> contains an element"), "{err}");
        let err = rejection(&manifest("<id>A</id><version>1.0.0<b/></version>"));
        assert!(err.contains("contains an element"), "{err}");
    }

    /// Only direct children of `<metadata>` are fields. Any descendant used to
    /// count, so an `<id>` buried in an unrelated element could replace the
    /// real one.
    #[test]
    fn only_direct_children_of_metadata_are_fields() {
        let n = parse_nuspec(&manifest(
            r#"<id>Real</id><version>1.0.0</version>
               <owners><id>Fake</id><version>9.9.9</version></owners>
               <frameworkAssemblies><dependency id="Nope" /></frameworkAssemblies>
               <dependencies><group targetFramework="net8.0">
                 <dependency id="Yes" /><x><dependency id="Hidden" /></x>
               </group></dependencies>"#,
        ))
        .unwrap();
        assert_eq!(n.id, "Real");
        assert_eq!(n.version, "1.0.0");
        let deps: Vec<&str> = n
            .dependency_groups
            .iter()
            .flat_map(|g| g.dependencies.iter().map(|d| d.id.as_str()))
            .collect();
        assert_eq!(deps, vec!["Yes"]);

        // Outside `<metadata>` nothing counts at all.
        let err = rejection(
            "<package><files><metadata2/></files><id>A</id><version>1.0.0</version></package>",
        );
        assert!(err.contains("no <metadata>"), "{err}");
    }

    /// NuGet reads fields in the namespace that is the default at `<metadata>`;
    /// an element of the same local name in another namespace is not a field.
    #[test]
    fn fields_are_read_in_the_manifest_namespace_only() {
        let n = parse_nuspec(&manifest(
            r#"<id xmlns="urn:other">Fake</id><o:version xmlns:o="urn:other">9.9.9</o:version>
               <id>Real</id><version>1.0.0</version>"#,
        ))
        .unwrap();
        assert_eq!(n.id, "Real");
        assert_eq!(n.version, "1.0.0");

        // A prefixed manifest whose default namespace is empty: NuGet looks
        // for un-namespaced fields and finds none, and so does this parser.
        let prefixed = r#"<n:package xmlns:n="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
            <n:metadata><n:id>A</n:id><n:version>1.0.0</n:version></n:metadata></n:package>"#;
        assert!(rejection(prefixed).contains("missing <id>"));

        // Every real form parses: no namespace, and each schema version.
        for ns in [
            "",
            r#" xmlns="http://schemas.microsoft.com/packaging/2010/07/nuspec.xsd""#,
            r#" xmlns="http://schemas.microsoft.com/packaging/2011/08/nuspec.xsd""#,
            r#" xmlns="http://schemas.microsoft.com/packaging/2012/06/nuspec.xsd""#,
            r#" xmlns="http://schemas.microsoft.com/packaging/2013/01/nuspec.xsd""#,
        ] {
            let xml = format!(
                "<package{ns}><metadata><id>A</id><version>1.0.0</version></metadata></package>"
            );
            assert_eq!(parse_nuspec(&xml).unwrap().id, "A", "{ns}");
        }
        // The legacy shape with the namespace on <metadata> rather than <package>.
        let legacy = r#"<package><metadata xmlns="http://schemas.microsoft.com/packaging/2010/07/nuspec.xsd">
            <id>A</id><version>1.0.0</version></metadata></package>"#;
        assert_eq!(parse_nuspec(legacy).unwrap().id, "A");
    }

    /// Element names are case-sensitive in XML and in NuGet; `<ID>` is not an
    /// id to the client, so it is not one here either.
    #[test]
    fn field_names_are_case_sensitive() {
        let err = rejection(&manifest("<ID>A</ID><Version>1.0.0</Version>"));
        assert!(err.contains("missing <id>"), "{err}");
    }

    #[test]
    fn dependency_shapes_follow_nuget() {
        // NuGet ignores ungrouped dependencies once any group exists.
        let err = rejection(&manifest(
            r#"<id>A</id><version>1.0.0</version><dependencies>
               <dependency id="Legacy" /><group targetFramework="net8.0"><dependency id="B" /></group>
               </dependencies>"#,
        ));
        assert!(err.contains("mixes"), "{err}");

        // A dependency NuGet could not construct is not silently dropped.
        let err = rejection(&manifest(
            r#"<id>A</id><version>1.0.0</version><dependencies><dependency version="1.0" /></dependencies>"#,
        ));
        assert!(err.contains("without an id"), "{err}");

        // A <dependency> outside <dependencies> is not a dependency.
        let n = parse_nuspec(&manifest(
            r#"<id>A</id><version>1.0.0</version><dependency id="Stray" />"#,
        ))
        .unwrap();
        assert!(n.dependency_groups.is_empty());
    }

    #[test]
    fn documents_nuget_cannot_load_are_refused() {
        let err = rejection(&manifest("<id>A&custom;</id><version>1.0.0</version>"));
        assert!(err.contains("undefined entity"), "{err}");
        let err = rejection(
            "<!DOCTYPE package [<!ENTITY x \"y\">]><package><metadata><id>A</id>\
             <version>1.0.0</version></metadata></package>",
        );
        assert!(err.contains("DOCTYPE"), "{err}");
        let err = rejection(&manifest(
            r#"<id>A</id><version>1.0.0</version><dependencies><dependency id="B" id="C" /></dependencies>"#,
        ));
        assert!(err.contains("attribute"), "{err}");
        assert!(parse_nuspec(
            "<package><metadata><id>A</id><version>1</version></metadata></package><package/>"
        )
        .is_err());
    }

    /// quick-xml's duplicate-attribute check scans the attributes already
    /// seen, so one element with tens of thousands of attributes was quadratic
    /// to read, several times over. The count is now capped before that check
    /// can grow.
    #[test]
    fn attributes_per_element_are_capped() {
        let many: String = (0..20_000).map(|i| format!(" a{i}=\"x\"")).collect();
        let xml = manifest(&format!(
            "<id>A</id><version>1.0.0</version><dependencies><dependency id=\"B\"{many} /></dependencies>"
        ));
        let start = std::time::Instant::now();
        let err = rejection(&xml);
        assert!(err.contains("attributes"), "{err}");
        assert!(start.elapsed() < std::time::Duration::from_secs(1));

        // Namespace declarations are counted by the reader itself.
        let decls: String = (0..20_000)
            .map(|i| format!(" xmlns:p{i}=\"u{i}\""))
            .collect();
        assert!(parse_nuspec(&format!("<package{decls}><metadata/></package>")).is_err());

        // Ordinary elements are unaffected.
        let few: String = (0..MAX_ATTRIBUTES - 1)
            .map(|i| format!(" a{i}=\"x\""))
            .collect();
        let xml = manifest(&format!(
            "<id>A</id><version>1.0.0</version><dependencies><dependency id=\"B\"{few} /></dependencies>"
        ));
        assert!(parse_nuspec(&xml).is_ok());
    }

    #[test]
    fn the_document_size_is_capped() {
        let padding = " ".repeat(MAX_NUSPEC_BYTES);
        let err = rejection(&manifest(&format!(
            "<id>A</id><version>1.0.0</version>{padding}"
        )));
        assert!(err.contains("larger than"), "{err}");
    }

    #[tokio::test]
    async fn parses_off_the_runtime() {
        let n = parse_nuspec_blocking(SAMPLE.to_string()).await.unwrap();
        assert_eq!(n.id, "Contoso.Utils");
    }

    #[test]
    fn license_kinds() {
        let n = parse_nuspec(&manifest(
            r#"<id>A</id><version>1.0.0</version><license type="Expression">MIT</license>"#,
        ))
        .unwrap();
        assert_eq!(n.license_expression.as_deref(), Some("MIT"));
        let n = parse_nuspec(&manifest(
            r#"<id>A</id><version>1.0.0</version><license type="file">LICENSE.txt</license>"#,
        ))
        .unwrap();
        assert_eq!(n.license_file.as_deref(), Some("LICENSE.txt"));
        assert!(n.license_expression.is_none());
        // An unknown type is not read as an expression the policy evaluates.
        let n = parse_nuspec(&manifest(
            r#"<id>A</id><version>1.0.0</version><license type="other">MIT</license>"#,
        ))
        .unwrap();
        assert!(n.license_expression.is_none() && n.license_file.is_none());
    }
}
