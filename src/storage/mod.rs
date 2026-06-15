//! Package storage abstraction.
//!
//! Storage keeps the large `.nupkg` payloads and a few small auxiliary files
//! (the extracted nuspec, readme and icon). The trait is intentionally small
//! and streaming-friendly: packages are ingested by handing over an
//! already-written temp file (so the backend can `rename` it into place with no
//! copy), and retrieved as a [`PackageContent`] the web layer can serve with
//! HTTP range support.

pub mod filesystem;

use std::path::PathBuf;

use async_trait::async_trait;

use crate::error::Result;

pub use filesystem::FilesystemStorage;

/// A small auxiliary file stored alongside a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxFile {
    /// The extracted `.nuspec` manifest.
    Nuspec,
    /// The embedded readme, if any.
    Readme,
    /// The embedded icon, if any.
    Icon,
}

impl AuxFile {
    fn file_name(self) -> &'static str {
        match self {
            AuxFile::Nuspec => "manifest.nuspec",
            AuxFile::Readme => "readme",
            AuxFile::Icon => "icon",
        }
    }
}

/// A handle to package content ready to be served.
///
/// Today only a local filesystem path is produced, which lets the HTTP layer
/// serve 25 GiB+ packages with zero-copy range requests. A future object-store
/// backend can add a streaming variant without changing the web layer's
/// contract beyond matching a new arm.
#[derive(Debug, Clone)]
pub enum PackageContent {
    /// The content is a file on the local filesystem.
    LocalPath(PathBuf),
}

/// Storage backend for package payloads and their small sidecar files.
#[async_trait]
pub trait PackageStorage: Send + Sync {
    /// Move an already-written temp file into permanent storage as the
    /// `.nupkg` for `id`/`version`. Returns the stored size in bytes.
    ///
    /// Implementations should prefer an atomic rename and fall back to a
    /// streaming copy across filesystem boundaries — never buffering in memory.
    async fn store_package(&self, id: &str, version: &str, temp_path: PathBuf) -> Result<u64>;

    /// Resolve the stored `.nupkg` for serving.
    async fn get_package(&self, id: &str, version: &str) -> Result<PackageContent>;

    /// Whether a `.nupkg` already exists for `id`/`version`.
    async fn package_exists(&self, id: &str, version: &str) -> bool;

    /// Move an already-written temp file into storage as the `.snupkg` symbol
    /// package for `id`/`version`. Stored next to the `.nupkg` so it is removed
    /// together with the version on delete. Returns the stored size in bytes.
    async fn store_symbol_package(
        &self,
        id: &str,
        version: &str,
        temp_path: PathBuf,
    ) -> Result<u64>;

    /// Store one extracted symbol file (a `.pdb`) addressed by its SSQP `key`
    /// and `filename`, so a debugger can fetch it directly. Symbol files are
    /// bounded in size and already in memory, so bytes are passed directly.
    async fn store_symbol(&self, key: &str, filename: &str, bytes: &[u8]) -> Result<()>;

    /// Resolve a stored symbol file for serving.
    async fn get_symbol(&self, key: &str, filename: &str) -> Result<PackageContent>;

    /// Delete a stored symbol file. Succeeds even if it is already gone.
    async fn delete_symbol(&self, key: &str, filename: &str) -> Result<()>;

    /// Store a small auxiliary file.
    async fn store_aux(&self, id: &str, version: &str, kind: AuxFile, bytes: &[u8]) -> Result<()>;

    /// Read a small auxiliary file.
    async fn get_aux(&self, id: &str, version: &str, kind: AuxFile) -> Result<Vec<u8>>;

    /// Delete a package and all of its auxiliary files. Succeeds even if some
    /// files are already gone.
    async fn delete(&self, id: &str, version: &str) -> Result<()>;
}
