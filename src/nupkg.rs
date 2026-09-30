//! Reading metadata out of a stored `.nupkg` archive.
//!
//! A `.nupkg` is a ZIP file. The crucial property exploited here is that the
//! ZIP *central directory* lives at the **end** of the file and references each
//! entry by offset. [`zip::ZipArchive`] therefore only needs to `seek` to read
//! the directory and then to the single small `.nuspec` entry — it never reads
//! the (potentially 25 GiB+) package body. The blocking ZIP work runs on a
//! dedicated thread via [`tokio::task::spawn_blocking`].

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::Error;

/// Hard cap on the `.nuspec` we are willing to read into memory. Real manifests
/// are a few KiB; this guards against a hostile archive declaring a giant one.
/// The parser enforces the same bound, so the two cannot drift apart.
const MAX_NUSPEC_BYTES: u64 = crate::nuspec::MAX_NUSPEC_BYTES as u64;

/// The most entries an archive may have.
///
/// The `zip` crate parses the whole central directory into memory — about five
/// times its on-disk size — before anything else can look at it, and a
/// directory of tiny entries compresses to nothing inside an upload. Real
/// packages, even SDK and runtime packs, have a few thousand entries; this is
/// checked against the count the archive declares, before the crate allocates.
pub const MAX_ARCHIVE_ENTRIES: u64 = 100_000;

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
    let mut archive = open_archive(path)?;

    // Names come from the central directory the crate already parsed; nothing
    // here opens an entry. `by_index` would seek to and read every local
    // header and set up a decompressor, and one encrypted or unsupported
    // entry anywhere in the package — which nothing else here needs — would
    // have rejected it.
    let mut entry_names = HashSet::with_capacity(archive.len());
    let mut nuspec_index: Option<usize> = None;
    for i in 0..archive.len() {
        let Some(name) = archive.name_for_index(i) else {
            continue;
        };
        let normalized = normalize_entry(name);
        // The nuspec is a root-level `*.nuspec` (no directory separators).
        if normalized.ends_with(".nuspec") && !normalized.contains('/') {
            // NuGet refuses a package with more than one; picking one of them
            // would be guessing which identity the client sees.
            if nuspec_index.replace(i).is_some() {
                return Err(Error::InvalidPackage(
                    "package contains more than one root .nuspec".into(),
                ));
            }
        }
        entry_names.insert(normalized);
    }

    let idx =
        nuspec_index.ok_or_else(|| Error::InvalidPackage("package contains no .nuspec".into()))?;
    let nuspec_entry = archive
        .by_index(idx)
        .map_err(|e| Error::InvalidPackage(format!("could not open nuspec: {e}")))?;
    if nuspec_entry.size() > MAX_NUSPEC_BYTES {
        return Err(Error::InvalidPackage("nuspec is implausibly large".into()));
    }

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

/// Open an archive for reading, after checking that its central directory
/// means the same thing to every reader (see [`check_central_directory`]).
pub(crate) fn open_archive(path: &Path) -> Result<zip::ZipArchive<File>, Error> {
    let mut file = File::open(path)?;
    let directory = check_central_directory(&mut file)?;
    let archive = zip::ZipArchive::new(file)
        .map_err(|e| Error::InvalidPackage(format!("not a valid zip/nupkg: {e}")))?;
    // The crate chose its end-of-central-directory record itself, skipping
    // candidates it could not use. If it landed on a different directory than
    // the last record — the one .NET's `ZipArchive` reads — the two readers
    // see different archives.
    if archive.central_directory_start() != directory.offset {
        return Err(Error::InvalidPackage(
            "archive's central directory is ambiguous (readers would disagree about where it \
             starts), so it is refused"
                .into(),
        ));
    }
    // The crate keys entries by name and keeps the last of a duplicated name.
    reject_duplicate_entries(directory.records, archive.len())?;
    Ok(archive)
}

/// The index of the entry named `normalized` (see [`normalize_entry`]), from the
/// central directory alone.
pub(crate) fn entry_index(archive: &zip::ZipArchive<File>, normalized: &str) -> Option<usize> {
    (0..archive.len()).find(|&i| {
        archive
            .name_for_index(i)
            .is_some_and(|name| normalize_entry(name) == normalized)
    })
}

