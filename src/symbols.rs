//! The symbol-package (`.snupkg`) indexing pipeline.
//!
//! A symbol package is a ZIP, just like a `.nupkg`, carrying the `.pdb` files
//! that match an already-published package. Indexing reads its manifest to
//! identify the owning package, then for every Portable PDB inside computes the
//! SSQP lookup key (see [`crate::pdb`]) and stores the PDB under that key so a
//! debugger can fetch it with a single request.
//!
//! NuGet requires the matching package to be pushed *before* its symbols, so a
//! symbol push for an unknown id/version is rejected.
//!
//! # Whose symbols are these?
//!
//! The symbol store is global — a debugger asks for a key and nothing else —
//! and both halves of the key (the PDB id and the file name) are chosen by
//! whoever pushes. A PDB id is also public: it is written into the assembly
//! that everyone can download. So a push credential on any feed must not be
//! able to decide what a debugger working on someone else's package is served
//! (its Source Link URLs, its embedded sources). Three things see to that:
//!
//! 1. **Every Portable PDB must belong to an assembly in the owning version's
//!    stored `.nupkg`**, as on nuget.org: the `.dll`/`.exe` beside it (same
//!    folder, same name) must carry a CodeView entry with the PDB's GUID and
//!    stamp, and a `PdbChecksum` entry that matches the PDB's own hash. The
//!    checksum is what makes this binding: the id alone can be copied into a
//!    forged PDB, the hash of the real PDB cannot be met by a different one.
//!    (Assemblies from compilers older than Visual Studio 15.9 carry no
//!    checksum; nuget.org refuses their symbols too.)
//! 2. **A key is claimed once, atomically.** The `(key, file)` row is claimed
//!    under a per-key lock before any bytes are written, and a claim is never
//!    moved to another package or version.
//! 3. **Stored bytes are never replaced by different ones.** A re-push of the
//!    same PDB is a no-op; anything else under an existing key is refused.
//!
//! The push also holds the owning version's lock (see [`crate::locks`]) so it
//! cannot interleave with a purge or an overwrite of that version, and it is
//! all-or-nothing: every PDB is validated before any is stored, and a failure
//! while storing undoes the claims this push made.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::database::PackageDatabase;
use crate::error::{Error, Result};
use crate::nuspec;
use crate::storage::{PackageContent, PackageStorage};
use crate::version::NuGetVersion;
use crate::{locks, nupkg, pdb, pe};

/// Hard cap on a single `.pdb`. An entry over it refuses the push rather than
/// being stored cut short.
const MAX_PDB_BYTES: u64 = 256 * 1024 * 1024;

/// Caps on a symbol package as a whole. Without them a `.snupkg` of a few
/// hundred KiB — entry names and deflated zeros both compress enormously —
/// decides how much disk and time one request consumes. A real symbol package
/// carries one PDB per assembly, so a few hundred is already generous.
const MAX_PDB_ENTRIES: usize = 512;
const MAX_PDB_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

/// The outcome of indexing a symbol package.
#[derive(Debug, Clone)]
pub struct SymbolResult {
    pub id: String,
    pub version: NuGetVersion,
    /// How many PDBs were successfully indexed by signature.
    pub indexed: usize,
    /// How many PDBs were stored but could not be indexed (e.g. native PDBs).
    pub skipped: usize,
}

/// Index a `.snupkg` whose bytes already live at `temp_path`. On success the
/// temp file has been moved into storage; on failure it is removed.
pub async fn index_symbol_package(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    temp_path: PathBuf,
) -> Result<SymbolResult> {
    let result = index_inner(storage, db, feed, &temp_path).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }
    result
}

