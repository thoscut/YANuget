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
//!    moved to another package. It moves to another version of the same
//!    package in one case only: the version holding it is deleted, and the
//!    other one's own symbol package holds the same bytes, which its own
//!    assemblies vouch for (`Successors`). A re-push of an identical PDB by
//!    a second version records nothing, so without that the second version
//!    would lose its symbols with the first.
//! 3. **Stored bytes are never replaced by different ones.** A re-push of the
//!    same PDB is a no-op; anything else under an existing key is refused.
//!
//! The push also holds the owning version's lock (see [`crate::locks`]) so it
//! cannot interleave with a purge or an overwrite of that version, and it is
//! all-or-nothing: every PDB is validated before any is stored, and a failure
//! while storing undoes the claims this push made.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::sync::OwnedMutexGuard;

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
    let version_guard = locks::lock_version(&id, &normalized).await;
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
    let (claimed, key_guards) = store_all(storage, db, &id, &version, &verified).await?;

    // 7. Store the whole .snupkg alongside the package (moves the temp file).
    if let Err(e) = storage
        .store_symbol_package(&id, &normalized, temp_path.clone())
        .await
    {
        release(storage, db, &claimed).await;
        return Err(e);
    }

    // The keys stay locked until the symbol package is in place. A key this
    // push found `Unchanged` still belongs to another version, and a purge of
    // that one looks for a version whose symbol package holds the same PDB to
    // hand the key to (`Successors`): released any earlier, the purge could
    // take the key in between, find no symbol package here and delete the
    // PDB this push was just told it has. The version lock goes first, since
    // that purge passes over a version that is locked rather than wait for it.
    drop(version_guard);
    drop(key_guards);

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
    // Built once: looking each assembly up by scanning the directory made the
    // check quadratic in the entry count.
    let indices = nupkg::entry_indices(&archive);
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
            .find_map(|ext| indices.get(&format!("{stem}{ext}")).copied())
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
/// newly claimed (for [`release`] should a later step fail), and the locks on
/// every key it touched, for the caller to hold until the push is complete.
async fn store_all(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    id: &str,
    version: &NuGetVersion,
    verified: &[Verified],
) -> Result<(Vec<(String, String)>, Vec<OwnedMutexGuard<()>>)> {
    // Every key this push touches is locked for the whole decision, in one
    // global order (the list is sorted), so two pushes sharing keys cannot
    // deadlock and none can claim a key between our check and our write.
    let mut key_guards = Vec::with_capacity(verified.len());
    for v in verified {
        key_guards.push(locks::lock_symbol(&v.key, &v.filename).await);
    }

    // Decide everything first; nothing is written unless every PDB is fine.
    let lower_id = crate::database::canonical_id(id);
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
    Ok((claimed, key_guards))
}

/// Finds, for the symbols of a version being deleted, another version of the
/// same package to hand each one to: one whose stored symbol package holds a
/// PDB with exactly the bytes stored under the key, and whose own stored
/// package has the assembly that vouches for it, as a push of it would have
/// checked. The newest such version wins.
///
/// "Build once, pack twice" (`1.0.0-rc.1` and `1.0.0` from the same
/// assemblies) pushes the same PDB for both. The second push finds the bytes
/// in place and changes nothing, so the key stays with the first version;
/// retention removing that one would otherwise take the second one's symbols
/// with it, silently.
///
/// The search is built for its usual answer, which is "none": every build
/// writes PDBs with new ids, while every version of `Foo` carries a `Foo.pdb`.
/// A candidate entry is only decompressed when the central directory already
/// says it has the stored PDB's name, length and CRC-32, so a purge with no
/// successor reads each other symbol package's directory once (they are kept
/// for every key of the purge) and decompresses nothing. Entries are hashed
/// as they stream past, never written out: the bytes are only of interest if
/// they are the stored ones, and those are already on disk to check against.
///
/// One per purge. The caller holds `gone`'s version lock, and the key's lock
/// around each [`Self::successor`].
pub(crate) struct Successors<'a> {
    storage: &'a dyn PackageStorage,
    db: &'a dyn PackageDatabase,
    id: &'a str,
    gone: &'a NuGetVersion,
    /// Every other stored version, newest first; read on first use.
    versions: Option<Vec<NuGetVersion>>,
    /// What each version's symbol package lists, by normalized version.
    listings: HashMap<String, Arc<Listing>>,
    work: Work,
}

