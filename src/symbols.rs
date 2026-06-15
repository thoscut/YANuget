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

use std::path::PathBuf;

use crate::database::PackageDatabase;
use crate::error::{Error, Result};
use crate::nuspec;
use crate::storage::PackageStorage;
use crate::version::NuGetVersion;
use crate::{nupkg, pdb};

/// Hard cap on a single `.pdb` we will read into memory to index/store it.
const MAX_PDB_BYTES: u64 = 256 * 1024 * 1024;

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
    temp_path: PathBuf,
) -> Result<SymbolResult> {
    let result = index_inner(storage, db, &temp_path).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }
    result
}

async fn index_inner(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    temp_path: &PathBuf,
) -> Result<SymbolResult> {
    // 1. Read the manifest to learn which package these symbols belong to.
    let archive = nupkg::read_archive(temp_path).await?;
    let manifest = nuspec::parse_nuspec(&archive.nuspec_xml)?;
    let version = NuGetVersion::parse(&manifest.version)
        .map_err(|e| Error::InvalidPackage(format!("invalid <version>: {e}")))?;
    let id = manifest.id.clone();
    let normalized = version.normalized();

    // 2. The owning package must already exist (NuGet's ordering rule).
    if !db.exists(&id, &version).await? {
        return Err(Error::PackageNotFound);
    }

    // 3. Index every PDB in the archive by its SSQP key.
    let pdb_entries: Vec<String> = archive
        .entry_names
        .iter()
        .filter(|name| name.ends_with(".pdb"))
        .cloned()
        .collect();

    let mut indexed = 0;
    let mut skipped = 0;
    for entry in &pdb_entries {
        let Some(bytes) = nupkg::extract_file(temp_path, entry, MAX_PDB_BYTES).await? else {
            continue;
        };
        let filename = file_name(entry);
        match pdb::portable_pdb_signature(&bytes) {
            Some(key) => {
                storage.store_symbol(&key, &filename, &bytes).await?;
                db.add_symbol(&key, &filename, &id, &version).await?;
                indexed += 1;
            }
            None => {
                // Not a Portable PDB (e.g. a native/Windows PDB). Keep the
                // payload via the stored .snupkg, but it cannot be indexed.
                tracing::warn!(%id, pdb = %filename, "symbol file is not an indexable Portable PDB");
                skipped += 1;
            }
        }
    }

    // 4. Store the whole .snupkg alongside the package (moves the temp file).
    storage
        .store_symbol_package(&id, &normalized, temp_path.clone())
        .await?;

    Ok(SymbolResult {
        id,
        version,
        indexed,
        skipped,
    })
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

    #[test]
    fn file_name_strips_directories() {
        assert_eq!(file_name("lib/net8.0/app.pdb"), "app.pdb");
        assert_eq!(file_name("App.PDB"), "app.pdb");
        assert_eq!(file_name("a\\b\\c.pdb"), "c.pdb");
    }
}