async fn index_inner(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    temp_path: &PathBuf,
) -> Result<SymbolResult> {
    // 1. Read the manifest to learn which package these symbols belong to.
    let archive = nupkg::read_archive(temp_path).await?;
    let manifest = nuspec::parse_nuspec_blocking(archive.nuspec_xml.clone()).await?;
    let version = NuGetVersion::parse(&manifest.version)
        .map_err(|e| Error::InvalidPackage(format!("invalid <version>: {e}")))?;
    let id = manifest.id.clone();
    let normalized = version.normalized();

    // 2. The owning package must already be a member of this feed (NuGet's
    //    ordering rule). Symbols themselves are stored globally by SSQP key.
    if !db.exists(feed, &id, &version).await? {
        return Err(Error::PackageNotFound);
    }

    // 3. Collect the PDBs.
    let pdb_entries: Vec<String> = archive
        .entry_names
        .iter()
        .filter(|name| name.ends_with(".pdb"))
        .cloned()
        .collect();

    if pdb_entries.len() > MAX_PDB_ENTRIES {
        return Err(Error::InvalidPackage(format!(
            "symbol package contains {} .pdb entries, more than the {MAX_PDB_ENTRIES} allowed",
            pdb_entries.len()
        )));
    }

    // One pass over the archive rather than one pass *per entry*. Extracting
    // each entry separately re-opened the file and re-scanned the central
    // directory every time, so the cost grew with the square of the entry
    // count: a 200 KiB upload naming 2000 PDBs took 25 seconds of a blocking
    // thread, and the entry count had no upper bound at all.
    //
    // Each PDB goes to its own temp file next to the upload, not into memory:
    // up to 512 MiB of highly compressible PDBs per request was a small upload
    // away, and a few concurrent ones exhausted memory. An entry over the cap
    // is refused, where it used to be cut short and indexed as if whole.
    let dir = temp_path.parent().unwrap_or_else(|| Path::new("."));
    let extracted = TempFiles(
        nupkg::extract_entries_to_files(
            temp_path,
            &pdb_entries,
            dir,
            MAX_PDB_BYTES,
            MAX_PDB_TOTAL_BYTES,
        )
        .await?,
    );

    // 4. From here on the owning version must stay what it is: a purge or an
    //    overwrite of it running alongside would otherwise leave symbols
    //    claimed for a version that is gone, or validated against a package
    //    that has since been replaced.
    let _version_guard = locks::lock_version(&id, &normalized).await;
    if !db.exists(feed, &id, &version).await? {
        return Err(Error::PackageNotFound);
    }
    let PackageContent::LocalPath(package_path) = storage.get_package(&id, &normalized).await?;

    // 5. Validate every PDB before storing any.
    let entries: Vec<(String, PathBuf)> = extracted
        .0
        .iter()
        .map(|e| (e.name.clone(), e.path.clone()))
        .collect();
    let (verified, skipped) = tokio::task::spawn_blocking(move || verify(&package_path, &entries))
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("symbol verification task panicked: {e}")))??;

    // 6. Claim and store, all or nothing.
    let claimed = store_all(storage, db, &id, &version, &verified).await?;

    // 7. Store the whole .snupkg alongside the package (moves the temp file).
    if let Err(e) = storage
        .store_symbol_package(&id, &normalized, temp_path.clone())
        .await
    {
        release(storage, db, &claimed).await;
        return Err(e);
    }

    Ok(SymbolResult {
        id,
        version,
        indexed: verified.len(),
        skipped,
    })
}

/// A PDB that has been matched to an assembly of the owning package.
struct Verified {
    key: String,
    filename: String,
    /// The extracted file, moved into the store if the key is new.
    path: PathBuf,
    /// SHA-256 of its bytes, to compare with whatever is stored under the key.
    sha256: [u8; 32],
}