/// What a search has cost so far, for the log and for tests.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Work {
    /// Symbol packages whose central directory was read.
    pub listed: usize,
    /// Entries decompressed, which only a likely match is.
    pub decompressed: usize,
}

/// A version picked to take a symbol over, still locked: the claim is made
/// for it, so it must stay what it was checked as until the claim is
/// recorded, as on a push of its own. Drop it after the hand-over.
pub(crate) struct Heir {
    pub version: NuGetVersion,
    _guard: OwnedMutexGuard<()>,
}

/// The PDB entries of one stored symbol package, from its central directory.
struct Listing {
    /// Length and modification time of the file when it was read. A symbol
    /// push replacing it while the purge runs makes them differ, and it is
    /// read again.
    stamp: (u64, Option<std::time::SystemTime>),
    package: PathBuf,
    snupkg: PathBuf,
    pdbs: Vec<PdbEntry>,
}

struct PdbEntry {
    /// As the archive spells it.
    name: String,
    /// [`file_name`] of it.
    filename: String,
    size: u64,
    crc32: u32,
}

impl<'a> Successors<'a> {
    pub(crate) fn new(
        storage: &'a dyn PackageStorage,
        db: &'a dyn PackageDatabase,
        id: &'a str,
        gone: &'a NuGetVersion,
    ) -> Self {
        Self {
            storage,
            db,
            id,
            gone,
            versions: None,
            listings: HashMap::new(),
            work: Work::default(),
        }
    }

    pub(crate) fn work(&self) -> Work {
        self.work
    }

