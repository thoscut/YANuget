//! Package metadata database abstraction.
//!
//! The web layer talks to a [`PackageDatabase`] trait object so the storage
//! engine can be swapped. A SQLite implementation ships in [`sqlite`]; other
//! engines (PostgreSQL, MySQL) can be added behind the same trait.

pub mod sqlite;

use async_trait::async_trait;

use crate::error::Result;
use crate::models::Package;
use crate::version::NuGetVersion;

pub use sqlite::SqliteDatabase;

/// A search query against the package index.
#[derive(Debug, Clone)]
pub struct SearchRequest {
    /// Free-text query. Empty matches everything.
    pub query: String,
    pub skip: i64,
    pub take: i64,
    pub include_prerelease: bool,
    pub include_semver2: bool,
    /// Optional package-type filter (e.g. `DotnetTool`).
    pub package_type: Option<String>,
}

impl Default for SearchRequest {
    fn default() -> Self {
        Self {
            query: String::new(),
            skip: 0,
            take: 20,
            include_prerelease: true,
            include_semver2: true,
            package_type: None,
        }
    }
}

/// One search hit: every (matching) version of a single package id, already
/// sorted ascending by version.
#[derive(Debug, Clone)]
pub struct SearchGroup {
    pub packages: Vec<Package>,
}

impl SearchGroup {
    /// The version chosen to represent the group (the highest one).
    pub fn latest(&self) -> &Package {
        self.packages
            .last()
            .expect("a search group always has at least one package")
    }

    /// Total downloads across all versions in the group.
    pub fn total_downloads(&self) -> u64 {
        self.packages.iter().map(|p| p.downloads).sum()
    }
}

/// A page of search results.
#[derive(Debug, Clone)]
pub struct SearchPage {
    pub total_hits: i64,
    pub groups: Vec<SearchGroup>,
}

/// Metadata store for indexed packages.
#[async_trait]
pub trait PackageDatabase: Send + Sync {
    /// Insert a freshly indexed package. Returns
    /// [`Error::PackageAlreadyExists`](crate::error::Error::PackageAlreadyExists)
    /// if the id/version pair is already present.
    async fn add(&self, package: &Package) -> Result<()>;

    /// Whether a specific id/version exists (listed or not).
    async fn exists(&self, id: &str, version: &NuGetVersion) -> Result<bool>;

    /// Fetch a single package version.
    async fn find(&self, id: &str, version: &NuGetVersion) -> Result<Option<Package>>;

    /// Fetch all versions of a package id, sorted ascending. When
    /// `include_unlisted` is false, unlisted versions are omitted.
    async fn find_versions(&self, id: &str, include_unlisted: bool) -> Result<Vec<Package>>;

    /// Set the listed flag. Returns `true` if a row was updated.
    async fn set_listed(&self, id: &str, version: &NuGetVersion, listed: bool) -> Result<bool>;

    /// Permanently remove a version. Returns `true` if a row was deleted.
    async fn delete(&self, id: &str, version: &NuGetVersion) -> Result<bool>;

    /// Atomically increment the download counter for a version.
    async fn increment_downloads(&self, id: &str, version: &NuGetVersion) -> Result<()>;

    /// Execute a search query.
    async fn search(&self, request: &SearchRequest) -> Result<SearchPage>;

    /// Autocomplete package ids by prefix/substring.
    async fn autocomplete(&self, query: &str, skip: i64, take: i64) -> Result<Vec<String>>;
}
