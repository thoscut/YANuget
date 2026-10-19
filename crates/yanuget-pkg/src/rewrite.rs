//! Rewriting a `.nupkg`: replace, add or remove entries and the manifest, and
//! copy everything else as it is.
//!
//! Unchanged entries are copied raw: their compressed bytes and CRC go from
//! the old archive to the new one without being inflated or deflated again,
//! so a package with a multi-gigabyte installer inside costs a file copy, and
//! memory does not grow with the package. New content is streamed from a file
//! or written from a small buffer.
//!
//! The OPC parts NuGet and Chocolatey read are kept consistent:
//! `[Content_Types].xml` gains a content type for any extension (or
//! extension-less part) it does not cover yet, and the core properties
//! (`package/services/metadata/core-properties/*.psmdcp`) are written anew
//! from a replaced manifest. `_rels/.rels` names the manifest and the core
//! properties by path, and neither path changes. `.signature.p7s` is dropped
//! whenever anything changes, since any change invalidates it.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::XmlVersion;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use crate::error::Error;
use crate::nupkg::{normalize_entry, open_archive};

/// The package signature, which any change invalidates.
const SIGNATURE: &str = ".signature.p7s";
const CONTENT_TYPES: &str = "[content_types].xml";
const RELATIONSHIPS: &str = "_rels/.rels";
const CORE_PROPERTIES_DIR: &str = "package/services/metadata/core-properties/";
/// What NuGet itself gives a part of a type it does not know.
const OCTET: &str = "application/octet";
/// The most of `[Content_Types].xml` that is read into memory; a real one is
/// a few hundred bytes.
const MAX_CONTENT_TYPES_BYTES: u64 = 1024 * 1024;
/// The longest entry name [`Rewrite::put`] takes.
const MAX_NAME_CHARS: usize = 512;

/// New content for an entry.
#[derive(Debug, Clone)]
pub enum Source {
    /// Held in memory: a report, a script.
    Bytes(Vec<u8>),
    /// Streamed from a file: an installer.
    File(PathBuf),
}

/// What to change. An empty rewrite copies the package unchanged.
#[derive(Debug, Clone, Default)]
pub struct Rewrite {
    nuspec: Option<String>,
    /// Keyed by normalized name; the value keeps the name as given.
    put: BTreeMap<String, (String, Source)>,
    remove: BTreeSet<String>,
}

impl Rewrite {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the manifest. The core properties are written anew from it.
    pub fn nuspec(mut self, xml: impl Into<String>) -> Self {
        self.nuspec = Some(xml.into());
        self
    }

    /// Replace the entry `name`, or add it when the package has none.
    /// Matching is case-insensitive and `\`/`/`-insensitive; a replaced entry
    /// keeps the spelling the package has. A name to add must be a relative
    /// path of `A-Z a-z 0-9 . _ -` segments joined by `/`.
    pub fn put(mut self, name: impl Into<String>, content: Source) -> Self {
        let name = name.into();
        self.put.insert(normalize_entry(&name), (name, content));
        self
    }

    /// Remove the entry `name`, which the package must have.
    pub fn remove(mut self, name: impl AsRef<str>) -> Self {
        self.remove.insert(normalize_entry(name.as_ref()));
        self
    }

    /// Whether the rewrite changes nothing.
    pub fn is_empty(&self) -> bool {
        self.nuspec.is_none() && self.put.is_empty() && self.remove.is_empty()
    }
}

/// What a rewrite did, by entry name as written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rewritten {
    pub replaced: Vec<String>,
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Whether the package carried a signature, which was dropped.
    pub signature_dropped: bool,
}

/// Rewrite the package at `input` into `output` (created or truncated) on a
/// blocking thread. On error `output` is removed.
pub async fn rewrite(
    input: impl AsRef<Path>,
    output: impl AsRef<Path>,
    plan: Rewrite,
) -> Result<Rewritten, Error> {
    let input = input.as_ref().to_path_buf();
    let output = output.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || rewrite_blocking(&input, &output, &plan))
        .await
        .map_err(|e| Error::Task(format!("nupkg rewrite task panicked: {e}")))?
}

/// [`rewrite`], on the calling thread.
pub fn rewrite_blocking(input: &Path, output: &Path, plan: &Rewrite) -> Result<Rewritten, Error> {
    let result = rewrite_into(input, output, plan);
    if result.is_err() {
        let _ = std::fs::remove_file(output);
    }
    result
}