    /// The version `gone`'s symbol `key`/`filename` goes to, or `None`.
    ///
    /// A candidate whose version lock is taken is passed over: it is being
    /// deleted, overwritten or given symbols right now, and none of those
    /// leaves it a safe place for a claim checked against what it was. It is
    /// only tried, never waited for — a symbol push holds its version's lock
    /// while it waits for the key's, which the caller holds.
    pub(crate) async fn successor(&mut self, key: &str, filename: &str) -> Result<Option<Heir>> {
        let stored = match self.storage.get_symbol(key, filename).await {
            Ok(PackageContent::LocalPath(path)) => path,
            Err(Error::PackageNotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        let size = match tokio::fs::metadata(&stored).await {
            Ok(meta) => meta.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let filename = filename.to_ascii_lowercase();
        let versions = match &self.versions {
            Some(versions) => versions.clone(),
            None => {
                let mut versions = self.db.stored_versions(self.id).await?;
                versions.retain(|v| v != self.gone);
                versions.reverse();
                self.versions.insert(versions).clone()
            }
        };
        // Hashed once a candidate gets past the name and length.
        let mut fingerprint: Option<(u32, [u8; 32])> = None;
        for version in versions {
            let Some(listing) = self.listing(&version).await else {
                continue;
            };
            if !listing
                .pdbs
                .iter()
                .any(|e| e.filename == filename && e.size == size)
            {
                continue;
            }
            let (crc32, sha256) = match fingerprint {
                Some(known) => known,
                None => *fingerprint.insert(fingerprint_file(&stored).await?),
            };
            let names: Vec<String> = listing
                .pdbs
                .iter()
                .filter(|e| e.filename == filename && e.size == size && e.crc32 == crc32)
                .map(|e| e.name.clone())
                .collect();
            if names.is_empty() {
                continue;
            }

            let normalized = version.normalized();
            let Some(guard) = locks::try_lock_version(self.id, &normalized) else {
                tracing::warn!(id = %self.id, version = %normalized, %key, "not handing a symbol over to a version that is being changed");
                continue;
            };
            if !self.db.package_data_exists(self.id, &version).await? {
                continue;
            }
            let (package, snupkg) = (listing.package.clone(), listing.snupkg.clone());
            let (stored, key_owned, filename_owned) =
                (stored.clone(), key.to_owned(), filename.clone());
            let (outcome, decompressed) = tokio::task::spawn_blocking(move || {
                let mut decompressed = 0;
                let outcome = holds_pdb(
                    &package,
                    &snupkg,
                    &names,
                    &stored,
                    (&key_owned, &filename_owned),
                    (size, &sha256),
                    &mut decompressed,
                );
                (outcome, decompressed)
            })
            .await
            .map_err(|e| Error::Other(anyhow::anyhow!("symbol verification task panicked: {e}")))?;
            self.work.decompressed += decompressed;
            match outcome {
                Ok(true) => {
                    return Ok(Some(Heir {
                        version,
                        _guard: guard,
                    }))
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(id = %self.id, version = %normalized, %key, error = %e, "could not read a symbol package to hand a symbol over to");
                }
            }
        }
        Ok(None)
    }

    /// The PDB entries of `version`'s stored symbol package, or `None` when
    /// it has none (a version without symbols of its own is no one to hand
    /// them to). Read once per purge unless the file changes.
    async fn listing(&mut self, version: &NuGetVersion) -> Option<Arc<Listing>> {
        let normalized = version.normalized();
        let package = match self.storage.get_package(self.id, &normalized).await {
            Ok(PackageContent::LocalPath(path)) => path,
            Err(_) => return None,
        };
        // The store keeps a version's symbol package beside its package.
        let snupkg = package.with_extension("snupkg");
        let Ok(meta) = tokio::fs::metadata(&snupkg).await else {
            self.listings.remove(&normalized);
            return None;
        };
        let stamp = (meta.len(), meta.modified().ok());
        if let Some(listing) = self.listings.get(&normalized) {
            if listing.stamp == stamp {
                return Some(listing.clone());
            }
        }
        self.work.listed += 1;
        let path = snupkg.clone();
        let pdbs = match tokio::task::spawn_blocking(move || list_pdbs(&path)).await {
            Ok(Ok(pdbs)) => pdbs,
            Ok(Err(e)) => {
                tracing::warn!(id = %self.id, version = %normalized, error = %e, "could not read a symbol package to hand a symbol over to");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!(id = %self.id, version = %normalized, error = %e, "symbol package listing task panicked");
                Vec::new()
            }
        };
        let listing = Arc::new(Listing {
            stamp,
            package,
            snupkg,
            pdbs,
        });
        self.listings.insert(normalized, listing.clone());
        Some(listing)
    }
}

/// The `.pdb` entries of a symbol package with their declared length and
/// CRC-32, from the central directory alone (blocking).
fn list_pdbs(snupkg: &Path) -> Result<Vec<PdbEntry>> {
    let archive = nupkg::open_archive(snupkg)?;
    let mut pdbs = Vec::new();
    for i in 0..archive.len() {
        let Some(name) = nupkg::entry_name(&archive, i)? else {
            continue;
        };
        if !nupkg::normalize_entry(&name).ends_with(".pdb") {
            continue;
        }
        let entry = archive
            .by_index_data(i)
            .map_err(|e| Error::InvalidPackage(format!("could not read the entry {name}: {e}")))?;
        pdbs.push(PdbEntry {
            filename: file_name(&name),
            size: entry.size(),
            crc32: entry.crc32(),
            name,
        });
    }
    Ok(pdbs)
}

/// Whether one of the entries `names` of the symbol package at `snupkg` is
/// byte for byte the PDB stored at `stored` (`size` bytes, SHA-256 `sha256`)
/// under `key`/`filename`, and an assembly in `package` vouches for it
/// (blocking). Counts the entries it decompresses into `decompressed`.
fn holds_pdb(
    package: &Path,
    snupkg: &Path,
    names: &[String],
    stored: &Path,
    (key, filename): (&str, &str),
    (size, sha256): (u64, &[u8; 32]),
    decompressed: &mut usize,
) -> Result<bool> {
    use std::io::Read;

    let mut archive = nupkg::open_archive(snupkg)?;
    let indices = nupkg::entry_indices(&archive);
    for name in names {
        // Looked up again: the file may have been replaced since the listing.
        let Some(&index) = indices.get(&nupkg::normalize_entry(name)) else {
            continue;
        };
        let mut entry = archive
            .by_index(index)
            .map_err(|e| Error::InvalidPackage(format!("could not open {name}: {e}")))?;
        if entry.size() != size {
            continue;
        }
        *decompressed += 1;
        let mut hasher = Sha256::new();
        let read = crate::streaming::hash_reader(&mut (&mut entry).take(size + 1), &mut hasher)
            .map_err(|e| Error::InvalidPackage(format!("could not read {name}: {e}")))?;
        if read != size || <[u8; 32]>::from(hasher.finalize()) != *sha256 {
            continue;
        }
        // These are the stored bytes, so the stored file is what the
        // candidate's assembly has to vouch for.
        let (verified, _) = match verify(package, &[(name.clone(), stored.to_path_buf())]) {
            Ok(verified) => verified,
            Err(e) => {
                tracing::warn!(pdb = %name, error = %e, "a symbol package's PDB is not vouched for by its package");
                continue;
            }
        };
        if verified
            .iter()
            .any(|v| v.key.eq_ignore_ascii_case(key) && v.filename == filename)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// CRC-32 and SHA-256 of a stored file, in one pass.
async fn fingerprint_file(path: &Path) -> Result<(u32, [u8; 32])> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut file = File::open(path)?;
        let mut crc = Crc32::new();
        let mut sha = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            };
            crc.update(&buf[..n]);
            sha.update(&buf[..n]);
        }
        Ok((crc.finish(), sha.finalize().into()))
    })
    .await
    .map_err(|e| Error::Other(anyhow::anyhow!("symbol hash task panicked: {e}")))?
}

