//! SQLite-backed [`PackageDatabase`].
//!
//! Nested metadata (authors, tags, package types, dependency groups) is stored
//! as JSON text columns, which keeps the schema small while remaining fully
//! queryable for the few fields search needs. Versions are stored both
//! normalized (the canonical key) and in their original form (so the exact
//! string the author pushed can be round-tripped), plus split numeric
//! components for fast SQL ordering of the version *core*. Pre-release ordering,
//! which SQL cannot express faithfully, is finished in Rust via
//! [`NuGetVersion`]'s `Ord`.
//!
//! Package metadata lives once in `packages`. Which feeds expose a version, and
//! that version's per-feed state (listed / enabled / pending / flagged), lives
//! in `feed_packages`. Feed-scoped reads join the two.
//!
//! The code is split by concern, one module per table or group of tables:
//!
//! | Module | Tables |
//! | --- | --- |
//! | `schema` | the schema and its migrations |
//! | `packages` | `packages`, `package_tags`: the global data of a version |
//! | `memberships` | `feed_packages`: a feed's own state for a version |
//! | `feeds` | feed-scoped reads joining the two: listings, stats |
//! | `search` | `search_text`, `search_keys`, `package_tags`: search, autocomplete, tags |
//! | `files` | `package_files` |
//! | `uploads` | `uploads` |
//! | `symbols` | `symbols` |
//! | `tombstones` | `tombstones` |
//!
//! [`PackageDatabase`] stays one trait, implemented here by delegating each
//! method to its module. Sub-traits would read better on paper, but a caller
//! holding the concrete [`SqliteDatabase`] (most tests) would then have to
//! import every one of them to call a method.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

use crate::error::{Error, Result};
use crate::models::Package;
use crate::version::NuGetVersion;

use super::{
    DatabaseStats, FeedVersion, Membership, MembershipChange, PackageDatabase, PackageFile,
    SearchPage, SearchRequest, SymbolKey, SymbolRef, TagCount, UploadSession, VersionFootprint,
};

/// The feed-scoped projection: every `packages` column plus the membership's
/// state aliased so it does not collide with the package's own template flags.
///
/// A macro rather than a `const` so call sites can `concat!` it into a whole
/// statement at compile time. sqlx only accepts a `&'static str` without an
/// explicit injection audit, and `format!`-ing a constant into a `String`
/// would forfeit that check to say nothing new.
macro_rules! feed_select {
    () => {
        "SELECT p.*, fp.listed AS m_listed, fp.enabled AS m_enabled, \
         fp.pending AS m_pending, fp.flagged AS m_flagged, fp.flag_reason AS m_flag_reason, \
         fp.downloads AS m_downloads, fp.pinned AS m_pinned \
         FROM packages p \
         JOIN feed_packages fp \
           ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version"
    };
}

mod feeds;
mod files;
mod memberships;
#[cfg(test)]
mod migration_tests;
mod packages;
mod schema;
mod search;
mod symbols;
mod tombstones;
mod uploads;

pub use search::MAX_QUERY_CHARS;

/// A SQLite package index.
#[derive(Debug, Clone)]
pub struct SqliteDatabase {
    pool: SqlitePool,
}

impl SqliteDatabase {
    /// Open (creating if needed) a SQLite database at `path` and run migrations.
    pub async fn connect(path: &str) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // `FULL`, not WAL's usual `NORMAL`: under `NORMAL` a power loss can
            // roll back a committed transaction, and deletes remove the payload
            // before the rows (see `retention::purge_global_data`). A rolled
            // back DELETE then brings back rows whose files are already gone.
            // The cost is one fsync of the WAL per commit, which the payload
            // writes around it dwarf.
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(30))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(16)
            .connect_with(options)
            .await?;
        Self::from_pool(pool).await
    }

    /// Build a fresh in-memory database (primarily for tests).
    pub async fn in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(Error::Database)?
            .foreign_keys(true);
        // A single, never-closed connection keeps the in-memory DB alive.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> Result<Self> {
        schema::migrate(&pool).await?;
        Ok(Self { pool })
    }

    /// Begin a transaction that writes, holding the write lock from the start.
    ///
    /// A deferred `BEGIN` takes a read snapshot at its first statement and
    /// upgrades it to a write lock only when it writes, and SQLite refuses that
    /// upgrade outright (`SQLITE_BUSY_SNAPSHOT`, no busy wait) if another
    /// connection committed in between. Preparing a statement that touches the
    /// FTS5 search index reads the index's own tables first, so concurrent
    /// pushes failed that way; `BEGIN IMMEDIATE` waits its turn instead.
    async fn write_tx(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }
}