/// Extract a single named entry (e.g. an embedded readme or icon) into memory.
/// Returns `None` if the entry is absent, and [`Error::InvalidPackage`] if it
/// is larger than `max_bytes`. Matching is case-insensitive and
/// `\\`/`/`-insensitive.
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
/// Each entry is streamed to its own file in `dir` (named `{uuid}.tmp`, so a
/// leftover is swept like any other stale upload) rather than held in memory:
/// a symbol package's PDBs compress well, and holding up to half a GiB of them
/// per request let a few small concurrent uploads exhaust memory.
///
/// `max_bytes_each` caps an individual entry and `max_total_bytes` the sum. An
/// entry over either is an error naming it — never a truncated file, which
/// used to be indexed as though it were the whole PDB. On any error, the files
/// written so far are removed; on success, removing them is the caller's job.
///
/// Returns the entries in archive order. Names that are absent are skipped.
pub async fn extract_entries_to_files(
    path: impl AsRef<Path>,
    entry_names: &[String],
    dir: impl AsRef<Path>,
    max_bytes_each: u64,
    max_total_bytes: u64,
) -> Result<Vec<ExtractedEntry>, Error> {
    let path: PathBuf = path.as_ref().to_path_buf();
    let dir: PathBuf = dir.as_ref().to_path_buf();
    let wanted: HashSet<String> = entry_names.iter().map(|n| normalize_entry(n)).collect();
    tokio::task::spawn_blocking(move || {
        let mut out = Vec::new();
        let result = extract_entries_blocking(
            &path,
            &wanted,
            &dir,
            max_bytes_each,
            max_total_bytes,
            &mut out,
        );
        if result.is_err() {
            for entry in &out {
                let _ = std::fs::remove_file(&entry.path);
            }
        }
        result.map(|()| out)
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("nupkg extract task panicked: {e}")))?
}

/// One archive entry written out by [`extract_entries_to_files`].
#[derive(Debug, Clone)]
pub struct ExtractedEntry {
    /// The entry's name as the archive spells it.
    pub name: String,
    /// Where its bytes were written.
    pub path: PathBuf,
    /// How many bytes that is.
    pub size: u64,
}

fn extract_entries_blocking(
    path: &Path,
    wanted: &HashSet<String>,
    dir: &Path,
    max_bytes_each: u64,
    max_total_bytes: u64,
    out: &mut Vec<ExtractedEntry>,
) -> Result<(), Error> {
    let mut archive = open_archive(path)?;

    let mut total: u64 = 0;
    for i in 0..archive.len() {
        let Some(name) = archive.name_for_index(i) else {
            continue;
        };
        if !wanted.contains(&normalize_entry(name)) {
            continue;
        }
        let name = name.to_string();
        let too_large = |what: &str, limit: u64| {
            Error::InvalidPackage(format!(
                "{name} takes the {what} past the {} MiB limit",
                limit / (1024 * 1024)
            ))
        };
        let mut entry = archive
            .by_index(i)
            .map_err(|e| Error::InvalidPackage(format!("could not open {name}: {e}")))?;
        // The declared sizes are checked first, so an honest oversized entry
        // costs nothing to refuse; the copy below enforces the same limits on
        // what actually decompresses.
        if entry.size() > max_bytes_each {
            return Err(too_large("entry", max_bytes_each));
        }
        if total.saturating_add(entry.size()) > max_total_bytes {
            return Err(too_large("package", max_total_bytes));
        }

        let target = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let mut file = File::create(&target)?;
        out.push(ExtractedEntry {
            name: name.clone(),
            path: target,
            size: 0,
        });
        let limit = max_bytes_each.min(max_total_bytes - total);
        let written = std::io::copy(&mut (&mut entry).take(limit.saturating_add(1)), &mut file)
            .map_err(|e| Error::InvalidPackage(format!("could not read {name}: {e}")))?;
        if written > limit {
            return Err(if written > max_bytes_each {
                too_large("entry", max_bytes_each)
            } else {
                too_large("package", max_total_bytes)
            });
        }
        // Durable before anything renames it into the store.
        file.sync_all()?;
        total += written;
        if let Some(last) = out.last_mut() {
            last.size = written;
        }
    }
    Ok(())
}