/// The CRC-32 a ZIP central directory records for each entry (IEEE 802.3,
/// reflected), to hold a stored file against an entry without decompressing
/// it. Bytewise from a table: it only runs over a PDB some candidate entry
/// already matches in name and length.
struct Crc32(u32);

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut bit = 0;
        while bit < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            bit += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

impl Crc32 {
    fn new() -> Self {
        Self(!0)
    }

    fn update(&mut self, bytes: &[u8]) {
        let mut c = self.0;
        for &b in bytes {
            c = CRC32_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    fn finish(&self) -> u32 {
        !self.0
    }
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
    crate::streaming::hash_reader(file, &mut hasher)?;
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

    fn nuspec(id: &str, version: &str) -> Vec<u8> {
        format!(
            "<package><metadata><id>{id}</id><version>{version}</version>\
             <authors>A</authors><description>d</description></metadata></package>"
        )
        .into_bytes()
    }

    /// A `.snupkg` for `id` 1.0.0 holding `pdbs`, as `(entry name, bytes)`.
    fn snupkg(id: &str, pdbs: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
        snupkg_at(id, "1.0.0", pdbs)
    }

    /// A `.snupkg` for `id` `version` holding `pdbs`.
    fn snupkg_at(id: &str, version: &str, pdbs: &[(&str, &[u8])]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pkg.snupkg");
        let manifest = nuspec(id, version);
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
        publish_at(storage, db, id, "1.0.0", assemblies).await;
    }

    /// Publish `id` `version` into the feed, as [`publish`] does 1.0.0.
    async fn publish_at(
        storage: &FilesystemStorage,
        db: &SqliteDatabase,
        id: &str,
        version: &str,
        assemblies: &[(&str, &[u8])],
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pkg.nupkg");
        let manifest = nuspec(id, version);
        let mut entries: Vec<(&str, &[u8])> = vec![("Pkg.nuspec", &manifest)];
        entries.extend_from_slice(assemblies);
        zip_file(&path, &entries);
        let version = NuGetVersion::parse(version).unwrap();
        storage
            .store_package(id, &version.normalized(), path)
            .await
            .unwrap();
        db.add_to_feed(FEED, &owning_package(id, &version))
            .await
            .unwrap();
    }

    fn owning_package(id: &str, version: &NuGetVersion) -> crate::models::Package {
        crate::models::Package {
            id: id.into(),
            version: version.clone(),
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

    #[test]
    fn crc32_is_the_one_a_zip_records() {
        let mut crc = Crc32::new();
        crc.update(b"1234");
        crc.update(b"56789");
        assert_eq!(crc.finish(), 0xCBF4_3926);

        let pdb = portable_pdb(&[3u8; 16], 3, &[0xA5; 70_000]);
        let (_sd, path) = snupkg("Sym.Lib", &[("lib/net8.0/Sym.Lib.pdb", &pdb)]);
        let listed = list_pdbs(&path).unwrap();
        let mut crc = Crc32::new();
        crc.update(&pdb);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].filename, "sym.lib.pdb");
        assert_eq!(listed[0].size, pdb.len() as u64);
        assert_eq!(listed[0].crc32, crc.finish());
    }

    /// Two PDBs per version, `A.pdb` and `B.pdb`, as every build of a package
    /// has them: same names, same lengths, new ids (`seed`).
    fn build(seed: u8) -> [Vec<u8>; 2] {
        [
            portable_pdb(&[seed; 16], u32::from(seed), b"tables of A"),
            portable_pdb(&[seed ^ 0x80; 16], u32::from(seed), b"tables of B"),
        ]
    }

    async fn publish_build(
        storage: &FilesystemStorage,
        db: &SqliteDatabase,
        id: &str,
        version: &str,
        [a, b]: &[Vec<u8>; 2],
    ) {
        publish_at(
            storage,
            db,
            id,
            version,
            &[
                ("lib/net8.0/A.dll", &assembly_for(a)),
                ("lib/net8.0/B.dll", &assembly_for(b)),
            ],
        )
        .await;
    }

    /// `PERF-22`: the usual purge has no successor, and finding that out
    /// read, extracted and verified every same-named PDB of every other
    /// version, once per key. The central directory now rules them out,
    /// and each symbol package is listed once for all keys.
    #[tokio::test]
    async fn a_purge_without_a_matching_pdb_decompresses_nothing() {
        const ID: &str = "Many.Lib";
        let (_d, storage, db) = fixtures().await;
        let first = build(21);
        for (version, pdbs) in [
            ("1.0.0", &first),
            ("2.0.0", &build(22)),
            ("3.0.0", &build(23)),
        ] {
            publish_build(&storage, &db, ID, version, pdbs).await;
            let (_sd, path) = snupkg_at(
                ID,
                version,
                &[
                    ("lib/net8.0/A.pdb", &pdbs[0]),
                    ("lib/net8.0/B.pdb", &pdbs[1]),
                ],
            );
            index_symbol_package(&storage, &db, FEED, path)
                .await
                .unwrap();
        }
        let gone = NuGetVersion::parse("1.0.0").unwrap();
        let symbols = db.find_symbols(ID, &gone).await.unwrap();
        assert_eq!(symbols.len(), 2);

        let mut successors = Successors::new(&storage, &db, ID, &gone);
        for sym in &symbols {
            let heir = successors.successor(&sym.key, &sym.filename).await.unwrap();
            assert!(heir.is_none(), "{}", sym.filename);
        }
        assert_eq!(
            successors.work(),
            Work {
                listed: 2,
                decompressed: 0
            }
        );

        // A version that does hold one of them is found, at the cost of
        // decompressing that one entry.
        publish_at(
            &storage,
            &db,
            ID,
            "4.0.0",
            &[("lib/net8.0/A.dll", &assembly_for(&first[0]))],
        )
        .await;
        let (_sd, path) = snupkg_at(ID, "4.0.0", &[("lib/net8.0/A.pdb", &first[0])]);
        index_symbol_package(&storage, &db, FEED, path)
            .await
            .unwrap();
        let mut successors = Successors::new(&storage, &db, ID, &gone);
        let key_a = pdb::portable_pdb_signature(&first[0]).unwrap();
        let key_b = pdb::portable_pdb_signature(&first[1]).unwrap();
        let heir = successors.successor(&key_a, "a.pdb").await.unwrap();
        assert_eq!(heir.unwrap().version.normalized(), "4.0.0");
        assert!(successors
            .successor(&key_b, "b.pdb")
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            successors.work(),
            Work {
                listed: 3,
                decompressed: 1
            }
        );
        // Nothing was written anywhere to find that out.
        let mut stray = Vec::new();
        let mut dirs = vec![_d.path().to_path_buf()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "tmp") {
                    stray.push(path);
                }
            }
        }
        assert!(stray.is_empty(), "{stray:?}");
    }