/// Match every extracted PDB against the stored package (blocking). Returns
/// the Portable PDBs, each proven to belong to one of the package's
/// assemblies, and how many PDBs were not Portable ones.
fn verify(package: &Path, entries: &[(String, PathBuf)]) -> Result<(Vec<Verified>, usize)> {
    let mut archive = nupkg::open_archive(package)?;
    let mut verified: BTreeMap<(String, String), Verified> = BTreeMap::new();
    let mut skipped = 0;
    for (name, path) in entries {
        let filename = file_name(name);
        let mut file = File::open(path)?;
        let Some(id) = pdb::read_pdb_id(&mut file)? else {
            // Not a Portable PDB (e.g. a native/Windows PDB). Keep the payload
            // via the stored .snupkg, but it cannot be indexed.
            tracing::warn!(pdb = %name, "symbol file is not an indexable Portable PDB");
            skipped += 1;
            continue;
        };
        let refuse = |why: String| Error::InvalidPackage(format!("symbol file {name}: {why}"));

        // The assembly this PDB belongs to: same folder, same name.
        let normalized = nupkg::normalize_entry(name);
        let stem = normalized.strip_suffix(".pdb").unwrap_or(&normalized);
        let Some(index) = [".dll", ".exe"]
            .iter()
            .find_map(|ext| nupkg::entry_index(&archive, &format!("{stem}{ext}")))
        else {
            return Err(refuse(format!(
                "the package has no {stem}.dll or {stem}.exe for it to belong to"
            )));
        };
        let size = archive
            .by_index(index)
            .map_err(|e| refuse(format!("could not open its assembly: {e}")))?
            .size();
        let mut scan = pe::DebugDirectoryScan::new(size);
        let directory = loop {
            let mut assembly = archive
                .by_index(index)
                .map_err(|e| refuse(format!("could not open its assembly: {e}")))?;
            let pass = scan
                .pass(&mut assembly)
                .map_err(|e| refuse(format!("could not read its assembly: {e}")))?;
            match pass {
                pe::Pass::Done(directory) => break directory,
                pe::Pass::Again => continue,
            }
        };
        let Some(directory) = directory else {
            return Err(refuse("its assembly is not a readable PE image".into()));
        };

        if !directory
            .codeview
            .iter()
            .any(|cv| cv.guid == id.guid && cv.stamp == id.stamp)
        {
            return Err(refuse(
                "its id matches no CodeView entry of its assembly; it was not built with \
                 this package"
                    .into(),
            ));
        }
        let mut checked = false;
        for entry in &directory.checksums {
            let Some(actual) = pdb::pdb_checksum(&mut file, &id, &entry.algorithm)? else {
                continue;
            };
            if actual != entry.checksum {
                return Err(refuse(format!(
                    "its {} checksum does not match the one its assembly records",
                    entry.algorithm
                )));
            }
            checked = true;
        }
        if !checked {
            return Err(refuse(
                "its assembly records no PDB checksum to verify it against (it needs a \
                 compiler from Visual Studio 15.9 or later, as on nuget.org)"
                    .into(),
            ));
        }

        let key = id.ssqp_key();
        let sha256 = sha256_file(&mut file)?;
        match verified.get(&(key.clone(), filename.clone())) {
            // The same PDB twice (in two folders): store it once.
            Some(seen) if seen.sha256 == sha256 => {}
            Some(_) => {
                return Err(refuse(format!(
                    "another PDB in this package has the same key {key} and name"
                )));
            }
            None => {
                verified.insert(
                    (key.clone(), filename.clone()),
                    Verified {
                        key,
                        filename,
                        path: path.clone(),
                        sha256,
                    },
                );
            }
        }
    }
    Ok((verified.into_values().collect(), skipped))
}

/// What storing one verified PDB comes to.
enum Plan {
    /// A new key: claim it and move the file in.
    Claim,
    /// This version's own claim, whose bytes are missing (a push that failed
    /// part-way before this code existed, or a lost file): put them back.
    Restore,
    /// The same bytes are already stored: nothing to do.
    Unchanged,
}

/// Claim and store every verified PDB, or none. Returns the keys this call
/// newly claimed (for [`release`] should a later step fail).
async fn store_all(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    id: &str,
    version: &NuGetVersion,
    verified: &[Verified],
) -> Result<Vec<(String, String)>> {
    // Every key this push touches is locked for the whole decision, in one
    // global order (the list is sorted), so two pushes sharing keys cannot
    // deadlock and none can claim a key between our check and our write.
    let mut _key_guards = Vec::with_capacity(verified.len());
    for v in verified {
        _key_guards.push(locks::lock_symbol(&v.key, &v.filename).await);
    }

    // Decide everything first; nothing is written unless every PDB is fine.
    let lower_id = id.to_lowercase();
    let normalized = version.normalized();
    let mut plans = Vec::with_capacity(verified.len());
    for v in verified {
        let plan = match db.find_symbol(&v.key, &v.filename).await? {
            None => Plan::Claim,
            Some(owner) => {
                let refuse = |why: &str| {
                    Error::InvalidPackage(format!(
                        "symbol {} ({}) {why}; a symbol package may not replace symbols that \
                         are already published",
                        v.filename, v.key
                    ))
                };
                if owner.lower_id != lower_id {
                    return Err(refuse(&format!("is already owned by {}", owner.lower_id)));
                }
                match stored_sha256(storage, &v.key, &v.filename).await? {
                    Some(stored) if stored == v.sha256 => Plan::Unchanged,
                    Some(_) => return Err(refuse("is already stored with different bytes")),
                    None if owner.normalized_version == normalized => Plan::Restore,
                    None => {
                        return Err(refuse(&format!(
                            "is already claimed by version {}",
                            owner.normalized_version
                        )))
                    }
                }
            }
        };
        plans.push(plan);
    }

    // Claim before writing: the row is what makes the bytes servable, and it
    // is only ever created here, under the key's lock.
    let mut claimed: Vec<(String, String)> = Vec::new();
    for (v, plan) in verified.iter().zip(plans) {
        let step = async {
            match plan {
                Plan::Unchanged => Ok(()),
                Plan::Claim => {
                    if !db.add_symbol(&v.key, &v.filename, id, version).await? {
                        return Err(Error::Conflict(format!(
                            "symbol {} ({}) was claimed concurrently",
                            v.filename, v.key
                        )));
                    }
                    claimed.push((v.key.clone(), v.filename.clone()));
                    storage
                        .store_symbol_file(&v.key, &v.filename, v.path.clone())
                        .await
                }
                Plan::Restore => {
                    storage
                        .store_symbol_file(&v.key, &v.filename, v.path.clone())
                        .await
                }
            }
        }
        .await;
        if let Err(e) = step {
            release(storage, db, &claimed).await;
            return Err(e);
        }
    }
    Ok(claimed)
}