/// How one entry of the input is written out.
enum Action {
    Copy,
    Drop,
    Put,
    Nuspec,
    ContentTypes,
    CoreProperties,
}

fn rewrite_into(input: &Path, output: &Path, plan: &Rewrite) -> Result<Rewritten, Error> {
    let mut archive = open_archive(input)?;
    let changing = !plan.is_empty();
    if let Some(xml) = &plan.nuspec {
        crate::nuspec::parse_nuspec(xml).map_err(|e| invalid(format!("the new nuspec: {e}")))?;
    }

    // Decide every entry's fate from the central directory first, so the
    // content types can be completed before anything is written.
    let mut actions = Vec::with_capacity(archive.len());
    let mut names_out: Vec<String> = Vec::new();
    let mut seen = HashSet::new();
    let mut done = Rewritten::default();
    let mut has_content_types = false;
    let mut has_nuspec = false;
    for i in 0..archive.len() {
        let name = crate::nupkg::entry_name(&archive, i)?
            .ok_or_else(|| Error::InvalidPackage("an entry has no name".into()))?;
        let normalized = normalize_entry(&name);
        seen.insert(normalized.clone());
        let is_nuspec = normalized.ends_with(".nuspec") && !normalized.contains('/');
        let is_core =
            normalized.starts_with(CORE_PROPERTIES_DIR) && normalized.ends_with(".psmdcp");
        let reserved =
            is_nuspec || is_core || normalized == CONTENT_TYPES || normalized == RELATIONSHIPS;
        if reserved && (plan.put.contains_key(&normalized) || plan.remove.contains(&normalized)) {
            return Err(invalid(format!(
                "{name} is part of the package structure; replace the manifest with \
                 Rewrite::nuspec, and the rest follows from the entries"
            )));
        }
        has_nuspec |= is_nuspec;
        let action = if normalized == SIGNATURE && changing {
            done.signature_dropped = true;
            Action::Drop
        } else if plan.remove.contains(&normalized) {
            done.removed.push(name.clone());
            Action::Drop
        } else if plan.put.contains_key(&normalized) {
            done.replaced.push(name.clone());
            Action::Put
        } else if is_nuspec && plan.nuspec.is_some() {
            Action::Nuspec
        } else if normalized == CONTENT_TYPES && changing {
            has_content_types = true;
            Action::ContentTypes
        } else if is_core && plan.nuspec.is_some() {
            Action::CoreProperties
        } else {
            Action::Copy
        };
        if !matches!(action, Action::Drop) {
            names_out.push(normalized);
        }
        actions.push((name, action));
    }
    if !has_nuspec {
        return Err(Error::InvalidPackage("package contains no .nuspec".into()));
    }
    if let Some(absent) = plan.remove.iter().find(|n| !seen.contains(*n)) {
        return Err(invalid(format!(
            "cannot remove {absent}: the package has no such entry"
        )));
    }
    if let Some(clash) = plan.put.keys().find(|n| plan.remove.contains(*n)) {
        return Err(invalid(format!("{clash} is both put and removed")));
    }
    let mut added = Vec::new();
    for (normalized, (name, _)) in &plan.put {
        if seen.contains(normalized) {
            continue;
        }
        check_new_name(name)?;
        let reserved = normalized == CONTENT_TYPES
            || normalized == RELATIONSHIPS
            || normalized == SIGNATURE
            || normalized.starts_with("package/")
            || normalized.starts_with("_rels/")
            || (normalized.ends_with(".nuspec") && !normalized.contains('/'));
        if reserved {
            return Err(invalid(format!(
                "{name} is reserved for the package structure"
            )));
        }
        names_out.push(normalized.clone());
        added.push(name.clone());
    }
    if changing && !has_content_types {
        return Err(Error::InvalidPackage(
            "package has no [Content_Types].xml, so it cannot be kept consistent".into(),
        ));
    }

    let file = File::create(output)?;
    let mut copied = Vec::new();
    let mut zip = ZipWriter::new(BufWriter::with_capacity(1024 * 1024, file));
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (i, (name, action)) in actions.iter().enumerate() {
        match action {
            Action::Drop => {}
            Action::Copy => {
                let entry = archive
                    .by_index_raw(i)
                    .map_err(|e| Error::InvalidPackage(format!("could not open {name}: {e}")))?;
                copied.push(Copied {
                    name: name.clone(),
                    size: entry.size(),
                    compressed: entry.compressed_size(),
                    crc32: entry.crc32(),
                });
                zip.raw_copy_file(entry).map_err(write_error)?;
            }
            Action::Put => {
                let (_, source) = &plan.put[&normalize_entry(name)];
                write_source(&mut zip, name, source, deflated)?;
            }
            Action::Nuspec => {
                let xml = plan.nuspec.as_deref().unwrap_or_default();
                zip.start_file(name.as_str(), deflated)
                    .map_err(write_error)?;
                zip.write_all(xml.as_bytes())?;
            }
            Action::ContentTypes => {
                let mut entry = archive
                    .by_index(i)
                    .map_err(|e| Error::InvalidPackage(format!("could not open {name}: {e}")))?;
                if entry.size() > MAX_CONTENT_TYPES_BYTES {
                    return Err(Error::InvalidPackage(format!(
                        "{name} is implausibly large"
                    )));
                }
                let mut xml = String::new();
                (&mut entry)
                    .take(MAX_CONTENT_TYPES_BYTES)
                    .read_to_string(&mut xml)
                    .map_err(|e| Error::InvalidPackage(format!("could not read {name}: {e}")))?;
                drop(entry);
                let xml = complete_content_types(&xml, &names_out)?;
                zip.start_file(name.as_str(), deflated)
                    .map_err(write_error)?;
                zip.write_all(xml.as_bytes())?;
            }
            Action::CoreProperties => {
                let xml = plan.nuspec.as_deref().unwrap_or_default();
                let nuspec = crate::nuspec::parse_nuspec(xml)?;
                zip.start_file(name.as_str(), deflated)
                    .map_err(write_error)?;
                zip.write_all(core_properties(&nuspec).as_bytes())?;
            }
        }
    }
    for name in &added {
        let (_, source) = &plan.put[&normalize_entry(name)];
        write_source(&mut zip, name, source, deflated)?;
    }
    let mut out = zip.finish().map_err(write_error)?;
    out.flush()?;
    let file = out.into_inner().map_err(|e| Error::Io(e.into_error()))?;
    drop(file);
    verify_copies(output, &copied)?;
    File::open(output)?.sync_all()?;
    done.added = added;
    Ok(done)
}