    /// Two versions of `Sym.Lib`, `1.0.0-rc.1` and `1.0.0`, from one build.
    /// The first one's symbols are pushed; returns the PDB and its key.
    async fn built_twice(
        storage: &FilesystemStorage,
        db: &SqliteDatabase,
        id: &str,
        seed: u8,
    ) -> (Vec<u8>, String) {
        let pdb = portable_pdb(&[seed; 16], u32::from(seed), b"shared tables");
        for version in ["1.0.0-rc.1", "1.0.0"] {
            publish_at(
                storage,
                db,
                id,
                version,
                &[("lib/Sym.Lib.dll", &assembly_for(&pdb))],
            )
            .await;
        }
        let (_sd, path) = snupkg_at(id, "1.0.0-rc.1", &[("lib/Sym.Lib.pdb", &pdb)]);
        index_symbol_package(storage, db, FEED, path).await.unwrap();
        let key = pdb::portable_pdb_signature(&pdb).unwrap();
        (pdb, key)
    }

    /// `MNT-38`: a hand-over claims the key for the other version, so that
    /// version must not be in the middle of a delete or an overwrite. One that
    /// is locked is passed over, without waiting for it.
    #[tokio::test]
    async fn a_symbol_is_not_handed_to_a_version_being_changed() {
        const ID: &str = "Busy.Lib";
        let (_d, storage, db) = fixtures().await;
        let (pdb, key) = built_twice(&storage, &db, ID, 11).await;
        let (_sd, path) = snupkg_at(ID, "1.0.0", &[("lib/Sym.Lib.pdb", &pdb)]);
        index_symbol_package(&storage, &db, FEED, path)
            .await
            .unwrap();

        let rc = NuGetVersion::parse("1.0.0-rc.1").unwrap();
        let _gone = locks::lock_version(ID, "1.0.0-rc.1").await;
        let _busy = locks::lock_version(ID, "1.0.0").await;
        crate::retention::purge_symbols(&storage, &db, ID, &rc)
            .await
            .unwrap();
        assert!(db.find_symbol(&key, "sym.lib.pdb").await.unwrap().is_none());
        assert!(storage.get_symbol(&key, "sym.lib.pdb").await.is_err());
    }

