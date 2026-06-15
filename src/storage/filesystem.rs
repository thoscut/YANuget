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
}

#[async_trait]
impl PackageStorage for FilesystemStorage {
    async fn store_package(&self, id: &str, version: &str, temp_path: PathBuf) -> Result<u64> {
        let dir = self.version_dir(id, version)?;
        tokio::fs::create_dir_all(&dir).await?;
        let dest = self.package_path(id, version)?;

        // Prefer an atomic rename (no copy, regardless of package size). Fall
        // back to a streaming copy when the temp file lives on another device.
        match tokio::fs::rename(&temp_path, &dest).await {
            Ok(()) => {}
            Err(_) => {
                tokio::fs::copy(&temp_path, &dest).await?;
                // Best-effort cleanup of the source temp file.
                let _ = tokio::fs::remove_file(&temp_path).await;
            }
        }

        let meta = tokio::fs::metadata(&dest).await?;
        Ok(meta.len())
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
fn safe_segment(segment: &str) -> Result<String> {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.contains('/')
        || segment.contains('\\')
        || segment.contains('\0')
    {
        return Err(Error::BadRequest(format!(
            "invalid storage path segment: {segment:?}"
        )));
    }
    Ok(segment.to_string())
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
        let got = storage.get_aux("Pkg", "1.0.0", AuxFile::Nuspec).await.unwrap();
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
    }
}
