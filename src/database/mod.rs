//! Package metadata database abstraction.
//!
//! The web layer talks to a [`PackageDatabase`] trait object so the storage
//! engine can be swapped. A SQLite implementation ships in [`sqlite`]; other
//! engines (PostgreSQL, MySQL) can be added behind the same trait.
//!
//! ## Feeds and deduplication
//!
//! Package metadata and payloads are stored **once** (keyed by id/version) and
//! shared by every feed. A *feed* is a named set of memberships: the
//! [`Membership`] rows that link a feed to the versions it exposes, each
//! carrying that feed's own mutable state (listed / enabled / pending /
//! flagged). The same version can therefore belong to many feeds — a release
//! ring, a mirror cache, a curated set — without ever duplicating its bytes.
//!
//! Methods come in two flavours: a handful operate on the **global** package
//! data (no feed), the rest are **feed-scoped** and take a `feed` name.

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

/// One version's membership of a single feed: the per-feed mutable state that
/// sits alongside the shared, immutable package metadata.
#[derive(Debug, Clone)]
pub struct Membership {
    pub feed: String,
    pub lower_id: String,
    pub normalized_version: String,
    /// Visible in search/registration (the NuGet "list" flag).
    pub listed: bool,
    /// Admin enable flag; a disabled membership is withheld entirely.
    pub enabled: bool,
    /// Awaiting approval (release-ring gate / mirror approval). A pending
    /// membership is withheld from clients until an admin approves it.
    pub pending: bool,
    /// Flagged by a policy check (e.g. a disallowed license under "warn").
    pub flagged: bool,
    /// Human-readable reason for [`Membership::flagged`].
    pub flag_reason: Option<String>,
}

impl Membership {
    /// An active (listed, enabled, not pending, not flagged) membership for the
    /// given package in `feed`.
    pub fn active(feed: &str, package: &Package) -> Self {
        Self {
            feed: feed.to_string(),
            lower_id: package.lower_id(),
            normalized_version: package.normalized_version(),
            listed: true,
            enabled: true,
            pending: false,
            flagged: false,
            flag_reason: None,
        }
    }
}

/// A package version together with its membership state in one feed. Returned by
/// admin/retention listings that must see pending, disabled and flagged rows.
#[derive(Debug, Clone)]
pub struct FeedVersion {
    pub package: Package,
    pub pending: bool,
    pub flagged: bool,
    pub flag_reason: Option<String>,
}

/// Metadata store for indexed packages.
#[async_trait]
pub trait PackageDatabase: Send + Sync {
    // --- global package data (shared by every feed) ---

    /// Insert global package metadata if absent. Idempotent: returns `true` when
    /// a new row was written, `false` when the version already existed (e.g. it
    /// was already pushed to another feed).
    async fn upsert_package_data(&self, package: &Package) -> Result<bool>;

    /// Whether global metadata exists for this id/version, regardless of feed.
    async fn package_data_exists(&self, id: &str, version: &NuGetVersion) -> Result<bool>;

    /// Fetch global package metadata, ignoring feed membership and visibility.
    async fn get_package_data(&self, id: &str, version: &NuGetVersion) -> Result<Option<Package>>;

    /// Hard-delete global metadata (and every feed membership). Returns `true`
    /// if a row was removed. Caller is responsible for storage/symbol cleanup.
    async fn delete_package_data(&self, id: &str, version: &NuGetVersion) -> Result<bool>;

    /// How many feeds currently contain this version.
    async fn feed_count(&self, id: &str, version: &NuGetVersion) -> Result<i64>;

    // --- feed membership ---

    /// Add a membership. Returns
    /// [`Error::PackageAlreadyExists`](crate::error::Error::PackageAlreadyExists)
    /// when the version is already a member of the feed.
    async fn add_membership(&self, membership: &Membership) -> Result<()>;

    /// Remove a membership. Returns `true` if a row was removed.
    async fn remove_membership(&self, feed: &str, id: &str, version: &NuGetVersion)
        -> Result<bool>;