    /// A store that stops in one method, on its `nth` call, until the test
    /// lets it go on: a fixed point inside a push or a purge to race against.
    struct Paused {
        inner: FilesystemStorage,
        at: &'static str,
        nth: usize,
        calls: std::sync::atomic::AtomicUsize,
        reached: tokio::sync::Notify,
        go: tokio::sync::Semaphore,
        /// The keys `delete_symbol` was called with, in order.
        deleted: std::sync::Mutex<Vec<String>>,
    }

    impl Paused {
        fn new(inner: FilesystemStorage, at: &'static str, nth: usize) -> Self {
            Self {
                inner,
                at,
                nth,
                calls: Default::default(),
                reached: tokio::sync::Notify::new(),
                go: tokio::sync::Semaphore::new(0),
                deleted: Default::default(),
            }
        }

        async fn pause(&self, at: &str) {
            use std::sync::atomic::Ordering;
            if at == self.at && self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.nth {
                self.reached.notify_one();
                let _ = self.go.acquire().await.unwrap();
            }
        }
    }

    #[async_trait::async_trait]
    impl PackageStorage for Paused {
        async fn store_package(&self, id: &str, version: &str, temp: PathBuf) -> Result<u64> {
            self.inner.store_package(id, version, temp).await
        }
        async fn get_package(&self, id: &str, version: &str) -> Result<PackageContent> {
            self.inner.get_package(id, version).await
        }
        async fn package_exists(&self, id: &str, version: &str) -> bool {
            self.inner.package_exists(id, version).await
        }
        async fn store_symbol_package(
            &self,
            id: &str,
            version: &str,
            temp: PathBuf,
        ) -> Result<u64> {
            self.pause("store_symbol_package").await;
            self.inner.store_symbol_package(id, version, temp).await
        }
        async fn store_symbol(&self, key: &str, filename: &str, bytes: &[u8]) -> Result<()> {
            self.inner.store_symbol(key, filename, bytes).await
        }
        async fn store_symbol_file(&self, key: &str, filename: &str, temp: PathBuf) -> Result<()> {
            self.inner.store_symbol_file(key, filename, temp).await
        }
        async fn get_symbol(&self, key: &str, filename: &str) -> Result<PackageContent> {
            self.inner.get_symbol(key, filename).await
        }
        async fn delete_symbol(&self, key: &str, filename: &str) -> Result<()> {
            self.deleted.lock().unwrap().push(key.to_owned());
            self.pause("delete_symbol").await;
            self.inner.delete_symbol(key, filename).await
        }
        async fn store_aux(
            &self,
            id: &str,
            version: &str,
            kind: crate::storage::AuxFile,
            bytes: &[u8],
        ) -> Result<()> {
            self.inner.store_aux(id, version, kind, bytes).await
        }
        async fn get_aux(
            &self,
            id: &str,
            version: &str,
            kind: crate::storage::AuxFile,
        ) -> Result<Vec<u8>> {
            self.inner.get_aux(id, version, kind).await
        }
        async fn aux_content(
            &self,
            id: &str,
            version: &str,
            kind: crate::storage::AuxFile,
        ) -> Result<PackageContent> {
            self.inner.aux_content(id, version, kind).await
        }
        async fn delete(&self, id: &str, version: &str) -> Result<()> {
            self.inner.delete(id, version).await
        }
        async fn store_blob(&self, sha256_hex: &str, temp: PathBuf) -> Result<u64> {
            self.inner.store_blob(sha256_hex, temp).await
        }
        async fn get_blob(&self, sha256_hex: &str) -> Result<PackageContent> {
            self.inner.get_blob(sha256_hex).await
        }
        async fn delete_blob(&self, sha256_hex: &str) -> Result<()> {
            self.inner.delete_blob(sha256_hex).await
        }
        async fn list_blobs(&self) -> Result<Vec<String>> {
            self.inner.list_blobs().await
        }
    }

