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
    let manifest = nuspec::parse_nuspec(&archive.nuspec_xml)?;
    let version = NuGetVersion::parse(&manifest.version)
        .map_err(|e| Error::InvalidPackage(format!("invalid <version>: {e}")))?;
    let id = manifest.id.clone();
    let normalized = version.normalized();

    // 2. The owning package must already be a member of this feed (NuGet's
    //    ordering rule). Symbols themselves are stored globally by SSQP key.
    if !db.exists(feed, &id, &version).await? {
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

    /// A minimal Portable PDB whose `#Pdb` stream begins with `guid`.
    fn portable_pdb(guid: &[u8; 16]) -> Vec<u8> {
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
        buf.extend_from_slice(&[0u8; 4]);
        buf
    }

    fn make_snupkg(portable: bool, native: bool) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pkg.snupkg");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("Sym.Lib.nuspec", opts).unwrap();
        zip.write_all(
            br#"<package><metadata><id>Sym.Lib</id><version>1.0.0</version>
                <authors>A</authors><description>d</description></metadata></package>"#,
        )
        .unwrap();
        if portable {
            zip.start_file("lib/net8.0/sym.lib.pdb", opts).unwrap();
            zip.write_all(&portable_pdb(&[7u8; 16])).unwrap();
        }
        if native {
            zip.start_file("lib/net8.0/native.pdb", opts).unwrap();
            zip.write_all(b"Microsoft C/C++ MSF 7.00\r\n").unwrap();
        }
        zip.finish().unwrap();
        (dir, path)
    }

    async fn fixtures() -> (tempfile::TempDir, FilesystemStorage, SqliteDatabase) {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        (dir, storage, db)
    }

    fn owning_package() -> crate::models::Package {
        crate::models::Package {
            id: "Sym.Lib".into(),
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

    #[tokio::test]
    async fn indexes_portable_skips_native() {
        let (_d, storage, db) = fixtures().await;
        db.add_to_feed(FEED, &owning_package()).await.unwrap();

        let (_sd, snupkg) = make_snupkg(true, true);
        let result = index_symbol_package(&storage, &db, FEED, snupkg)
            .await
            .unwrap();
        assert_eq!(result.indexed, 1);
        assert_eq!(result.skipped, 1);

        // The portable PDB is resolvable by its SSQP key.
        let key = crate::pdb::portable_pdb_signature(&portable_pdb(&[7u8; 16])).unwrap();
        assert!(db.find_symbol(&key, "sym.lib.pdb").await.unwrap().is_some());
        assert!(storage.get_symbol(&key, "sym.lib.pdb").await.is_ok());
    }

    #[tokio::test]
    async fn rejects_symbols_for_unknown_package() {
        let (_d, storage, db) = fixtures().await;
        // No package added first.
        let (_sd, snupkg) = make_snupkg(true, false);
        let err = index_symbol_package(&storage, &db, FEED, snupkg.clone())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PackageNotFound));
        // The rejected temp file was cleaned up.
        assert!(!snupkg.exists());
    }
}