fn extract_file_blocking(
    path: &Path,
    needle: &str,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, Error> {
    let mut archive = open_archive(path)?;
    let Some(idx) = entry_index(&archive, needle) else {
        return Ok(None);
    };

    let entry = archive
        .by_index(idx)
        .map_err(|e| Error::InvalidPackage(format!("could not open entry: {e}")))?;
    let name = entry.name().to_string();
    let too_large = || {
        Error::InvalidPackage(format!(
            "package entry {name} is larger than the {} KiB allowed for it",
            max_bytes / 1024
        ))
    };
    // Refused rather than cut short. A truncated readme was stored and
    // flagged as present — possibly ending part-way through a UTF-8
    // sequence — and a truncated icon is a broken image; either way the feed
    // served something other than what the package contains.
    if entry.size() > max_bytes {
        return Err(too_large());
    }
    let mut buf = Vec::new();
    // One byte past the cap, in case the declared size understates it.
    entry
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| Error::InvalidPackage(format!("could not read {name}: {e}")))?;
    if buf.len() as u64 > max_bytes {
        return Err(too_large());
    }
    Ok(Some(buf))
}

pub(crate) fn normalize_entry(name: &str) -> String {
    name.replace('\\', "/").to_ascii_lowercase()
}

/// How far back from the end of the file to look for the end-of-central-directory
/// record. Its trailing comment may be up to 64 KiB, and the record itself is 22
/// bytes.
const EOCD_SEARCH_WINDOW: u64 = 22 + 65_535;
/// `PK\x05\x06` — the end-of-central-directory record.
const EOCD_SIGNATURE: [u8; 4] = *b"PK\x05\x06";
/// `PK\x06\x07` — the ZIP64 end-of-central-directory locator.
const ZIP64_LOCATOR_SIGNATURE: [u8; 4] = *b"PK\x06\x07";
/// `PK\x06\x06` — the ZIP64 end-of-central-directory record.
const ZIP64_EOCD_SIGNATURE: [u8; 4] = *b"PK\x06\x06";
/// `PK\x01\x02` — one central-directory file header.
const CENTRAL_HEADER_SIGNATURE: [u8; 4] = *b"PK\x01\x02";

/// Where the central directory is, and how many records it really holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CentralDirectory {
    offset: u64,
    records: u64,
}

