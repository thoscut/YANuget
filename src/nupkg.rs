//! Reading metadata out of a stored `.nupkg` archive.
//!
//! A `.nupkg` is a ZIP file. The crucial property exploited here is that the
//! ZIP *central directory* lives at the **end** of the file and references each
//! entry by offset. [`zip::ZipArchive`] therefore only needs to `seek` to read
//! the directory and then to the single small `.nuspec` entry — it never reads
//! the (potentially 25 GiB+) package body. The blocking ZIP work runs on a
//! dedicated thread via [`tokio::task::spawn_blocking`].

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::error::Error;

/// Hard cap on the `.nuspec` we are willing to read into memory. Real manifests
/// are a few KiB; this guards against a hostile archive declaring a giant one.
const MAX_NUSPEC_BYTES: u64 = 16 * 1024 * 1024;

/// Metadata extracted from a `.nupkg` without reading its payload.
#[derive(Debug, Clone)]
pub struct ArchiveContents {
    /// The raw XML of the root `.nuspec`.
    pub nuspec_xml: String,
    /// Lower-cased names of every entry in the archive (from the central
    /// directory). Used to confirm embedded readme/icon files actually exist.
    pub entry_names: HashSet<String>,
}

impl ArchiveContents {
    /// Whether the archive contains an entry at `path` (case-insensitive,
    /// `\\` normalized to `/`).
    pub fn contains(&self, path: &str) -> bool {
        let needle = normalize_entry(path);
        self.entry_names.contains(&needle)
    }
}

/// Read the `.nuspec` (and the entry listing) from a stored package file.
pub async fn read_archive(path: impl AsRef<Path>) -> Result<ArchiveContents, Error> {
    let path: PathBuf = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || read_archive_blocking(&path))
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("nupkg read task panicked: {e}")))?
}

fn read_archive_blocking(path: &Path) -> Result<ArchiveContents, Error> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| Error::InvalidPackage(format!("not a valid zip/nupkg: {e}")))?;

    // Collect entry names and locate the root-level `.nuspec`.
    let mut entry_names = HashSet::with_capacity(archive.len());
    let mut nuspec_index: Option<usize> = None;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| Error::InvalidPackage(format!("corrupt zip entry: {e}")))?;
        let name = entry.name().to_string();
        let normalized = normalize_entry(&name);
        // The nuspec is a root-level `*.nuspec` (no directory separators).
        if nuspec_index.is_none() && normalized.ends_with(".nuspec") && !normalized.contains('/') {
            if entry.size() > MAX_NUSPEC_BYTES {
                return Err(Error::InvalidPackage("nuspec is implausibly large".into()));
            }
            nuspec_index = Some(i);
        }
        entry_names.insert(normalized);
    }

    let idx =
        nuspec_index.ok_or_else(|| Error::InvalidPackage("package contains no .nuspec".into()))?;
    let nuspec_entry = archive
        .by_index(idx)
        .map_err(|e| Error::InvalidPackage(format!("could not open nuspec: {e}")))?;

    let mut buf = String::new();
    nuspec_entry
        .take(MAX_NUSPEC_BYTES)
        .read_to_string(&mut buf)
        .map_err(|e| Error::InvalidPackage(format!("nuspec is not valid UTF-8: {e}")))?;

    Ok(ArchiveContents {
        nuspec_xml: buf,
        entry_names,
    })
}

/// Extract a single named entry (e.g. an embedded readme or icon) into memory,
/// capped at `max_bytes`. Returns `None` if the entry is absent. Matching is
/// case-insensitive and `\\`/`/`-insensitive.
pub async fn extract_file(
    path: impl AsRef<Path>,
    entry_name: &str,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, Error> {
    let path: PathBuf = path.as_ref().to_path_buf();
    let needle = normalize_entry(entry_name);
    tokio::task::spawn_blocking(move || extract_file_blocking(&path, &needle, max_bytes))
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("nupkg extract task panicked: {e}")))?
}

fn extract_file_blocking(
    path: &Path,
    needle: &str,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, Error> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| Error::InvalidPackage(format!("not a valid zip/nupkg: {e}")))?;

    let mut found: Option<usize> = None;
    for i in 0..archive.len() {
        let entry = archive
            .by_index(i)
            .map_err(|e| Error::InvalidPackage(format!("corrupt zip entry: {e}")))?;
        if normalize_entry(entry.name()) == *needle {
            found = Some(i);
            break;
        }
    }
    let Some(idx) = found else { return Ok(None) };

    let entry = archive
        .by_index(idx)
        .map_err(|e| Error::InvalidPackage(format!("could not open entry: {e}")))?;
    let mut buf = Vec::new();
    entry
        .take(max_bytes)
        .read_to_end(&mut buf)
        .map_err(Error::Io)?;
    Ok(Some(buf))
}

fn normalize_entry(name: &str) -> String {
    name.replace('\\', "/").to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// Build a minimal `.nupkg` on disk and return its path (kept alive by the
    /// returned `TempDir`).
    fn make_nupkg(nuspec: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.nupkg");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("Contoso.Utils.nuspec", opts).unwrap();
        zip.write_all(nuspec.as_bytes()).unwrap();
        zip.start_file("lib/net8.0/Contoso.Utils.dll", opts)
            .unwrap();
        zip.write_all(b"\x4d\x5a fake assembly").unwrap();
        zip.start_file("docs/README.md", opts).unwrap();
        zip.write_all(b"# Readme").unwrap();
        zip.finish().unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn reads_nuspec_and_entries() {
        let nuspec = r#"<package><metadata><id>Contoso.Utils</id><version>1.0.0</version></metadata></package>"#;
        let (_dir, path) = make_nupkg(nuspec);

        let contents = read_archive(&path).await.unwrap();
        assert!(contents.nuspec_xml.contains("Contoso.Utils"));
        assert!(contents.contains("docs/README.md"));
        assert!(contents.contains("lib/net8.0/Contoso.Utils.dll"));
        assert!(!contents.contains("missing.txt"));
    }

    #[tokio::test]
    async fn rejects_non_zip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.nupkg");
        std::fs::write(&path, b"this is not a zip file at all").unwrap();
        let err = read_archive(&path).await.unwrap_err();
        assert!(matches!(err, Error::InvalidPackage(_)));
    }

    #[tokio::test]
    async fn rejects_zip_without_nuspec() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonuspec.nupkg");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("readme.txt", opts).unwrap();
        zip.write_all(b"hi").unwrap();
        zip.finish().unwrap();

        let err = read_archive(&path).await.unwrap_err();
        assert!(matches!(err, Error::InvalidPackage(_)));
    }
}