#[async_trait]
impl PackageDatabase for SqliteDatabase {
    async fn ping(&self) -> Result<()> {
        // Touch a real table rather than `SELECT 1`, so a database file that has
        // vanished or been replaced by an empty one is reported as unhealthy.
        sqlx::query("SELECT COUNT(*) FROM packages LIMIT 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(())
    }

    // --- global package data: `packages` ---

    async fn upsert_package_data(&self, package: &Package) -> Result<bool> {
        packages::upsert_package_data(self, package).await
    }

    async fn package_data_exists(&self, id: &str, version: &NuGetVersion) -> Result<bool> {
        packages::package_data_exists(self, id, version).await
    }

    async fn get_package_data(&self, id: &str, version: &NuGetVersion) -> Result<Option<Package>> {
        packages::get_package_data(self, id, version).await
    }

    async fn delete_package_data(&self, id: &str, version: &NuGetVersion) -> Result<bool> {
        packages::delete_package_data(self, id, version).await
    }

    async fn feed_count(&self, id: &str, version: &NuGetVersion) -> Result<i64> {
        packages::feed_count(self, id, version).await
    }

    async fn add_version(&self, package: &Package, membership: &Membership) -> Result<bool> {
        packages::add_version(self, package, membership).await
    }

    async fn replace_version(&self, package: &Package, membership: &Membership) -> Result<()> {
        packages::replace_version(self, package, membership).await
    }

    async fn orphaned_versions(&self, limit: i64) -> Result<Vec<Package>> {
        packages::orphaned_versions(self, limit).await
    }

    // --- feed membership: `feed_packages` ---

    async fn add_membership(&self, membership: &Membership) -> Result<()> {
        memberships::add_membership(self, membership).await
    }

    async fn remove_membership(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<bool> {
        memberships::remove_membership(self, feed, id, version).await
    }

    async fn get_membership(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<Option<Membership>> {
        memberships::get_membership(self, feed, id, version).await
    }

    async fn approve_membership(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<bool> {
        memberships::approve_membership(self, feed, id, version).await
    }

    async fn exists(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<bool> {
        memberships::exists(self, feed, id, version).await
    }

    async fn set_listed(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
        listed: bool,
    ) -> Result<bool> {
        memberships::set_listed(self, feed, id, version, listed).await
    }

    async fn set_enabled(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
        enabled: bool,
    ) -> Result<bool> {
        memberships::set_enabled(self, feed, id, version, enabled).await
    }

    async fn set_pinned(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
        pinned: bool,
    ) -> Result<bool> {
        memberships::set_pinned(self, feed, id, version, pinned).await
    }

    async fn update_memberships(
        &self,
        feed: &str,
        id: &str,
        versions: &[NuGetVersion],
        change: MembershipChange,
    ) -> Result<u64> {
        memberships::update_memberships(self, feed, id, versions, change).await
    }

    async fn is_servable(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<bool> {
        memberships::is_servable(self, feed, id, version).await
    }

    async fn increment_downloads(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<()> {
        memberships::increment_downloads(self, feed, id, version).await
    }

    // --- feed-scoped reads ---

    async fn find(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<Option<Package>> {
        feeds::find(self, feed, id, version).await
    }

    async fn find_versions(
        &self,
        feed: &str,
        id: &str,
        include_unlisted: bool,
    ) -> Result<Vec<Package>> {
        feeds::find_versions(self, feed, id, include_unlisted).await
    }

    async fn find_all_versions(&self, feed: &str, id: &str) -> Result<Vec<FeedVersion>> {
        feeds::find_all_versions(self, feed, id).await
    }

    async fn find_all_versions_of(&self, feed: &str, ids: &[String]) -> Result<Vec<FeedVersion>> {
        feeds::find_all_versions_of(self, feed, ids).await
    }

    async fn version_footprints(&self, ids: &[String]) -> Result<Vec<VersionFootprint>> {
        feeds::version_footprints(self, ids).await
    }

    async fn all_package_ids(&self, feed: &str) -> Result<Vec<String>> {
        feeds::all_package_ids(self, feed).await
    }

    async fn stats(&self, feed: &str) -> Result<DatabaseStats> {
        feeds::stats(self, feed).await
    }

    async fn recent_packages(&self, feed: &str, limit: i64) -> Result<Vec<Package>> {
        feeds::recent_packages(self, feed, limit).await
    }

    // --- search ---

    async fn search(&self, feed: &str, request: &SearchRequest) -> Result<SearchPage> {
        search::search(self, feed, request).await
    }

    async fn autocomplete(
        &self,
        feed: &str,
        query: &str,
        include_prerelease: bool,
        include_semver2: bool,
        skip: i64,
        take: i64,
    ) -> Result<(Vec<String>, i64)> {
        search::autocomplete(
            self,
            feed,
            query,
            include_prerelease,
            include_semver2,
            skip,
            take,
        )
        .await
    }

    async fn tag_counts(&self, feed: &str, limit: i64) -> Result<Vec<TagCount>> {
        search::tag_counts(self, feed, limit).await
    }

    // --- attached files: `package_files` ---

    async fn add_file(&self, file: &PackageFile) -> Result<()> {
        files::add_file(self, file).await
    }

    async fn files_for(&self, id: &str, version: &NuGetVersion) -> Result<Vec<PackageFile>> {
        files::files_for(self, id, version).await
    }

    async fn files_for_id(&self, id: &str) -> Result<Vec<PackageFile>> {
        files::files_for_id(self, id).await
    }

    async fn get_file(
        &self,
        id: &str,
        version: &NuGetVersion,
        name: &str,
    ) -> Result<Option<PackageFile>> {
        files::get_file(self, id, version, name).await
    }

    async fn delete_file(
        &self,
        id: &str,
        version: &NuGetVersion,
        name: &str,
    ) -> Result<Option<PackageFile>> {
        files::delete_file(self, id, version, name).await
    }

    async fn blob_references(&self, sha256: &str) -> Result<i64> {
        files::blob_references(self, sha256).await
    }

    async fn increment_file_downloads(
        &self,
        id: &str,
        version: &NuGetVersion,
        name: &str,
    ) -> Result<()> {
        files::increment_file_downloads(self, id, version, name).await
    }

    // --- resumable uploads: `uploads` ---

    async fn create_upload(&self, upload: &UploadSession) -> Result<()> {
        uploads::create_upload(self, upload).await
    }

    async fn get_upload(&self, id: &str) -> Result<Option<UploadSession>> {
        uploads::get_upload(self, id).await
    }

    async fn set_upload_received(&self, id: &str, received: u64) -> Result<()> {
        uploads::set_upload_received(self, id, received).await
    }

    async fn delete_upload(&self, id: &str) -> Result<()> {
        uploads::delete_upload(self, id).await
    }

    async fn expired_uploads(&self, now: DateTime<Utc>) -> Result<Vec<UploadSession>> {
        uploads::expired_uploads(self, now).await
    }

    // --- symbols: `symbols` ---

    async fn add_symbol(
        &self,
        key: &str,
        filename: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<bool> {
        symbols::add_symbol(self, key, filename, id, version).await
    }

    async fn delete_symbol(&self, key: &str, filename: &str) -> Result<()> {
        symbols::delete_symbol(self, key, filename).await
    }

    async fn find_symbol(&self, key: &str, filename: &str) -> Result<Option<SymbolRef>> {
        symbols::find_symbol(self, key, filename).await
    }

    async fn find_symbols(&self, id: &str, version: &NuGetVersion) -> Result<Vec<SymbolKey>> {
        symbols::find_symbols(self, id, version).await
    }

    async fn delete_symbols(&self, id: &str, version: &NuGetVersion) -> Result<u64> {
        symbols::delete_symbols(self, id, version).await
    }

    // --- tombstones: `tombstones` ---

    async fn add_tombstone(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<()> {
        tombstones::add_tombstone(self, feed, id, version).await
    }

    async fn is_tombstoned(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<bool> {
        tombstones::is_tombstoned(self, feed, id, version).await
    }

    async fn clear_tombstone(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<()> {
        tombstones::clear_tombstone(self, feed, id, version).await
    }
}

/// How many ids one `IN (…)` list binds. Each is a parameter, and SQLite
/// builds before 3.32 cap those at 999; a single list of a thousand would be a
/// latent failure on exactly the deployments least able to debug it.
const ID_CHUNK: usize = 400;

/// `?first,?first+1,…`: `count` numbered parameters for an `IN (…)` list.
fn placeholders(first: usize, count: usize) -> String {
    (first..first + count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

fn json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| Error::Other(e.into()))
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| Error::Other(e.into()))
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .map(|d| d.is_unique_violation())
        .unwrap_or(false)
}

/// Read an RFC 3339 timestamp column.
fn parse_time(raw: &str, what: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| Error::Other(anyhow::anyhow!("bad {what} timestamp: {e}")))
}

/// What the tests of every module share.
#[cfg(test)]
pub(super) mod test_support {
    pub use super::SqliteDatabase;
    pub use crate::database::*;
    pub use crate::error::Error;
    pub use crate::models::{Package, PackageType};
    pub use crate::version::NuGetVersion;
    pub use chrono::Utc;

    pub const FEED: &str = "default";

    pub fn sample(id: &str, version: &str) -> Package {
        Package {
            id: id.to_string(),
            version: NuGetVersion::parse(version).unwrap(),
            listed: true,
            enabled: true,
            authors: vec!["Alice".into()],
            description: format!("description for {id}"),
            icon_url: None,
            license_url: None,
            license_expression: Some("MIT".into()),
            project_url: None,
            repository_url: None,
            repository_type: None,
            min_client_version: None,
            release_notes: None,
            language: None,
            title: None,
            summary: None,
            tags: vec!["sample".into()],
            has_readme: false,
            has_embedded_icon: false,
            is_development_dependency: false,
            require_license_acceptance: false,
            is_semver2: NuGetVersion::parse(version).unwrap().is_semver2(),
            package_size: 25_000_000_000, // 25 GB — exercises i64 sizing
            package_hash: "aGFzaA==".into(),
            package_hash_algorithm: "SHA512".into(),
            published: Utc::now(),
            downloads: 0,
            package_types: vec![],
            dependencies: vec![],
        }
    }

    pub fn tagged(id: &str, version: &str, tags: &[&str]) -> Package {
        let mut p = sample(id, version);
        p.tags = tags.iter().map(|t| t.to_string()).collect();
        p
    }

    pub fn query(q: &str) -> SearchRequest {
        SearchRequest {
            query: q.into(),
            ..Default::default()
        }
    }

    /// The ids a search in [`FEED`] finds, checking the count agrees.
    pub async fn hits(db: &SqliteDatabase, q: &str) -> Vec<String> {
        let page = db.search(FEED, &query(q)).await.unwrap();
        assert_eq!(page.total_hits as usize, page.groups.len(), "{q:?}");
        page.groups.iter().map(|g| g.latest().id.clone()).collect()
    }

    /// The pool, for tests that inspect or rewind the schema.
    pub fn pool(db: &SqliteDatabase) -> &sqlx::SqlitePool {
        &db.pool
    }
}