/// Locate the central directory the way .NET's `ZipArchive` does — the *last*
/// end-of-central-directory record, and its ZIP64 counterpart when there is
/// one — and count the records in it by walking them.
///
/// Every count and offset a reader could take from the archive has to agree:
/// the entries on this disk and in total (the `zip` crate reads one, .NET the
/// other), the 32-bit and ZIP64 records, the declared count and the records
/// physically present (.NET reads until the signatures stop), and the declared
/// directory size and where the walk ended. Any disagreement is exactly the
/// room two readers need to see different entries, so it is refused.
fn check_central_directory(file: &mut File) -> Result<CentralDirectory, Error> {
    let ambiguous = |what: &str| {
        Error::InvalidPackage(format!(
            "archive's central directory is inconsistent ({what}), so readers would disagree \
             about its entries; it is refused"
        ))
    };

    let len = file.metadata()?.len();
    let window = EOCD_SEARCH_WINDOW.min(len);
    let tail_start = len - window;
    file.seek(SeekFrom::Start(tail_start))?;
    let mut tail = vec![0u8; window as usize];
    file.read_exact(&mut tail)?;

    // The last signature with room for a whole record after it. A comment may
    // contain the same four bytes, but .NET takes the last one too, and the
    // crate choosing any other is caught by the caller.
    let pos = tail
        .windows(4)
        .rposition(|w| w == EOCD_SIGNATURE)
        .filter(|pos| pos + 22 <= tail.len())
        .ok_or_else(|| {
            Error::InvalidPackage("not a valid zip/nupkg: no end of central directory".into())
        })?;
    let eocd = &tail[pos..pos + 22];
    let disk = u16_at(eocd, 4);
    let directory_disk = u16_at(eocd, 6);
    let on_disk = u16_at(eocd, 8);
    let total = u16_at(eocd, 10);
    let size = u32_at(eocd, 12);
    let offset = u32_at(eocd, 16);

    let saturated = disk == u16::MAX
        || directory_disk == u16::MAX
        || on_disk == u16::MAX
        || total == u16::MAX
        || size == u32::MAX
        || offset == u32::MAX;

    // A ZIP64 locator sits immediately before the record. Whatever comes
    // last before the end records bounds the directory.
    let eocd_at = tail_start + pos as u64;
    let mut directory_limit = eocd_at;
    let zip64 = if eocd_at >= 20 {
        let mut locator = [0u8; 20];
        file.seek(SeekFrom::Start(eocd_at - 20))?;
        file.read_exact(&mut locator)?;
        if locator[..4] == ZIP64_LOCATOR_SIGNATURE {
            let record_at = u64_at(&locator, 8);
            let mut record = [0u8; 56];
            if record_at
                .checked_add(56)
                .is_none_or(|end| end > eocd_at - 20)
            {
                return Err(ambiguous("ZIP64 record out of bounds"));
            }
            directory_limit = record_at;
            file.seek(SeekFrom::Start(record_at))?;
            file.read_exact(&mut record)?;
            if record[..4] != ZIP64_EOCD_SIGNATURE {
                return Err(ambiguous("ZIP64 locator points at no record"));
            }
            Some((
                u32_at(&record, 16) as u64,
                u32_at(&record, 20) as u64,
                u64_at(&record, 24),
                u64_at(&record, 32),
                u64_at(&record, 40),
                u64_at(&record, 48),
            ))
        } else {
            None
        }
    } else {
        None
    };

    let (disk, directory_disk, on_disk, total, size, offset) = match zip64 {
        Some(values) => {
            // The crate prefers the ZIP64 values whenever a locator is present;
            // .NET only when a 32-bit field is saturated. Unsaturated fields
            // must therefore say the same thing in both records.
            let narrow = [
                (disk as u64, values.0, disk == u16::MAX),
                (directory_disk as u64, values.1, directory_disk == u16::MAX),
                (on_disk as u64, values.2, on_disk == u16::MAX),
                (total as u64, values.3, total == u16::MAX),
                (size as u64, values.4, size == u32::MAX),
                (offset as u64, values.5, offset == u32::MAX),
            ];
            if narrow.iter().any(|&(n, wide, sat)| !sat && n != wide) {
                return Err(ambiguous("ZIP64 record disagrees with the 32-bit one"));
            }
            values
        }
        None if saturated => return Err(ambiguous("ZIP64 fields without a ZIP64 record")),
        None => (
            disk as u64,
            directory_disk as u64,
            on_disk as u64,
            total as u64,
            size as u64,
            offset as u64,
        ),
    };

    if disk != 0 || directory_disk != 0 {
        return Err(Error::InvalidPackage(
            "multi-disk archives are not supported".into(),
        ));
    }
    if on_disk != total {
        return Err(ambiguous("entries on this disk differ from the total"));
    }
    if total > MAX_ARCHIVE_ENTRIES {
        return Err(Error::InvalidPackage(format!(
            "archive declares {total} entries, more than the {MAX_ARCHIVE_ENTRIES} allowed"
        )));
    }
    if offset
        .checked_add(size)
        .is_none_or(|end| end > directory_limit)
    {
        return Err(ambiguous("directory runs past its end record"));
    }

    // Walk the records. A crafted directory can hold more records than it
    // declares; .NET reads them all and the crate only as many as declared.
    file.seek(SeekFrom::Start(offset))?;
    let mut reader = BufReader::new(Read::by_ref(file).take(size.saturating_add(4)));
    let mut records: u64 = 0;
    let mut walked: u64 = 0;
    let mut header = [0u8; 46];
    loop {
        let mut signature = [0u8; 4];
        match reader.read_exact(&mut signature) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(Error::Io(e)),
        }
        if signature != CENTRAL_HEADER_SIGNATURE {
            break;
        }
        records += 1;
        if records > total {
            return Err(ambiguous("more records than declared"));
        }
        header[..4].copy_from_slice(&signature);
        reader
            .read_exact(&mut header[4..])
            .map_err(|_| ambiguous("truncated record"))?;
        let variable =
            u16_at(&header, 28) as u64 + u16_at(&header, 30) as u64 + u16_at(&header, 32) as u64;
        let skipped = std::io::copy(
            &mut Read::by_ref(&mut reader).take(variable),
            &mut std::io::sink(),
        )?;
        if skipped != variable {
            return Err(ambiguous("truncated record"));
        }
        walked += 46 + variable;
    }
    if records != total {
        return Err(ambiguous("fewer records than declared"));
    }
    if walked != size {
        return Err(ambiguous("records do not fill the declared directory size"));
    }
    Ok(CentralDirectory { offset, records })
}

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
/// `unique` is the deduplicated count the crate kept; `records` is how many
/// [`check_central_directory`] walked. Taking the count from the walk rather
/// than a header field is what makes it hold for ZIP64 archives, and for a
/// header that simply lies.
fn reject_duplicate_entries(records: u64, unique: usize) -> Result<(), Error> {
    if records != unique as u64 {
        return Err(Error::InvalidPackage(format!(
            "archive names the same entry more than once ({records} records, {unique} distinct \
             names); readers disagree about which one wins, so it is refused"
        )));
    }
    Ok(())
}