/// Undo claims made by a push that failed later on: the rows, and any bytes
/// already moved in under them. Best-effort; the push's error is what the
/// caller reports.
async fn release(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    claimed: &[(String, String)],
) {
    for (key, filename) in claimed {
        if let Err(e) = db.delete_symbol(key, filename).await {
            tracing::warn!(%key, %filename, error = %e, "could not release a symbol claim");
        }
        let _ = storage.delete_symbol(key, filename).await;
    }
}

/// SHA-256 of the bytes stored under a key, or `None` when there are none.
async fn stored_sha256(
    storage: &dyn PackageStorage,
    key: &str,
    filename: &str,
) -> Result<Option<[u8; 32]>> {
    let path = match storage.get_symbol(key, filename).await {
        Ok(PackageContent::LocalPath(path)) => path,
        Err(Error::PackageNotFound) => return Ok(None),
        Err(e) => return Err(e),
    };
    tokio::task::spawn_blocking(move || {
        let mut file = File::open(path)?;
        sha256_file(&mut file).map(Some)
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("symbol hash task panicked: {e}")))?
}

fn sha256_file(file: &mut File) -> Result<[u8; 32]> {
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    std::io::copy(file, &mut hasher)?;
    Ok(hasher.finalize().into())
}

/// Extracted PDBs awaiting a decision. Whatever was not moved into the store
/// is removed when this goes out of scope, on every path out of the push.
struct TempFiles(Vec<nupkg::ExtractedEntry>);

impl Drop for TempFiles {
    fn drop(&mut self) {
        for entry in &self.0 {
            // Gone already when it was moved into the store.
            let _ = std::fs::remove_file(&entry.path);
        }
    }
}