/// An entry copied raw, as the source archive describes it.
struct Copied {
    name: String,
    size: u64,
    compressed: u64,
    crc32: u32,
}

/// Check that every entry copied raw reads back with the size, compressed size
/// and CRC it had, from the central directory alone. Whatever the ZIP writer
/// got wrong is found here rather than by `choco install`.
fn verify_copies(path: &Path, copied: &[Copied]) -> Result<(), Error> {
    let mut archive = open_archive(path)?;
    for c in copied {
        let entry = archive
            .by_name(&c.name)
            .map_err(|e| Error::Io(std::io::Error::other(format!("{}: {e}", c.name))))?;
        if (entry.size(), entry.compressed_size(), entry.crc32()) != (c.size, c.compressed, c.crc32)
        {
            return Err(Error::Io(std::io::Error::other(format!(
                "the rewritten package does not carry {} as the source does",
                c.name
            ))));
        }
    }
    Ok(())
}

fn write_source(
    zip: &mut ZipWriter<BufWriter<File>>,
    name: &str,
    source: &Source,
    options: SimpleFileOptions,
) -> Result<(), Error> {
    match source {
        Source::Bytes(bytes) => {
            zip.start_file(name, options).map_err(write_error)?;
            zip.write_all(bytes)?;
        }
        Source::File(path) => {
            let mut file = File::open(path)?;
            // ZIP64 sizes when the content could need them; deflate never
            // grows data by more than a fraction of a percent.
            let large = file.metadata()?.len() >= u64::from(u32::MAX) / 2;
            zip.start_file(name, options.large_file(large))
                .map_err(write_error)?;
            std::io::copy(&mut file, zip)?;
        }
    }
    Ok(())
}