fn u16_at(buf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([buf[at], buf[at + 1]])
}

fn u32_at(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

fn u64_at(buf: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&buf[at..at + 8]);
    u64::from_le_bytes(bytes)
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
        raw_zip(entries, Trailer::default())
    }

    /// How [`raw_zip`] writes the end of the archive; every field defaults to
    /// what a well-formed archive says.
    #[derive(Default, Clone)]
    struct Trailer {
        /// "Entries on this disk" in the 32-bit record.
        on_disk: Option<u16>,
        /// "Total entries" in the 32-bit record.
        total: Option<u16>,
        /// Write a ZIP64 record and locator, saturating the 32-bit fields.
        zip64: bool,
        /// Both entry counts in the ZIP64 record, instead of the real one.
        zip64_count: Option<u64>,
        /// Archive comment.
        comment: Vec<u8>,
    }

    fn raw_zip(entries: &[(&str, &[u8])], trailer: Trailer) -> Vec<u8> {
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
        if trailer.zip64 {
            let record_at = out.len() as u64;
            out.extend_from_slice(b"PK\x06\x06");
            out.extend_from_slice(&44u64.to_le_bytes()); // size of the rest
            out.extend_from_slice(&45u16.to_le_bytes()); // version made by
            out.extend_from_slice(&45u16.to_le_bytes()); // version needed
            out.extend_from_slice(&0u32.to_le_bytes()); // this disk
            out.extend_from_slice(&0u32.to_le_bytes()); // disk with CD
            out.extend_from_slice(&trailer.zip64_count.unwrap_or(count as u64).to_le_bytes());
            out.extend_from_slice(&trailer.zip64_count.unwrap_or(count as u64).to_le_bytes());
            out.extend_from_slice(&(dir_size as u64).to_le_bytes());
            out.extend_from_slice(&(dir_offset as u64).to_le_bytes());
            out.extend_from_slice(b"PK\x06\x07");
            out.extend_from_slice(&0u32.to_le_bytes()); // disk with the record
            out.extend_from_slice(&record_at.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes()); // total disks
        }
        let (narrow_count, narrow_size, narrow_offset) = if trailer.zip64 {
            (u16::MAX, u32::MAX, u32::MAX)
        } else {
            (count, dir_size, dir_offset)
        };
        out.extend_from_slice(b"PK\x05\x06");
        out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        out.extend_from_slice(&0u16.to_le_bytes()); // disk with CD
        out.extend_from_slice(&trailer.on_disk.unwrap_or(narrow_count).to_le_bytes());
        out.extend_from_slice(&trailer.total.unwrap_or(narrow_count).to_le_bytes());
        out.extend_from_slice(&narrow_size.to_le_bytes());
        out.extend_from_slice(&narrow_offset.to_le_bytes());
        out.extend_from_slice(&(trailer.comment.len() as u16).to_le_bytes());
        out.extend_from_slice(&trailer.comment);
        out
    }

    async fn read_bytes(bytes: &[u8]) -> Result<ArchiveContents, Error> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.nupkg");
        std::fs::write(&path, bytes).unwrap();
        read_archive(&path).await
    }

    fn refused(result: Result<ArchiveContents, Error>, needle: &str) {
        match result {
            Err(Error::InvalidPackage(m)) => assert!(m.contains(needle), "unexpected: {m}"),
            other => panic!("expected a refusal mentioning {needle:?}, got {other:?}"),
        }
    }

    /// The duplicate check used to read the count from the 32-bit record and
    /// opt out for ZIP64 archives entirely, so a ZIP64 archive could carry two
    /// manifests unchecked.
    #[tokio::test]
    async fn duplicates_are_caught_in_zip64_archives_too() {
        let (a, b) = (manifest("First.Id"), manifest("Second.Id"));
        let dup = [("P.nuspec", a.as_slice()), ("P.nuspec", b.as_slice())];
        let zip64 = Trailer {
            zip64: true,
            ..Default::default()
        };
        refused(
            read_bytes(&raw_zip(&dup, zip64.clone())).await,
            "more than once",
        );
        // A well-formed ZIP64 archive is still read.
        let ok = [("P.nuspec", a.as_slice())];
        let contents = read_bytes(&raw_zip(&ok, zip64)).await.unwrap();
        assert!(contents.nuspec_xml.contains("First.Id"));
    }

    /// The crate reads "entries on this disk", the old check read "total
    /// entries". Declaring one duplicate-free total over two records let the
    /// crate collapse a duplicate that the check never saw.
    #[tokio::test]
    async fn the_two_entry_counts_must_agree() {
        let (a, b) = (manifest("First.Id"), manifest("Second.Id"));
        let dup = [("P.nuspec", a.as_slice()), ("P.nuspec", b.as_slice())];
        let lying = Trailer {
            on_disk: Some(2),
            total: Some(1),
            ..Default::default()
        };
        refused(read_bytes(&raw_zip(&dup, lying)).await, "inconsistent");
    }

    /// .NET reads records until the signatures stop; the crate reads as many as
    /// declared. Records past the declared count are a second view.
    #[tokio::test]
    async fn records_beyond_the_declared_count_are_refused() {
        let (a, b) = (manifest("First.Id"), manifest("Second.Id"));
        let dup = [("P.nuspec", a.as_slice()), ("P.nuspec", b.as_slice())];
        let short = Trailer {
            on_disk: Some(1),
            total: Some(1),
            ..Default::default()
        };
        refused(read_bytes(&raw_zip(&dup, short)).await, "inconsistent");
    }

    /// The crate skips an end record it cannot use and tries an earlier one;
    /// .NET takes the last. A fake record hidden in the comment is where the
    /// two part ways.
    #[tokio::test]
    async fn a_fake_end_record_in_the_comment_is_refused() {
        let a = manifest("First.Id");
        let mut comment = b"PK\x05\x06".to_vec();
        comment.extend_from_slice(&[0xEE; 18]);
        let fake = Trailer {
            comment,
            ..Default::default()
        };
        assert!(read_bytes(&raw_zip(&[("P.nuspec", &a)], fake))
            .await
            .is_err());
    }

    /// The crate allocates the whole directory before anything can look at
    /// it, so an absurd declared count is refused from the end record alone.
    #[tokio::test]
    async fn an_absurd_entry_count_is_refused_before_parsing() {
        let a = manifest("First.Id");
        let huge = Trailer {
            zip64: true,
            zip64_count: Some(MAX_ARCHIVE_ENTRIES + 1),
            ..Default::default()
        };
        refused(
            read_bytes(&raw_zip(&[("P.nuspec", &a)], huge)).await,
            "entries, more than",
        );
    }

    /// An embedded file over its cap is refused, not cut short: the cut used to
    /// be stored as the readme, possibly mid-way through a UTF-8 sequence.
    #[tokio::test]
    async fn an_oversized_entry_is_refused_rather_than_truncated() {
        let nuspec = r#"<package><metadata><id>Contoso.Utils</id><version>1.0.0</version></metadata></package>"#;
        let (_dir, path) = make_nupkg(nuspec);
        // `docs/README.md` holds 8 bytes.
        let exact = extract_file(&path, "docs/README.md", 8).await.unwrap();
        assert_eq!(exact.as_deref(), Some(&b"# Readme"[..]));
        let err = extract_file(&path, "DOCS\\readme.md", 7).await.unwrap_err();
        assert!(
            matches!(&err, Error::InvalidPackage(m) if m.contains("docs/README.md is larger")),
            "{err}"
        );
        assert!(extract_file(&path, "missing.md", 8)
            .await
            .unwrap()
            .is_none());
    }

    /// Entries are streamed to files, and one over either cap is an error —
    /// never a short file that goes on to be indexed as the whole PDB.
    #[tokio::test]
    async fn entries_are_extracted_to_files_within_caps() {
        let a = manifest("First.Id");
        let bytes = zip_with_entries(&[
            ("P.nuspec", &a),
            ("lib/a.pdb", &[1u8; 100]),
            ("lib/b.pdb", &[2u8; 50]),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.snupkg");
        std::fs::write(&path, &bytes).unwrap();
        let out = tempfile::tempdir().unwrap();
        let names = vec!["lib/a.pdb".to_string(), "LIB\\B.PDB".to_string()];
        let files_in = |d: &Path| std::fs::read_dir(d).unwrap().count();

        let got = extract_entries_to_files(&path, &names, out.path(), 100, 150)
            .await
            .unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].name, "lib/a.pdb");
        assert_eq!(std::fs::read(&got[0].path).unwrap(), vec![1u8; 100]);
        assert_eq!(got[1].size, 50);
        for entry in &got {
            std::fs::remove_file(&entry.path).unwrap();
        }

        // One entry over its cap.
        let err = extract_entries_to_files(&path, &names, out.path(), 99, 1000)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("lib/a.pdb"), "{err}");
        // The total over its cap, on the second entry: the first is cleaned up.
        let err = extract_entries_to_files(&path, &names, out.path(), 100, 149)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("lib/b.pdb"), "{err}");
        assert_eq!(files_in(out.path()), 0, "partial output left behind");
    }

    #[tokio::test]
    async fn two_root_manifests_are_refused() {
        let (a, b) = (manifest("First.Id"), manifest("Second.Id"));
        let bytes = zip_with_entries(&[("A.nuspec", &a), ("B.nuspec", &b)]);
        refused(read_bytes(&bytes).await, "more than one root .nuspec");
    }

    /// Only the manifest is opened, so an entry the reader cannot decode
    /// elsewhere in the package no longer rejects it.
    #[tokio::test]
    async fn entries_other_than_the_manifest_are_not_opened() {
        let a = manifest("First.Id");
        let mut bytes = zip_with_entries(&[("P.nuspec", &a), ("lib/x.dll", b"MZ")]);
        // Mark the second entry's central record as compressed with an
        // unsupported method (14, LZMA: not built in) — reading it would fail.
        let record = bytes
            .windows(4)
            .enumerate()
            .filter(|(_, w)| *w == b"PK\x01\x02")
            .nth(1)
            .map(|(i, _)| i)
            .unwrap();
        bytes[record + 10..record + 12].copy_from_slice(&14u16.to_le_bytes());
        let contents = read_bytes(&bytes).await.unwrap();
        assert!(contents.contains("lib/x.dll"));
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
        // The records really were walked, rather than the check opting out.
        let mut file = File::open(&path).unwrap();
        assert_eq!(check_central_directory(&mut file).unwrap().records, 3);
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