/// The final path segment (the bare file name), lower-cased.
fn file_name(entry: &str) -> String {
    entry
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(entry)
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::SqliteDatabase;
    use crate::storage::FilesystemStorage;
    use crate::version::NuGetVersion;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    #[test]
    fn file_name_strips_directories() {
        assert_eq!(file_name("lib/net8.0/app.pdb"), "app.pdb");
        assert_eq!(file_name("App.PDB"), "app.pdb");
        assert_eq!(file_name("a\\b\\c.pdb"), "c.pdb");
    }

    /// A minimal Portable PDB whose `#Pdb` stream begins with the id
    /// (`guid`, `stamp`), followed by `body` standing in for the tables.
    fn portable_pdb(guid: &[u8; 16], stamp: u32, body: &[u8]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&0x424A_5342u32.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        let version = b"PDB v1.0\0\0\0\0";
        buf.extend_from_slice(&(version.len() as u32).to_le_bytes());
        buf.extend_from_slice(version);
        buf.extend_from_slice(&0u16.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        let hp = buf.len();
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(&20u32.to_le_bytes());
        buf.extend_from_slice(b"#Pdb\0\0\0\0");
        let off = buf.len() as u32;
        buf[hp..hp + 4].copy_from_slice(&off.to_le_bytes());
        buf.extend_from_slice(guid);
        buf.extend_from_slice(&stamp.to_le_bytes());
        buf.extend_from_slice(body);
        buf
    }

    /// The SHA-256 `PdbChecksum` an assembly built with `pdb` records.
    fn checksum_of(pdb: &[u8]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(pdb);
        let id = pdb::read_pdb_id(&mut cursor).unwrap().unwrap();
        pdb::pdb_checksum(&mut cursor, &id, "SHA256")
            .unwrap()
            .unwrap()
    }

    /// The assembly matching `pdb`: its CodeView entry carries the PDB's id
    /// and its checksum entry the PDB's hash.
    fn assembly_for(pdb: &[u8]) -> Vec<u8> {
        let id = pdb::read_pdb_id(&mut std::io::Cursor::new(pdb))
            .unwrap()
            .unwrap();
        pe::test_image(&id.guid, id.stamp, Some(("SHA256", &checksum_of(pdb))))
    }

    fn zip_file(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
    }

    fn nuspec(id: &str) -> Vec<u8> {
        format!(
            "<package><metadata><id>{id}</id><version>1.0.0</version>\
             <authors>A</authors><description>d</description></metadata></package>"
        )
        .into_bytes()
    }

    /// A `.snupkg` for `id` 1.0.0 holding `pdbs`, as `(entry name, bytes)`.
    fn snupkg(id: &str, pdbs: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pkg.snupkg");
        let manifest = nuspec(id);
        let mut entries: Vec<(&str, &[u8])> = vec![("Pkg.nuspec", &manifest)];
        entries.extend_from_slice(pdbs);
        zip_file(&path, &entries);
        (dir, path)
    }

    async fn fixtures() -> (tempfile::TempDir, FilesystemStorage, SqliteDatabase) {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        (dir, storage, db)
    }

    /// Publish `id` 1.0.0 into the feed, its stored `.nupkg` holding
    /// `assemblies` as `(entry name, bytes)`.
    async fn publish(
        storage: &FilesystemStorage,
        db: &SqliteDatabase,
        id: &str,
        assemblies: &[(&str, &[u8])],
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pkg.nupkg");
        let manifest = nuspec(id);
        let mut entries: Vec<(&str, &[u8])> = vec![("Pkg.nuspec", &manifest)];
        entries.extend_from_slice(assemblies);
        zip_file(&path, &entries);
        storage.store_package(id, "1.0.0", path).await.unwrap();
        db.add_to_feed(FEED, &owning_package(id)).await.unwrap();
    }

    fn owning_package(id: &str) -> crate::models::Package {
        crate::models::Package {
            id: id.into(),
            version: NuGetVersion::parse("1.0.0").unwrap(),
            listed: true,
            enabled: true,
            authors: vec![],
            description: String::new(),
            icon_url: None,
            license_url: None,
            license_expression: None,
            project_url: None,
            repository_url: None,
            repository_type: None,
            min_client_version: None,
            release_notes: None,
            language: None,
            title: None,
            summary: None,
            tags: vec![],
            has_readme: false,
            has_embedded_icon: false,
            is_development_dependency: false,
            require_license_acceptance: false,
            is_semver2: false,
            package_size: 1,
            package_hash: "h".into(),
            package_hash_algorithm: "SHA512".into(),
            published: chrono::Utc::now(),
            downloads: 0,
            package_types: vec![],
            dependencies: vec![],
        }
    }

    const FEED: &str = "default";

    fn refusal(result: Result<SymbolResult>) -> String {
        match result {
            Err(Error::InvalidPackage(m)) => m,
            other => panic!("expected the push to be refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn indexes_portable_skips_native() {
        let (_d, storage, db) = fixtures().await;
        let pdb = portable_pdb(&[7u8; 16], 0x1234, b"tables");
        publish(
            &storage,
            &db,
            "Sym.Lib",
            &[("lib/net8.0/Sym.Lib.dll", &assembly_for(&pdb))],
        )
        .await;

        let (_sd, path) = snupkg(
            "Sym.Lib",
            &[
                ("lib/net8.0/Sym.Lib.pdb", &pdb),
                ("lib/net8.0/native.pdb", b"Microsoft C/C++ MSF 7.00\r\n"),
            ],
        );
        let result = index_symbol_package(&storage, &db, FEED, path)
            .await
            .unwrap();
        assert_eq!(result.indexed, 1);
        assert_eq!(result.skipped, 1);

        // The portable PDB is resolvable by its SSQP key, byte for byte.
        let key = pdb::portable_pdb_signature(&pdb).unwrap();
        assert!(db.find_symbol(&key, "sym.lib.pdb").await.unwrap().is_some());
        let PackageContent::LocalPath(stored) =
            storage.get_symbol(&key, "sym.lib.pdb").await.unwrap();
        assert_eq!(std::fs::read(stored).unwrap(), pdb);

        // Pushing the same symbols again is a no-op, not a conflict.
        let (_sd, path) = snupkg("Sym.Lib", &[("lib/net8.0/Sym.Lib.pdb", &pdb)]);
        let again = index_symbol_package(&storage, &db, FEED, path)
            .await
            .unwrap();
        assert_eq!(again.indexed, 1);
    }

    #[tokio::test]
    async fn rejects_symbols_for_unknown_package() {
        let (_d, storage, db) = fixtures().await;
        // No package added first.
        let pdb = portable_pdb(&[7u8; 16], 1, b"");
        let (_sd, path) = snupkg("Sym.Lib", &[("lib/net8.0/sym.lib.pdb", &pdb)]);
        let err = index_symbol_package(&storage, &db, FEED, path.clone())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PackageNotFound));
        // The rejected temp file was cleaned up.
        assert!(!path.exists());
    }

    /// A PDB id is public — it is written into the assembly anyone can
    /// download — so a PDB is only accepted when the owning package's own
    /// assembly vouches for it, down to its checksum.
    #[tokio::test]
    async fn a_pdb_must_belong_to_an_assembly_of_the_package() {
        let (_d, storage, db) = fixtures().await;
        let real = portable_pdb(&[7u8; 16], 42, b"the real tables");
        let assembly = assembly_for(&real);
        publish(
            &storage,
            &db,
            "Sym.Lib",
            &[("lib/net8.0/Sym.Lib.dll", &assembly)],
        )
        .await;
        let push = |pdbs: Vec<(&'static str, Vec<u8>)>| {
            let pdbs: Vec<(&str, &[u8])> = pdbs.iter().map(|(n, b)| (*n, b.as_slice())).collect();
            snupkg("Sym.Lib", &pdbs)
        };

        // No assembly beside it.
        let (_sd, path) = push(vec![("lib/net6.0/Sym.Lib.pdb", real.clone())]);
        let m = refusal(index_symbol_package(&storage, &db, FEED, path).await);
        assert!(m.contains("has no lib/net6.0/sym.lib.dll"), "{m}");

        // Another build: the id does not match the CodeView entry.
        let other = portable_pdb(&[8u8; 16], 42, b"the real tables");
        let (_sd, path) = push(vec![("lib/net8.0/Sym.Lib.pdb", other)]);
        let m = refusal(index_symbol_package(&storage, &db, FEED, path).await);
        assert!(m.contains("matches no CodeView entry"), "{m}");

        // A forgery: the right id, copied out of the assembly, over different
        // content. Only the checksum tells it apart.
        let forged = portable_pdb(&[7u8; 16], 42, b"evil source link");
        let (_sd, path) = push(vec![("lib/net8.0/Sym.Lib.pdb", forged)]);
        let m = refusal(index_symbol_package(&storage, &db, FEED, path).await);
        assert!(m.contains("checksum does not match"), "{m}");

        assert!(db
            .find_symbols("Sym.Lib", &NuGetVersion::parse("1.0.0").unwrap())
            .await
            .unwrap()
            .is_empty());
    }

    /// Without a checksum entry nothing binds the PDB's content to the
    /// assembly, so — as on nuget.org — such symbols are refused.
    #[tokio::test]
    async fn an_assembly_without_a_pdb_checksum_cannot_vouch_for_a_pdb() {
        let (_d, storage, db) = fixtures().await;
        let pdb = portable_pdb(&[7u8; 16], 42, b"tables");
        let old_compiler = pe::test_image(&[7u8; 16], 42, None);
        publish(
            &storage,
            &db,
            "Sym.Lib",
            &[("lib/Sym.Lib.dll", &old_compiler)],
        )
        .await;
        let (_sd, path) = snupkg("Sym.Lib", &[("lib/Sym.Lib.pdb", &pdb)]);
        let m = refusal(index_symbol_package(&storage, &db, FEED, path).await);
        assert!(m.contains("no PDB checksum"), "{m}");
    }

    /// One bad PDB refuses the whole push; the good ones before it used to be
    /// stored regardless.
    #[tokio::test]
    async fn a_push_is_all_or_nothing() {
        let (_d, storage, db) = fixtures().await;
        let good = portable_pdb(&[1u8; 16], 1, b"a");
        let bad = portable_pdb(&[2u8; 16], 2, b"b");
        publish(
            &storage,
            &db,
            "Sym.Lib",
            &[
                ("lib/A.dll", &assembly_for(&good)),
                (
                    "lib/B.dll",
                    &pe::test_image(&[2u8; 16], 2, Some(("SHA256", &[0u8; 32]))),
                ),
            ],
        )
        .await;
        let (sd, path) = snupkg("Sym.Lib", &[("lib/A.pdb", &good), ("lib/B.pdb", &bad)]);
        refusal(index_symbol_package(&storage, &db, FEED, path).await);

        let key = pdb::portable_pdb_signature(&good).unwrap();
        assert!(db.find_symbol(&key, "a.pdb").await.unwrap().is_none());
        assert!(storage.get_symbol(&key, "a.pdb").await.is_err());
        // Nothing extracted is left behind next to the upload either.
        assert_eq!(std::fs::read_dir(sd.path()).unwrap().count(), 0);
    }

    /// Bytes already stored under a key are never replaced by different ones.
    #[tokio::test]
    async fn stored_symbols_are_never_replaced_with_different_bytes() {
        let (_d, storage, db) = fixtures().await;
        let pdb = portable_pdb(&[7u8; 16], 42, b"tables");
        publish(
            &storage,
            &db,
            "Sym.Lib",
            &[("lib/Sym.Lib.dll", &assembly_for(&pdb))],
        )
        .await;
        let key = pdb::portable_pdb_signature(&pdb).unwrap();
        let version = NuGetVersion::parse("1.0.0").unwrap();
        // Something else is already there under this version's own claim.
        assert!(db
            .add_symbol(&key, "sym.lib.pdb", "Sym.Lib", &version)
            .await
            .unwrap());
        storage
            .store_symbol(&key, "sym.lib.pdb", b"different")
            .await
            .unwrap();

        let (_sd, path) = snupkg("Sym.Lib", &[("lib/Sym.Lib.pdb", &pdb)]);
        let m = refusal(index_symbol_package(&storage, &db, FEED, path).await);
        assert!(m.contains("different bytes"), "{m}");
        let PackageContent::LocalPath(stored) =
            storage.get_symbol(&key, "sym.lib.pdb").await.unwrap();
        assert_eq!(std::fs::read(stored).unwrap(), b"different");

        // A claim without bytes (a push that died part-way) is completed.
        storage.delete_symbol(&key, "sym.lib.pdb").await.unwrap();
        let (_sd, path) = snupkg("Sym.Lib", &[("lib/Sym.Lib.pdb", &pdb)]);
        index_symbol_package(&storage, &db, FEED, path)
            .await
            .unwrap();
        let PackageContent::LocalPath(stored) =
            storage.get_symbol(&key, "sym.lib.pdb").await.unwrap();
        assert_eq!(std::fs::read(stored).unwrap(), pdb);
    }

    /// Two packages carrying the same assembly race for its key: exactly one
    /// claims it, and the row and the bytes agree about which.
    #[tokio::test]
    async fn concurrent_claims_for_one_key_have_one_winner() {
        let (_d, storage, db) = fixtures().await;
        let pdb = portable_pdb(&[7u8; 16], 42, b"tables");
        let assembly = assembly_for(&pdb);
        publish(&storage, &db, "First.Lib", &[("lib/Shared.dll", &assembly)]).await;
        publish(
            &storage,
            &db,
            "Second.Lib",
            &[("lib/Shared.dll", &assembly)],
        )
        .await;

        let (_a, first) = snupkg("First.Lib", &[("lib/Shared.pdb", &pdb)]);
        let (_b, second) = snupkg("Second.Lib", &[("lib/Shared.pdb", &pdb)]);
        let (r1, r2) = tokio::join!(
            index_symbol_package(&storage, &db, FEED, first),
            index_symbol_package(&storage, &db, FEED, second),
        );
        assert_eq!(
            [r1.is_ok(), r2.is_ok()].iter().filter(|ok| **ok).count(),
            1,
            "{r1:?} / {r2:?}"
        );
        let winner = if r1.is_ok() {
            "first.lib"
        } else {
            "second.lib"
        };
        let key = pdb::portable_pdb_signature(&pdb).unwrap();
        let owner = db.find_symbol(&key, "shared.pdb").await.unwrap().unwrap();
        assert_eq!(owner.lower_id, winner);
    }
}
