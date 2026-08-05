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

    reject_duplicate_entries(path, archive.len())?;

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

/// Extract several named entries in **one** pass over the archive.
///
/// [`extract_file`] re-opens the file and re-parses the central directory on
/// every call, then scans it linearly for the name. Calling it once per entry is
/// therefore quadratic in the entry count, and a `.snupkg` can name as many
/// entries as it likes — so an upload of a few hundred KiB could occupy a
/// blocking thread for tens of minutes. This opens and parses once.
///
/// `max_bytes_each` caps an individual entry and `max_total_bytes` caps the sum,
/// because the results are held in memory. Entries that would push the total
/// over the cap are not read, and the returned vector is short — the caller is
/// expected to notice and report rather than silently proceed.
///
/// Returns the entries in archive order, each as `(original name, bytes)`.
/// Names that are absent are skipped.
pub async fn extract_entries(
    path: impl AsRef<Path>,
    entry_names: &[String],
    max_bytes_each: u64,
    max_total_bytes: u64,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let path: PathBuf = path.as_ref().to_path_buf();
    let wanted: std::collections::HashSet<String> =
        entry_names.iter().map(|n| normalize_entry(n)).collect();
    tokio::task::spawn_blocking(move || {
        extract_entries_blocking(&path, &wanted, max_bytes_each, max_total_bytes)
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("nupkg extract task panicked: {e}")))?
}

fn extract_entries_blocking(
    path: &Path,
    wanted: &std::collections::HashSet<String>,
    max_bytes_each: u64,
    max_total_bytes: u64,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| Error::InvalidPackage(format!("not a valid zip/nupkg: {e}")))?;

    let mut out = Vec::new();
    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| Error::InvalidPackage(format!("corrupt zip entry: {e}")))?;
        let name = entry.name().to_string();
        if !wanted.contains(&normalize_entry(&name)) {
            continue;
        }
        let remaining = max_total_bytes.saturating_sub(total);
        if remaining == 0 {
            break;
        }
        let mut buf = Vec::new();
        (&mut entry)
            .take(max_bytes_each.min(remaining))
            .read_to_end(&mut buf)
            .map_err(Error::Io)?;
        total = total.saturating_add(buf.len() as u64);
        out.push((name, buf));
    }
    Ok(out)
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

/// How far back from the end of the file to look for the end-of-central-directory
/// record. Its trailing comment may be up to 64 KiB, and the record itself is 22
/// bytes.
const EOCD_SEARCH_WINDOW: u64 = 66 * 1024;
/// `PK\x05\x06` — the end-of-central-directory signature.
const EOCD_SIGNATURE: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];

/// Reject an archive whose central directory names the same entry twice.
///
/// A ZIP may physically contain two records with one name, and readers disagree
/// about which one wins: the `zip` crate keys entries by name and keeps the
/// **last**, while .NET's `ZipArchive` — what the NuGet client uses — enumerates
/// records and takes the **first**. A package carrying two `.nuspec` entries
/// therefore has one identity here and a different one on the client, which is a
/// split view of the same bytes: the feed advertises a package the client will
/// not agree it installed.
///
/// Rather than bet on two implementations agreeing, such an archive is refused.
/// `unique` is the deduplicated count the reader saw; the end-of-central-directory
/// record says how many were actually written. ZIP64 archives store that count
/// elsewhere and mark this field `0xFFFF`; those skip the check rather than risk
/// rejecting a valid package.
fn reject_duplicate_entries(path: &Path, unique: usize) -> Result<(), Error> {
    let Some(declared) = declared_entry_count(path)? else {
        return Ok(());
    };
    if declared as usize > unique {
        return Err(Error::InvalidPackage(format!(
            "archive names the same entry more than once ({declared} records, {unique} distinct \
             names); readers disagree about which one wins, so it is refused"
        )));
    }
    Ok(())
}