/// Add a content type for every part in `names` (normalized) that the types
/// do not cover: a `Default` for an extension, an `Override` for a part with
/// none. The document is otherwise left as it is, byte for byte.
fn complete_content_types(xml: &str, names: &[String]) -> Result<String, Error> {
    let bad = |e: &dyn std::fmt::Display| {
        Error::InvalidPackage(format!("[Content_Types].xml is malformed: {e}"))
    };
    let mut extensions = HashSet::new();
    let mut overrides = HashSet::new();
    let mut reader = quick_xml::Reader::from_str(xml);
    loop {
        match reader.read_event().map_err(|e| bad(&e))? {
            Event::Start(e) | Event::Empty(e) => {
                let local = e.local_name();
                let (wanted, set) = match local.as_ref() {
                    "Default" => ("Extension", &mut extensions),
                    "Override" => ("PartName", &mut overrides),
                    _ => continue,
                };
                for attribute in e.attributes() {
                    let attribute = attribute.map_err(|e| bad(&e))?;
                    if attribute.key.as_ref() == wanted {
                        let value = attribute
                            .normalized_value(XmlVersion::Implicit1_0)
                            .map_err(|e| bad(&e))?;
                        set.insert(value.to_ascii_lowercase());
                    }
                }
            }
            Event::DocType(_) => return Err(bad(&"it has a DOCTYPE")),
            Event::Eof => break,
            _ => {}
        }
    }

    let mut missing = String::new();
    let mut added = HashSet::new();
    for name in names {
        if name == CONTENT_TYPES || name.ends_with('/') {
            continue;
        }
        let file = name.rsplit('/').next().unwrap_or(name);
        match file.rsplit_once('.') {
            Some((_, ext)) if !ext.is_empty() => {
                if !extensions.contains(ext) && added.insert(format!(".{ext}")) {
                    missing.push_str(&format!(
                        "<Default Extension=\"{}\" ContentType=\"{OCTET}\" />",
                        escape(ext)
                    ));
                }
            }
            _ => {
                let part = format!("/{name}");
                if !overrides.contains(&part) && added.insert(part.clone()) {
                    missing.push_str(&format!(
                        "<Override PartName=\"{}\" ContentType=\"{OCTET}\" />",
                        escape(&part)
                    ));
                }
            }
        }
    }
    if missing.is_empty() {
        return Ok(xml.to_string());
    }
    let at = xml
        .rfind("</Types>")
        .ok_or_else(|| bad(&"it has no closing </Types>"))?;
    Ok(format!("{}{missing}{}", &xml[..at], &xml[at..]))
}

/// The core properties NuGet writes for a manifest.
fn core_properties(nuspec: &crate::nuspec::Nuspec) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><coreProperties \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:dcterms=\"http://purl.org/dc/terms/\" \
         xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" \
         xmlns=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\">",
    );
    let mut field = |tag: &str, value: Option<&str>| {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            out.push_str(&format!("<{tag}>{}</{tag}>", escape(v)));
        }
    };
    field("dc:creator", nuspec.authors.as_deref());
    field("dc:description", nuspec.description.as_deref());
    field("dc:identifier", Some(&nuspec.id));
    field("version", Some(&nuspec.version));
    field("dc:title", nuspec.title.as_deref());
    field("keywords", nuspec.tags.as_deref());
    field("lastModifiedBy", Some("yanuget-pkg"));
    out.push_str("</coreProperties>");
    out
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// A name to add: a relative path of plain segments, so it needs no OPC
/// percent-encoding and cannot leave the package root.
fn check_new_name(name: &str) -> Result<(), Error> {
    let ok = !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name.split('/').all(|segment| {
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        });
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "{name:?} is not a name to add: use '/'-separated segments of letters, \
             digits, '.', '_' and '-'"
        )))
    }
}

fn invalid(why: String) -> Error {
    Error::InvalidRewrite(why)
}