    /// Fetch a membership (any state) for admin/promotion logic.
    async fn get_membership(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<Option<Membership>>;

    /// Clear the pending flag on a membership (approve / promote). Returns `true`
    /// if a row was updated.
    async fn approve_membership(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<bool>;

    /// Convenience: insert the global data (if needed) and an active membership.
    async fn add_to_feed(&self, feed: &str, package: &Package) -> Result<()> {
        self.upsert_package_data(package).await?;
        self.add_membership(&Membership::active(feed, package))
            .await
    }

    // --- feed-scoped reads/writes ---

    /// Whether a specific id/version is a member of `feed` (any state).
    async fn exists(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<bool>;

    /// Fetch a single servable (enabled, not pending) package version in `feed`.
    async fn find(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<Option<Package>>;

    /// Fetch all servable versions of a package id in `feed`, sorted ascending.
    /// When `include_unlisted` is false, unlisted versions are omitted.
    async fn find_versions(
        &self,
        feed: &str,
        id: &str,
        include_unlisted: bool,
    ) -> Result<Vec<Package>>;

    /// Set the listed flag for a membership. Returns `true` if a row was updated.
    async fn set_listed(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
        listed: bool,
    ) -> Result<bool>;

    /// Set the admin `enabled` flag for a membership. A disabled membership is
    /// withheld entirely. Returns `true` if a row was updated.
    async fn set_enabled(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
        enabled: bool,
    ) -> Result<bool>;

    /// Whether a version may be served from `feed`: present, enabled and not
    /// pending. (Unlisted-but-enabled versions are still servable by version.)
    async fn is_servable(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<bool>;

    /// Every version of a package id in `feed` — including unlisted, disabled
    /// **and pending** — sorted ascending. For admin views and retention.
    async fn find_all_versions(&self, feed: &str, id: &str) -> Result<Vec<FeedVersion>>;

    /// Atomically increment the (global) download counter for a version.
    async fn increment_downloads(&self, id: &str, version: &NuGetVersion) -> Result<()>;

    /// Execute a search query within `feed`.
    async fn search(&self, feed: &str, request: &SearchRequest) -> Result<SearchPage>;

    /// Autocomplete package ids in `feed` by prefix/substring.
    async fn autocomplete(
        &self,
        feed: &str,
        query: &str,
        skip: i64,
        take: i64,
    ) -> Result<Vec<String>>;

    /// Every distinct package id in `feed` (original casing), ascending. Used by
    /// the retention sweep, which must visit packages search would not return.
    async fn all_package_ids(&self, feed: &str) -> Result<Vec<String>>;

    /// Aggregate counters for `feed`, for the statistics page.
    async fn stats(&self, feed: &str) -> Result<DatabaseStats>;

    /// The most recently published versions in `feed`, newest first.
    async fn recent_packages(&self, feed: &str, limit: i64) -> Result<Vec<Package>>;

    // --- symbols (global; keyed by SSQP signature) ---

    /// Record a symbol-file mapping: its SSQP `key`/`filename` and the owning
    /// package version (for cleanup on delete/retention).
    async fn add_symbol(
        &self,
        key: &str,
        filename: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<()>;

    /// Resolve a symbol file by its SSQP key and filename, returning the owning
    /// package's lower-cased id and normalized version when present.
    async fn find_symbol(&self, key: &str, filename: &str) -> Result<Option<SymbolRef>>;

    /// All symbol `(key, filename)` pairs owned by a package version.
    async fn find_symbols(&self, id: &str, version: &NuGetVersion) -> Result<Vec<SymbolKey>>;

    /// Remove all symbol mappings for a package version. Returns how many rows
    /// were removed.
    async fn delete_symbols(&self, id: &str, version: &NuGetVersion) -> Result<u64>;
}

/// Feed-wide aggregate statistics.
#[derive(Debug, Clone, Default)]
pub struct DatabaseStats {
    /// Distinct package ids.
    pub package_count: i64,
    /// Total package versions (rows).
    pub version_count: i64,
    /// Versions that are listed (visible in search).
    pub listed_count: i64,
    /// Sum of download counters across all versions.
    pub total_downloads: i64,
    /// Sum of on-disk `.nupkg` sizes, in bytes.
    pub total_size: i64,
    /// Number of indexed symbol files.
    pub symbol_count: i64,
}

/// A symbol file's owning package, resolved from an SSQP lookup.
#[derive(Debug, Clone)]
pub struct SymbolRef {
    pub lower_id: String,
    pub normalized_version: String,
}

/// The storage address of one symbol file.
#[derive(Debug, Clone)]
pub struct SymbolKey {
    pub key: String,
    pub filename: String,
}