    /// `COR-53`: a push of PDBs another version already holds changes
    /// nothing under their keys and used to release them at once, before its
    /// own symbol package was stored. A purge of the other version taking a
    /// key in that gap found no symbol package to hand it to and deleted the
    /// PDB the push had just answered `201` for.
    #[tokio::test]
    async fn a_symbol_push_keeps_its_keys_until_its_symbol_package_is_stored() {
        const ID: &str = "Window.Lib";
        let (_d, storage, db) = fixtures().await;
        let (pdb, key) = built_twice(&storage, &db, ID, 12).await;
        let paused = Paused::new(storage, "store_symbol_package", 1);

        let (_sd, path) = snupkg_at(ID, "1.0.0", &[("lib/Sym.Lib.pdb", &pdb)]);
        let push = index_symbol_package(&paused, &db, FEED, path);
        let purge = async {
            // The push is storing its symbol package.
            paused.reached.notified().await;
            let rc = NuGetVersion::parse("1.0.0-rc.1").unwrap();
            let purge = async {
                let _gone = locks::lock_version(ID, "1.0.0-rc.1").await;
                crate::retention::purge_symbols(&paused.inner, &db, ID, &rc).await
            };
            tokio::pin!(purge);
            // As far as the purge gets before the push goes on.
            let early =
                tokio::time::timeout(std::time::Duration::from_millis(300), &mut purge).await;
            paused.go.add_permits(1);
            match early {
                Ok(done) => done,
                Err(_) => purge.await,
            }
        };
        let (pushed, purged) = tokio::join!(push, purge);
        pushed.unwrap();
        purged.unwrap();

        let owner = db.find_symbol(&key, "sym.lib.pdb").await.unwrap();
        assert_eq!(owner.unwrap().normalized_version, "1.0.0");
        let PackageContent::LocalPath(stored) =
            paused.inner.get_symbol(&key, "sym.lib.pdb").await.unwrap();
        assert_eq!(std::fs::read(stored).unwrap(), pdb);
    }