/// Read the entry count from the end-of-central-directory record. `None` when it
/// cannot be determined (no EOCD found, or a ZIP64 archive).
fn declared_entry_count(path: &Path) -> Result<Option<u16>, Error> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let window = EOCD_SEARCH_WINDOW.min(len);
    file.seek(SeekFrom::Start(len - window))?;
    let mut buf = vec![0u8; window as usize];
    file.read_exact(&mut buf)?;

    // Scan backwards for the last EOCD signature: a comment could contain the
    // same four bytes, and the real record is the final one.
    let Some(pos) = buf
        .windows(4)
        .rposition(|w| w == EOCD_SIGNATURE)
        .filter(|pos| pos + 22 <= buf.len())
    else {
        return Ok(None);
    };
    // Offset 10 in the record: total number of central-directory entries.
    let total = u16::from_le_bytes([buf[pos + 10], buf[pos + 11]]);
    // The ZIP64 sentinel — the real count lives in the ZIP64 EOCD record.
    if total == u16::MAX {
        return Ok(None);
    }
    Ok(Some(total))
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

    /// Build a minimal *stored* ZIP by hand, so the same name can appear twice.
    /// `zip::ZipWriter` refuses to emit a duplicate, but nothing stops an
    /// attacker's own writer — which is the whole point.
    fn zip_with_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        // The reader validates CRC-32 on a full entry read, so the fixture has
        // to carry real checksums.
        fn crc32(data: &[u8]) -> u32 {
            let mut crc = !0u32;
            for &byte in data {
                crc ^= byte as u32;
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xEDB8_8320 & (!(crc & 1)).wrapping_add(1));
                }
            }
            !crc
        }

        let mut out = Vec::new();
        let mut directory = Vec::new();
        for (name, body) in entries {
            let crc = crc32(body);
            let offset = out.len() as u32;
            let n = name.as_bytes();
            // Local file header.
            out.extend_from_slice(b"PK\x03\x04");
            out.extend_from_slice(&20u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u16.to_le_bytes()); // flags
            out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
            out.extend_from_slice(&0u32.to_le_bytes()); // time+date
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(n.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra len
            out.extend_from_slice(n);
            out.extend_from_slice(body);

            // Central directory record.
            directory.extend_from_slice(b"PK\x01\x02");
            directory.extend_from_slice(&20u16.to_le_bytes()); // version made by
            directory.extend_from_slice(&20u16.to_le_bytes()); // version needed
            directory.extend_from_slice(&0u16.to_le_bytes()); // flags
            directory.extend_from_slice(&0u16.to_le_bytes()); // method
            directory.extend_from_slice(&0u32.to_le_bytes()); // time+date
            directory.extend_from_slice(&crc.to_le_bytes());
            directory.extend_from_slice(&(body.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(body.len() as u32).to_le_bytes());
            directory.extend_from_slice(&(n.len() as u16).to_le_bytes());
            directory.extend_from_slice(&0u16.to_le_bytes()); // extra len
            directory.extend_from_slice(&0u16.to_le_bytes()); // comment len
            directory.extend_from_slice(&0u16.to_le_bytes()); // disk start
            directory.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            directory.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            directory.extend_from_slice(&offset.to_le_bytes());
            directory.extend_from_slice(n);
        }
        let dir_offset = out.len() as u32;
        let dir_size = directory.len() as u32;
        let count = entries.len() as u16;
        out.extend_from_slice(&directory);
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        out.extend_from_slice(&0u16.to_le_bytes()); // disk with CD
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&count.to_le_bytes());
        out.extend_from_slice(&dir_size.to_le_bytes());
        out.extend_from_slice(&dir_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }

    fn manifest(id: &str) -> Vec<u8> {
        format!("<package><metadata><id>{id}</id><version>1.0.0</version></metadata></package>")
            .into_bytes()
    }

    /// A ZIP may physically hold two records with one name. The `zip` crate
    /// keys entries by name and keeps the last; .NET's `ZipArchive` — what the
    /// NuGet client uses — enumerates records and takes the first. An archive
    /// relying on that disagreement is refused.
    #[tokio::test]
    async fn rejects_an_archive_with_duplicate_entry_names() {
        let first = manifest("First.Id");
        let second = manifest("Second.Id");
        let bytes = zip_with_entries(&[("P.nuspec", &first), ("P.nuspec", &second)]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dup.nupkg");
        std::fs::write(&path, &bytes).unwrap();

        let err = read_archive(&path).await.unwrap_err();
        assert!(
            matches!(&err, Error::InvalidPackage(m) if m.contains("more than once")),
            "unexpected error: {err}"
        );
    }

    /// The same shape without the duplicate must still be accepted — the check
    /// keys on record count, so an off-by-one here would reject every package.
    #[tokio::test]
    async fn a_hand_built_archive_without_duplicates_is_accepted() {
        let only = manifest("Only.Id");
        let dll = b"MZ".to_vec();
        let bytes = zip_with_entries(&[("P.nuspec", &only), ("lib/a.dll", &dll)]);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ok.nupkg");
        std::fs::write(&path, &bytes).unwrap();

        let contents = read_archive(&path).await.unwrap();
        assert!(contents.nuspec_xml.contains("Only.Id"));
        assert!(contents.contains("lib/a.dll"));
    }

    #[tokio::test]
    async fn an_ordinary_archive_is_not_mistaken_for_a_duplicate() {
        let nuspec = r#"<package><metadata><id>Contoso.Utils</id><version>1.0.0</version></metadata></package>"#;
        let (_dir, path) = make_nupkg(nuspec);
        assert!(read_archive(&path).await.is_ok());
        // The count really was read, rather than the check silently opting out.
        assert_eq!(declared_entry_count(&path).unwrap(), Some(3));
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
