//! Filesystem-backed [`PackageStorage`].
//!
//! Layout (mirroring BaGet/BaGetter for drop-in compatibility):
//!
//! ```text
//! {root}/{lower_id}/{normalized_version}/
//!     {lower_id}.{normalized_version}.nupkg
//!     manifest.nuspec
//!     readme
//!     icon
//! ```

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::error::{Error, Result};

use super::{AuxFile, PackageContent, PackageStorage};

/// Stores packages under a single root directory on the local filesystem.
#[derive(Debug, Clone)]
pub struct FilesystemStorage {
    root: PathBuf,
}

impl FilesystemStorage {
    /// Create a storage rooted at `root`, creating the directory if needed.
    pub async fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self { root })
    }

    /// Directory holding a single package version's files. The id and version
    /// are lower-cased so lookups are case-insensitive regardless of the casing
    /// the caller used (matching NuGet's normalized URLs).
    fn version_dir(&self, id: &str, version: &str) -> Result<PathBuf> {
        let id = safe_segment(id)?.to_ascii_lowercase();
        let version = safe_segment(version)?.to_ascii_lowercase();
        Ok(self.root.join(id).join(version))
    }

    fn package_path(&self, id: &str, version: &str) -> Result<PathBuf> {
        let dir = self.version_dir(id, version)?;
        let lid = id.to_ascii_lowercase();
        let lver = version.to_ascii_lowercase();
        Ok(dir.join(format!("{lid}.{lver}.nupkg")))
    }

    fn aux_path(&self, id: &str, version: &str, kind: AuxFile) -> Result<PathBuf> {
        Ok(self.version_dir(id, version)?.join(kind.file_name()))
    }

    fn symbol_package_path(&self, id: &str, version: &str) -> Result<PathBuf> {
        let dir = self.version_dir(id, version)?;
        let lid = id.to_ascii_lowercase();
        let lver = version.to_ascii_lowercase();
        Ok(dir.join(format!("{lid}.{lver}.snupkg")))
    }

    /// Directory holding one symbol file, addressed by its SSQP key. Kept under
    /// a `.symbols` root, separate from the per-version package directories,
    /// because symbols are looked up by key rather than by id/version.
    fn symbol_path(&self, key: &str, filename: &str) -> Result<PathBuf> {
        let key = safe_segment(key)?.to_ascii_lowercase();
        let filename = safe_segment(filename)?.to_ascii_lowercase();
        Ok(self.root.join(".symbols").join(key).join(filename))
    }

    /// Where a blob with this SHA-256 lives: `.blobs/sha256/{first two}/{all}`.
    /// Only a lower-case 64-digit hex string is a blob name, so nothing a
    /// client sends ever becomes part of this path.
    fn blob_path(&self, sha256_hex: &str) -> Result<PathBuf> {
        if sha256_hex.len() != 64
            || !sha256_hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::BadRequest(format!(
                "not a SHA-256 blob name: {sha256_hex:?}"
            )));
        }
        Ok(self
            .root
            .join(".blobs")
            .join("sha256")
            .join(&sha256_hex[..2])
            .join(sha256_hex))
    }
}

/// Move `temp_path` to `dest`: a rename when both are on one filesystem (no
/// copy, whatever the size), else a streaming copy and removal of the source.
async fn move_into_place(temp_path: &Path, dest: &Path) -> Result<()> {
    match tokio::fs::rename(temp_path, dest).await {
        Ok(()) => Ok(()),
        Err(_) => {
            tokio::fs::copy(temp_path, dest).await?;
            // Best-effort cleanup of the source temp file.
            let _ = tokio::fs::remove_file(temp_path).await;
            Ok(())
        }
    }
}