    /// `COR-53`: a purge deleted the files key by key but every row only at
    /// the end, so in between another version pushing one of those PDBs found
    /// the key claimed by the version going away, with no file, and was
    /// refused. Each row now goes with its file, under the key's lock.
    #[tokio::test]
    async fn a_purge_releases_each_key_with_its_file() {
        const ID: &str = "Rows.Lib";
        let (_d, storage, db) = fixtures().await;
        let pdbs = build(13);
        for version in ["1.0.0", "2.0.0"] {
            publish_build(&storage, &db, ID, version, &pdbs).await;
        }
        let (_sd, path) = snupkg_at(
            ID,
            "1.0.0",
            &[
                ("lib/net8.0/A.pdb", &pdbs[0]),
                ("lib/net8.0/B.pdb", &pdbs[1]),
            ],
        );
        index_symbol_package(&storage, &db, FEED, path)
            .await
            .unwrap();
        let paused = Paused::new(storage, "delete_symbol", 2);

        let gone = NuGetVersion::parse("1.0.0").unwrap();
        let purge = async {
            let _gone = locks::lock_version(ID, "1.0.0").await;
            crate::retention::purge_symbols(&paused, &db, ID, &gone).await
        };
        let push = async {
            // One key is gone, the purge is deleting the other.
            paused.reached.notified().await;
            let first = paused.deleted.lock().unwrap()[0].clone();
            let (entry, pdb) = if pdb::portable_pdb_signature(&pdbs[0]).unwrap() == first {
                ("lib/net8.0/A.pdb", &pdbs[0])
            } else {
                ("lib/net8.0/B.pdb", &pdbs[1])
            };
            let (_sd, path) = snupkg_at(ID, "2.0.0", &[(entry, pdb)]);
            let pushed = index_symbol_package(&paused.inner, &db, FEED, path).await;
            paused.go.add_permits(1);
            (pushed, first)
        };
        let (purged, (pushed, first)) = tokio::join!(purge, push);
        purged.unwrap();
        pushed.unwrap();

        assert!(db.find_symbols(ID, &gone).await.unwrap().is_empty());
        let owner = db.find_symbol(&first, &file_name_of(&pdbs, &first)).await;
        assert_eq!(owner.unwrap().unwrap().normalized_version, "2.0.0");
    }

    /// The file name `build`'s PDB with `key` is pushed under.
    fn file_name_of(pdbs: &[Vec<u8>; 2], key: &str) -> String {
        if pdb::portable_pdb_signature(&pdbs[0]).unwrap() == key {
            "a.pdb".into()
        } else {
            "b.pdb".into()
        }
    }
}