fn write_error(e: zip::result::ZipError) -> Error {
    match e {
        zip::result::ZipError::Io(e) => Error::Io(e),
        other => Error::Io(std::io::Error::other(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NUSPEC: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd">
  <metadata><id>Tool</id><version>1.0.0</version><authors>Vendor</authors>
  <description>A tool.</description></metadata>
</package>"#;

    const TYPES: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><Types \
        xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
        <Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\" />\
        <Default Extension=\"nuspec\" ContentType=\"application/octet\" />\
        <Default Extension=\"PS1\" ContentType=\"application/octet\" />\
        <Default Extension=\"exe\" ContentType=\"application/octet\" />\
        <Default Extension=\"psmdcp\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\" />\
        <Default Extension=\"p7s\" ContentType=\"application/octet\" /></Types>";

    const PSMDCP: &str = "package/services/metadata/core-properties/0123.psmdcp";

    /// A package laid out as `nuget pack`/`choco pack` lay one out, signed.
    fn package(dir: &Path) -> PathBuf {
        let path = dir.join("tool.1.0.0.nupkg");
        let mut zip = ZipWriter::new(File::create(&path).unwrap());
        let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let entries: [(&str, &[u8], SimpleFileOptions); 7] = [
            ("_rels/.rels", b"<Relationships />", deflated),
            ("Tool.nuspec", NUSPEC.as_bytes(), deflated),
            (
                "tools/chocolateyInstall.ps1",
                b"Install-ChocolateyPackage @args",
                deflated,
            ),
            (
                "tools/My%20Tool.exe",
                &[0x4d, 0x5a, 1, 2, 3, 4, 5, 6, 7, 8],
                stored,
            ),
            (PSMDCP, b"<coreProperties />", deflated),
            ("[Content_Types].xml", TYPES.as_bytes(), deflated),
            (".signature.p7s", b"signature", stored),
        ];
        for (name, bytes, options) in entries {
            zip.start_file(name, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    fn entries(path: &Path) -> Vec<(String, CompressionMethod, u32, Vec<u8>)> {
        let mut archive = open_archive(path).unwrap();
        (0..archive.len())
            .map(|i| {
                let mut e = archive.by_index_raw(i).unwrap();
                let mut raw = Vec::new();
                e.read_to_end(&mut raw).unwrap();
                (
                    e.name().unwrap().into_owned(),
                    e.compression(),
                    e.crc32(),
                    raw,
                )
            })
            .collect()
    }

    fn read(path: &Path, name: &str) -> String {
        let mut archive = open_archive(path).unwrap();
        let mut s = String::new();
        archive
            .by_name(name)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        s
    }

    #[test]
    fn an_empty_rewrite_keeps_every_entry_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let input = package(dir.path());
        let output = dir.path().join("out.nupkg");
        let done = rewrite_blocking(&input, &output, &Rewrite::new()).unwrap();
        assert_eq!(done, Rewritten::default());
        assert_eq!(entries(&input), entries(&output));
    }

    #[test]
    fn entries_are_replaced_added_and_removed_and_the_structure_follows() {
        let dir = tempfile::tempdir().unwrap();
        let input = package(dir.path());
        let installer = dir.path().join("setup.msi");
        std::fs::write(&installer, b"\xd0\xcf\x11\xe0 msi").unwrap();
        let output = dir.path().join("out.nupkg");
        let nuspec = NUSPEC
            .replace("1.0.0", "1.0.0.20261016")
            .replace("A tool.", "A &lt;tool&gt; &amp; more.");
        let plan = Rewrite::new()
            .nuspec(nuspec.clone())
            .put(
                "TOOLS\\chocolateyinstall.ps1",
                Source::Bytes(b"# rewritten".to_vec()),
            )
            .put("tools/files/setup.msi", Source::File(installer))
            .put("internalizer/report.json", Source::Bytes(b"{}".to_vec()))
            .put("tools/LICENSE", Source::Bytes(b"text".to_vec()))
            .remove("tools/My%20Tool.exe");
        let done = rewrite_blocking(&input, &output, &plan).unwrap();
        assert_eq!(done.replaced, ["tools/chocolateyInstall.ps1"]);
        assert_eq!(done.removed, ["tools/My%20Tool.exe"]);
        assert!(done.added.contains(&"tools/files/setup.msi".to_string()));
        assert!(done.signature_dropped);

        // Readable as a package, by the same checks an upload passes.
        let names: Vec<String> = entries(&output).into_iter().map(|e| e.0).collect();
        assert!(!names
            .iter()
            .any(|n| n == ".signature.p7s" || n.ends_with(".exe")));
        assert_eq!(read(&output, "Tool.nuspec"), nuspec);
        assert_eq!(read(&output, "tools/chocolateyInstall.ps1"), "# rewritten");
        let mut msi = Vec::new();
        open_archive(&output)
            .unwrap()
            .by_name("tools/files/setup.msi")
            .unwrap()
            .read_to_end(&mut msi)
            .unwrap();
        assert_eq!(msi, b"\xd0\xcf\x11\xe0 msi");
        // Untouched entries are still copied raw.
        let raw =
            |path: &Path, name: &str| entries(path).into_iter().find(|e| e.0 == name).unwrap();
        assert_eq!(raw(&input, "_rels/.rels"), raw(&output, "_rels/.rels"));

        let types = read(&output, "[Content_Types].xml");
        assert!(
            types.starts_with(TYPES.trim_end_matches("</Types>")),
            "{types}"
        );
        assert!(types.contains("<Default Extension=\"msi\" ContentType=\"application/octet\" />"));
        assert!(types.contains("<Default Extension=\"json\""));
        assert!(
            types.contains("<Override PartName=\"/tools/license\""),
            "{types}"
        );
        assert_eq!(
            types.matches("Extension=\"ps1\"").count(),
            0,
            "covered as PS1 already"
        );

        let core = read(&output, PSMDCP);
        assert!(core.contains("<version>1.0.0.20261016</version>"), "{core}");
        assert!(core.contains("<dc:description>A &lt;tool&gt; &amp; more.</dc:description>"));
        assert!(core.contains("<dc:identifier>Tool</dc:identifier>"));
    }

    #[test]
    fn a_rewrite_that_cannot_be_done_leaves_no_output() {
        let dir = tempfile::tempdir().unwrap();
        let input = package(dir.path());
        let output = dir.path().join("out.nupkg");
        let refused = |plan: Rewrite| {
            let err = rewrite_blocking(&input, &output, &plan).unwrap_err();
            assert!(!output.exists());
            assert!(matches!(err, Error::InvalidRewrite(_)), "{err}");
            err.to_string()
        };
        assert!(refused(Rewrite::new().remove("Tool.nuspec")).contains("package structure"));
        assert!(
            refused(Rewrite::new().put("[Content_Types].xml", Source::Bytes(vec![])))
                .contains("package structure")
        );
        assert!(refused(Rewrite::new().remove("tools/absent.ps1")).contains("no such entry"));
        assert!(refused(Rewrite::new().nuspec("<package/>")).contains("new nuspec"));
        for name in [
            "../evil.ps1",
            "/abs.ps1",
            "a//b",
            "tools/my file.exe",
            "Other.nuspec",
            "package/x",
        ] {
            refused(Rewrite::new().put(name, Source::Bytes(vec![])));
        }
        assert!(refused(
            Rewrite::new()
                .put("tools/x.ps1", Source::Bytes(vec![]))
                .remove("TOOLS/X.ps1")
        )
        .contains("no such entry"));
    }

    #[test]
    fn content_types_are_completed_only_where_needed() {
        let names = [
            "tools/a.ps1".to_string(),
            "tools/b".to_string(),
            "[content_types].xml".to_string(),
        ];
        let xml = "<Types><Default Extension=\"ps1\" ContentType=\"x\"/><Override PartName=\"/TOOLS/B\" ContentType=\"x\"/></Types>";
        assert_eq!(complete_content_types(xml, &names).unwrap(), xml);
        assert!(complete_content_types("<Types><!DOCTYPE x></Types>", &names).is_err());
        assert!(complete_content_types("<Types>", &["c.txt".to_string()]).is_err());
    }

    /// Done when a 5 GB entry passes in constant memory. Takes a while and
    /// some disk (the entry is zeros, so it deflates to a few MiB), hence
    /// ignored by default: `cargo test -p yanuget-pkg -- --ignored`.
    #[test]
    #[ignore]
    fn a_five_gigabyte_entry_passes_in_constant_memory() {
        const SIZE: u64 = 5 * 1024 * 1024 * 1024;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("big.nupkg");
        {
            let mut zip = ZipWriter::new(BufWriter::new(File::create(&input).unwrap()));
            let deflated =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            zip.start_file("Big.nuspec", deflated).unwrap();
            zip.write_all(NUSPEC.as_bytes()).unwrap();
            zip.start_file("[Content_Types].xml", deflated).unwrap();
            zip.write_all(TYPES.as_bytes()).unwrap();
            zip.start_file("tools/big.exe", deflated.large_file(true))
                .unwrap();
            std::io::copy(&mut std::io::repeat(0).take(SIZE), &mut zip).unwrap();
            zip.finish().unwrap();
        }
        let peak = || -> u64 {
            let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
            status
                .lines()
                .find_map(|l| l.strip_prefix("VmHWM:"))
                .and_then(|v| v.trim().trim_end_matches(" kB").trim().parse().ok())
                .unwrap_or(0)
        };
        let before = peak();
        let output = dir.path().join("out.nupkg");
        let plan = Rewrite::new().put("internalizer/report.json", Source::Bytes(b"{}".to_vec()));
        rewrite_blocking(&input, &output, &plan).unwrap();
        let grown_kib = peak().saturating_sub(before);
        assert!(grown_kib < 64 * 1024, "peak memory grew by {grown_kib} KiB");
        let mut archive = open_archive(&output).unwrap();
        let mut big = archive.by_name("tools/big.exe").unwrap();
        assert_eq!(big.size(), SIZE);
        // Reading it to the end checks the CRC too.
        assert_eq!(std::io::copy(&mut big, &mut std::io::sink()).unwrap(), SIZE);
    }
}