#[async_trait]
impl PackageStorage for FilesystemStorage {
    async fn store_package(&self, id: &str, version: &str, temp_path: PathBuf) -> Result<u64> {
        let dir = self.version_dir(id, version)?;
        tokio::fs::create_dir_all(&dir).await?;
        let dest = self.package_path(id, version)?;

        // Prefer an atomic rename (no copy, regardless of package size). Fall
        // back to a streaming copy when the temp file lives on another device.
        move_into_place(&temp_path, &dest).await?;

        let meta = tokio::fs::metadata(&dest).await?;
        Ok(meta.len())
    }

    async fn store_blob(&self, sha256_hex: &str, temp_path: PathBuf) -> Result<u64> {
        let dest = self.blob_path(sha256_hex)?;
        // The same bytes are already stored: keep those, drop this copy. The
        // name *is* the content, so there is nothing to reconcile.
        if let Ok(meta) = tokio::fs::metadata(&dest).await {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Ok(meta.len());
        }
        if let Some(dir) = dest.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        move_into_place(&temp_path, &dest).await?;
        Ok(tokio::fs::metadata(&dest).await?.len())
    }

    async fn get_blob(&self, sha256_hex: &str) -> Result<PackageContent> {
        let path = self.blob_path(sha256_hex)?;
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            Ok(PackageContent::LocalPath(path))
        } else {
            Err(Error::PackageNotFound)
        }
    }

    async fn delete_blob(&self, sha256_hex: &str) -> Result<()> {
        let path = self.blob_path(sha256_hex)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_package(&self, id: &str, version: &str) -> Result<PackageContent> {
        let path = self.package_path(id, version)?;
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            Ok(PackageContent::LocalPath(path))
        } else {
            Err(Error::PackageNotFound)
        }
    }

    async fn package_exists(&self, id: &str, version: &str) -> bool {
        match self.package_path(id, version) {
            Ok(path) => tokio::fs::try_exists(&path).await.unwrap_or(false),
            Err(_) => false,
        }
    }

    async fn store_symbol_package(
        &self,
        id: &str,
        version: &str,
        temp_path: PathBuf,
    ) -> Result<u64> {
        let dir = self.version_dir(id, version)?;
        tokio::fs::create_dir_all(&dir).await?;
        let dest = self.symbol_package_path(id, version)?;
        match tokio::fs::rename(&temp_path, &dest).await {
            Ok(()) => {}
            Err(_) => {
                tokio::fs::copy(&temp_path, &dest).await?;
                let _ = tokio::fs::remove_file(&temp_path).await;
            }
        }
        let meta = tokio::fs::metadata(&dest).await?;
        Ok(meta.len())
    }

    async fn store_symbol(&self, key: &str, filename: &str, bytes: &[u8]) -> Result<()> {
        let dest = self.symbol_path(key, filename)?;
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(&dest, bytes).await?;
        Ok(())
    }

    async fn get_symbol(&self, key: &str, filename: &str) -> Result<PackageContent> {
        let path = self.symbol_path(key, filename)?;
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            Ok(PackageContent::LocalPath(path))
        } else {
            Err(Error::PackageNotFound)
        }
    }

    async fn delete_symbol(&self, key: &str, filename: &str) -> Result<()> {
        let path = self.symbol_path(key, filename)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) | Err(_) => {}
        }
        // Best-effort: remove the now-empty key directory.
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::remove_dir(parent).await;
        }
        Ok(())
    }

    async fn store_aux(&self, id: &str, version: &str, kind: AuxFile, bytes: &[u8]) -> Result<()> {
        let dir = self.version_dir(id, version)?;
        tokio::fs::create_dir_all(&dir).await?;
        let path = self.aux_path(id, version, kind)?;
        tokio::fs::write(&path, bytes).await?;
        Ok(())
    }

    async fn get_aux(&self, id: &str, version: &str, kind: AuxFile) -> Result<Vec<u8>> {
        let path = self.aux_path(id, version, kind)?;
        match tokio::fs::read(&path).await {
            Ok(bytes) => Ok(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::PackageNotFound),
            Err(e) => Err(e.into()),
        }
    }

    async fn aux_content(&self, id: &str, version: &str, kind: AuxFile) -> Result<PackageContent> {
        let path = self.aux_path(id, version, kind)?;
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            Ok(PackageContent::LocalPath(path))
        } else {
            Err(Error::PackageNotFound)
        }
    }

    async fn delete(&self, id: &str, version: &str) -> Result<()> {
        let dir = self.version_dir(id, version)?;
        match tokio::fs::remove_dir_all(&dir).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

/// Validate a path segment, rejecting anything that could escape the root.
///
/// Separators and `..` are the portable part. The rest is Windows: a `:` makes
/// `c:x` a drive-relative path, which `PathBuf::join` lets *replace* the base
/// instead of extending it, and turns `name:stream` into an NTFS alternate data
/// stream. Device names (`NUL`, `COM1.pdb`) open devices rather than files, and
/// Windows silently strips a trailing dot or space, so `a.` and `a` would be
/// two names for one file. They are refused on every platform, so a store
/// written on one system stays valid on the other.
fn safe_segment(segment: &str) -> Result<String> {
    let invalid = segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment
            .chars()
            .any(|c| matches!(c, '/' | '\\' | ':') || c.is_control())
        || segment.ends_with(['.', ' '])
        || is_windows_device_name(segment);
    if invalid {
        return Err(Error::BadRequest(format!(
            "invalid storage path segment: {segment:?}"
        )));
    }
    Ok(segment.to_string())
}

/// Whether `segment` names a Windows device, with or without an extension
/// (`nul`, `COM1.pdb`, `lpt9.txt`).
pub(crate) fn is_windows_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or("").trim_end();
    let stem = stem.to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_storage() -> (tempfile::TempDir, FilesystemStorage) {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path()).await.unwrap();
        (dir, storage)
    }

    async fn write_temp(content: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("upload.tmp");
        tokio::fs::write(&path, content).await.unwrap();
        (dir, path)
    }

    #[test]
    fn path_segments_that_would_leave_the_store_on_windows_are_refused() {
        for bad in [
            "c:evil.pdb", // drive-relative: replaces the base when joined
            "a:b",        // an NTFS alternate data stream
            "nul.pdb",    // a device, whatever the extension
            "COM1",
            "lpt9.txt",
            "name.",     // Windows drops the trailing dot…
            "name ",     // …and the trailing space
            "tab\there", // control characters
            "..",
            "a/b",
            "a\\b",
            "",
        ] {
            assert!(safe_segment(bad).is_err(), "{bad:?} was accepted");
        }
        for good in [
            "contoso.utils",
            "1.0.0-beta.2",
            "com10",
            "console.pdb",
            "nullable.pdb",
        ] {
            assert!(safe_segment(good).is_ok(), "{good:?} was refused");
        }
    }

    #[tokio::test]
    async fn blobs_are_stored_once_under_their_hash() {
        let (_d, storage) = temp_storage().await;
        let sha = "ab".repeat(32);
        let (_t1, first) = write_temp(b"image bytes").await;
        assert_eq!(storage.store_blob(&sha, first).await.unwrap(), 11);
        // The same content again: kept once, and the second copy is gone.
        let (_t2, second) = write_temp(b"image bytes").await;
        storage.store_blob(&sha, second.clone()).await.unwrap();
        assert!(!second.exists());
        let PackageContent::LocalPath(path) = storage.get_blob(&sha).await.unwrap();
        assert!(
            path.ends_with(format!("sha256/ab/{sha}"))
                || path.ends_with(format!("sha256\\ab\\{sha}"))
        );
        storage.delete_blob(&sha).await.unwrap();
        storage.delete_blob(&sha).await.unwrap();
        assert!(matches!(
            storage.get_blob(&sha).await,
            Err(Error::PackageNotFound)
        ));
        // Only a hash is a blob name.
        for bad in ["../../etc/passwd", "AB".repeat(32).as_str(), "abc"] {
            assert!(storage.get_blob(bad).await.is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn store_and_retrieve_package() {
        let (_d, storage) = temp_storage().await;
        let (_td, temp) = write_temp(b"nupkg-bytes").await;

        let size = storage
            .store_package("Contoso.Utils", "1.0.0", temp)
            .await
            .unwrap();
        assert_eq!(size, b"nupkg-bytes".len() as u64);
        assert!(storage.package_exists("Contoso.Utils", "1.0.0").await);

        let content = storage.get_package("Contoso.Utils", "1.0.0").await.unwrap();
        let PackageContent::LocalPath(path) = content;
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"nupkg-bytes");
    }

    #[tokio::test]
    async fn aux_files_round_trip() {
        let (_d, storage) = temp_storage().await;
        storage
            .store_aux("Pkg", "1.0.0", AuxFile::Nuspec, b"<nuspec/>")
            .await
            .unwrap();
        let got = storage
            .get_aux("Pkg", "1.0.0", AuxFile::Nuspec)
            .await
            .unwrap();
        assert_eq!(got, b"<nuspec/>");

        let missing = storage.get_aux("Pkg", "1.0.0", AuxFile::Icon).await;
        assert!(matches!(missing.unwrap_err(), Error::PackageNotFound));
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let (_d, storage) = temp_storage().await;
        let (_td, temp) = write_temp(b"x").await;
        storage.store_package("P", "1.0.0", temp).await.unwrap();
        storage.delete("P", "1.0.0").await.unwrap();
        assert!(!storage.package_exists("P", "1.0.0").await);
        // Deleting again is fine.
        storage.delete("P", "1.0.0").await.unwrap();
    }

    #[tokio::test]
    async fn rejects_path_traversal() {
        let (_d, storage) = temp_storage().await;
        assert!(storage.get_package("..", "1.0.0").await.is_err());
        assert!(storage.package_path("a/b", "1.0.0").is_err());
        // Symbol keys/filenames are validated too.
        assert!(storage.symbol_path("../etc", "x.pdb").is_err());
        assert!(storage.symbol_path("KEY", "a/b.pdb").is_err());
    }

    #[tokio::test]
    async fn symbols_round_trip_and_delete() {
        let (_d, storage) = temp_storage().await;
        storage
            .store_symbol("ABCDEF01FFFFFFFF", "App.pdb", b"pdb-bytes")
            .await
            .unwrap();

        // Lookup is case-insensitive (the key/filename are lower-cased on disk).
        let content = storage
            .get_symbol("abcdef01ffffffff", "app.pdb")
            .await
            .unwrap();
        let PackageContent::LocalPath(path) = content;
        assert_eq!(tokio::fs::read(path).await.unwrap(), b"pdb-bytes");

        storage
            .delete_symbol("ABCDEF01FFFFFFFF", "App.pdb")
            .await
            .unwrap();
        assert!(storage
            .get_symbol("abcdef01ffffffff", "app.pdb")
            .await
            .is_err());
        // Deleting a missing symbol is a no-op.
        storage.delete_symbol("nope", "x.pdb").await.unwrap();
    }

    #[tokio::test]
    async fn symbol_package_is_stored_next_to_version() {
        let (_d, storage) = temp_storage().await;
        let (_td, temp) = write_temp(b"snupkg-bytes").await;
        let size = storage
            .store_symbol_package("Sym.Lib", "1.0.0", temp)
            .await
            .unwrap();
        assert_eq!(size, b"snupkg-bytes".len() as u64);
        // It lives in the same version directory and is removed with it.
        storage.delete("sym.lib", "1.0.0").await.unwrap();
        let (_td2, temp2) = write_temp(b"x").await;
        storage
            .store_package("Sym.Lib", "1.0.0", temp2)
            .await
            .unwrap();
        // (sanity) the version dir is usable again after delete.
        assert!(storage.package_exists("sym.lib", "1.0.0").await);
    }
}
