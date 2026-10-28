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

/// The canonical, case-folded form of a package id: what every table stores
/// in `lower_id` and what every lookup binds.
///
/// ASCII-only on purpose. Storage paths and the per-version lock fold with
/// `to_ascii_lowercase`, and a valid id is ASCII anyway, so the database has
/// to agree with them rather than apply Unicode rules of its own. With
/// `to_lowercase` here, an id containing the Kelvin sign `K` (which folds to an
/// ASCII `k` under Unicode rules only) matched a row while missing both the
/// lock and the directory: a delete removed the rows, orphaned the payload and
/// was not serialised against a concurrent push of the same version.
pub fn canonical_id(id: &str) -> String {
    id.to_ascii_lowercase()
}

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
    /// The order of the page's package ids.
    pub sort: SearchSort,
    /// Only packages carrying this tag (case-insensitive, exact).
    pub tag: Option<String>,
    /// Load only each id's [headline](SearchGroup::headline) version and a
    /// count of the others, not every version. Enough for the gallery and the
    /// statistics page; `/v3/search` lists every version and needs them all.
    pub headlines_only: bool,
}

/// A file attached to a package version (a disk image, an archive).
///
/// Global like the package data itself: every feed holding the version holds
/// its files. The bytes live in the blob store under [`PackageFile::sha256`],
/// once however many versions share them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageFile {
    pub lower_id: String,
    pub normalized_version: String,
    /// The name as uploaded (validated; unique per version, ignoring case).
    pub name: String,
    /// Lower-case hex SHA-256 of the content: its blob name and its ETag.
    pub sha256: String,
    pub size: u64,
    pub uploaded: chrono::DateTime<chrono::Utc>,
    pub downloads: u64,
}

/// A digest of a version's attached files, names and contents, in any
/// order: what the admin page sends back with an approval, so the files can
/// be checked to be the ones it showed. No files has a digest too.
pub fn files_fingerprint<'a>(files: impl IntoIterator<Item = &'a PackageFile>) -> String {
    use sha2::Digest;
    let mut entries: Vec<(String, &str)> = files
        .into_iter()
        .map(|f| (f.name.to_lowercase(), f.sha256.as_str()))
        .collect();
    entries.sort_unstable();
    let mut hasher = sha2::Sha256::new();
    for (name, sha256) in entries {
        hasher.update(name.as_bytes());
        hasher.update([0]);
        hasher.update(sha256.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

/// An unfinished resumable (tus) upload of a [`PackageFile`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadSession {
    /// Random, unguessable id; also names the partial file.
    pub id: String,
    /// The feed it was started in, whose push key it answers to.
    pub feed: String,
    pub lower_id: String,
    pub normalized_version: String,
    pub name: String,
    /// The declared total size.
    pub length: u64,
    /// Bytes received so far.
    pub received: u64,
    /// The SHA-256 (hex) the finished file must have, if the client said.
    pub expected_sha256: Option<String>,
    pub expires: chrono::DateTime<chrono::Utc>,
}

/// How many packages of a feed carry one tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagCount {
    /// The tag, lower-cased.
    pub tag: String,
    /// Distinct package ids (not versions) with a visible version carrying it.
    pub packages: i64,
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
            sort: SearchSort::default(),
            tag: None,
            headlines_only: false,
        }
    }
}

/// The order search results come back in.
///
/// Only the gallery chooses one. `/v3/search` always asks for the default,
/// which is the ranking NuGet clients have always been given here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchSort {
    /// Most downloads (across every matching version) first.
    #[default]
    Downloads,
    /// By package id, A to Z.
    Name,
    /// The package whose newest matching version was published last, first.
    Updated,
}

impl SearchSort {
    /// Parse the gallery's `sort` value: `downloads`, `name` or `updated`,
    /// case-insensitively.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "downloads" => Some(Self::Downloads),
            "name" => Some(Self::Name),
            "updated" => Some(Self::Updated),
            _ => None,
        }
    }

    /// The value [`Self::parse`] reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Downloads => "downloads",
            Self::Name => "name",
            Self::Updated => "updated",
        }
    }
}

/// One search hit: every (matching) version of a single package id, already
/// sorted ascending by version — or, for a
/// [`headlines_only`](SearchRequest::headlines_only) search, just the headline.
#[derive(Debug, Clone)]
pub struct SearchGroup {
    pub packages: Vec<Package>,
    /// How many versions the search admitted. `packages.len()` unless only
    /// the headline was loaded.
    pub version_count: u64,
    /// Downloads of every version of the id this feed serves, whether or not
    /// the search's filters (pre-release, SemVer2, listed) admitted it: the
    /// package's total, as `totalDownloads` means on nuget.org.
    pub total_downloads: u64,
}

impl SearchGroup {
    /// The version chosen to represent the group (the highest one).
    pub fn latest(&self) -> &Package {
        self.packages
            .last()
            .expect("a search group always has at least one package")
    }

    /// The version to headline: the newest stable one, or the newest of any
    /// kind when every version is a pre-release.
    ///
    /// This is what a person should be shown and offered an install command
    /// for. [`Self::latest`] includes pre-releases, so using it made the gallery
    /// headline `2.0.0-beta` while a NuGet client searching the same feed —
    /// which excludes pre-releases unless asked — offered `1.9.0`.
    pub fn headline(&self) -> &Package {
        self.packages
            .iter()
            .rev()
            .find(|p| !p.is_prerelease())
            .unwrap_or_else(|| self.latest())
    }

    /// How many versions the search admitted (the `version_count` field);
    /// never less than the versions loaded, so a group built without it still
    /// counts.
    pub fn version_count(&self) -> u64 {
        self.version_count.max(self.packages.len() as u64)
    }

    /// Total downloads of the package, across all its versions in the feed
    /// (the `total_downloads` field); never less than what the matching
    /// versions alone add up to, so a group built without it still counts.
    pub fn total_downloads(&self) -> u64 {
        let shown: u64 = self.packages.iter().map(|p| p.downloads).sum();
        self.total_downloads.max(shown)
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
    /// Kept by retention whatever its rules say. Affects nothing else: an
    /// explicit delete still removes a pinned version.
    pub pinned: bool,
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
            pinned: false,
        }
    }
}

/// A flag change applied to many memberships at once by
/// [`PackageDatabase::update_memberships`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MembershipChange {
    /// Set the admin `enabled` flag.
    Enabled(bool),
    /// Set the retention pin.
    Pinned(bool),
    /// Clear the pending flag.
    Approve,
}

/// Which packages the admin dashboard lists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AdminFilter {
    #[default]
    All,
    /// Those with a version awaiting approval.
    Pending,
    /// Those with a version a policy check flagged.
    Flagged,
}

impl AdminFilter {
    /// Read a `?filter=` value; `None` for anything else.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "all" => Some(Self::All),
            "pending" => Some(Self::Pending),
            "flagged" => Some(Self::Flagged),
            _ => None,
        }
    }

    /// The value [`Self::parse`] reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Pending => "pending",
            Self::Flagged => "flagged",
        }
    }
}

/// One package on the admin dashboard: how many versions the feed holds,
/// whatever their state, and how many of them are pending or flagged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminPackageSummary {
    /// The id as published.
    pub id: String,
    pub versions: i64,
    pub pending: i64,
    pub flagged: i64,
}

/// A page of the admin dashboard, and how many packages each filter has.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdminOverview {
    pub packages: Vec<AdminPackageSummary>,
    /// Every package id the feed holds.
    pub total: i64,
    /// Ids with at least one pending version.
    pub pending: i64,
    /// Ids with at least one flagged version.
    pub flagged: i64,
}

impl AdminOverview {
    /// How many packages `filter` admits: what its page is one page of.
    pub fn matching(&self, filter: AdminFilter) -> i64 {
        match filter {
            AdminFilter::All => self.total,
            AdminFilter::Pending => self.pending,
            AdminFilter::Flagged => self.flagged,
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
    /// Exempt from retention (see [`Membership::pinned`]).
    pub pinned: bool,
    /// When the version entered this feed: pushed, promoted, copied, moved or
    /// mirrored in, or last overwritten. Not `package.published`, which is
    /// global and older for a version promoted later.
    pub added: chrono::DateTime<chrono::Utc>,
}

/// Metadata store for indexed packages.
#[async_trait]
pub trait PackageDatabase: Send + Sync {
    /// Cheap round-trip proving the store is reachable and answering.
    ///
    /// Backs the readiness probe: a server whose database file has been deleted
    /// or whose volume was unmounted still accepts connections and still serves
    /// a static "OK", which is exactly the failure an orchestrator needs to see.
    async fn ping(&self) -> Result<()>;

    // --- global package data (shared by every feed) ---

    /// Insert global package metadata if absent. Idempotent: returns `true` when
    /// a new row was written, `false` when the version already existed (e.g. it
    /// was already pushed to another feed).
    async fn upsert_package_data(&self, package: &Package) -> Result<bool>;

    /// Whether global metadata exists for this id/version, regardless of feed.
    async fn package_data_exists(&self, id: &str, version: &NuGetVersion) -> Result<bool>;

    /// Fetch global package metadata, ignoring feed membership and visibility.
    ///
    /// Listing, the admin flag and download counts belong to each feed's
    /// membership, so the package comes back listed, enabled and with no
    /// downloads; read a feed for those.
    async fn get_package_data(&self, id: &str, version: &NuGetVersion) -> Result<Option<Package>>;

    /// Hard-delete global metadata and everything recorded against the
    /// version: every feed membership, its tags, attached-file rows and symbol
    /// mappings. Returns `true` if a row was removed. The caller deletes the
    /// stored bytes those rows point at first (payload, file blobs, PDBs).
    async fn delete_package_data(&self, id: &str, version: &NuGetVersion) -> Result<bool>;

    /// How many feeds currently contain this version.
    async fn feed_count(&self, id: &str, version: &NuGetVersion) -> Result<i64>;

    /// Insert the global data (if absent) and a membership, in one
    /// transaction: either both are recorded or neither is. Returns whether the
    /// global data was new, like [`Self::upsert_package_data`];
    /// [`Error::PackageAlreadyExists`](crate::error::Error::PackageAlreadyExists)
    /// when the version is already a member of the feed.
    async fn add_version(&self, package: &Package, membership: &Membership) -> Result<bool>;

    /// Replace a version's global data with a new build's, and set the
    /// membership's state, in one transaction. What hangs off the version
    /// rather than off one build of it stays: attached files, every other
    /// feed's membership, and this membership's download count.
    async fn replace_version(&self, package: &Package, membership: &Membership) -> Result<()>;

    /// [`Self::replace_version`] for `allow_overwrite = "pending-only"`:
    /// refused with [`Error::VersionExists`](crate::error::Error::VersionExists)
    /// when any feed has approved the version, checked in the same transaction
    /// as the replacement, so a concurrent approval is either seen or waits.
    /// With `drop_files`, the version's attached-file rows are deleted in that
    /// transaction too and returned, for the caller to delete the blobs nothing
    /// else references.
    ///
    /// `others` are the other feeds' license verdicts on the new build: a feed
    /// holding the version whose verdict blocks it refuses the replacement
    /// with [`Error::PolicyViolation`](crate::error::Error::PolicyViolation),
    /// and every other feed listed there has its flag set from its verdict.
    async fn replace_pending_version(
        &self,
        package: &Package,
        membership: &Membership,
        drop_files: bool,
        others: &[(String, crate::policy::PolicyOutcome)],
    ) -> Result<Vec<PackageFile>>;

    /// The feeds in which a version is approved (held and not pending).
    async fn approved_in(&self, id: &str, version: &NuGetVersion) -> Result<Vec<String>>;

    /// Up to `limit` versions whose global data no feed holds any more: what a
    /// purge that failed part-way leaves behind.
    async fn orphaned_versions(&self, limit: i64) -> Result<Vec<Package>>;

    // --- feed membership ---

    /// Add a membership. Returns
    /// [`Error::PackageAlreadyExists`](crate::error::Error::PackageAlreadyExists)
    /// when the version is already a member of the feed.
    async fn add_membership(&self, membership: &Membership) -> Result<()>;

    /// Remove a membership. Returns `true` if a row was removed.
    async fn remove_membership(&self, feed: &str, id: &str, version: &NuGetVersion)
        -> Result<bool>;

    /// Remove a membership on purpose (moved out, pruned) and, when a row was
    /// removed, record its tombstone, in one transaction: either both happen
    /// or neither does, so a read-through mirror never finds the version gone
    /// without the tombstone that keeps it from fetching it back. Returns
    /// `true` if a row was removed.
    async fn remove_membership_for_good(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<bool>;

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

    /// Set the listed flag for a membership by hand (an unlist or relist), so
    /// a mirror no longer follows its upstream's listing for it. Returns
    /// `true` if a row was updated.
    async fn set_listed(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
        listed: bool,
    ) -> Result<bool>;

    /// Set the listed flag as a mirror's upstream has it, unless it was set
    /// by hand ([`Self::set_listed`]). Returns `true` if it changed.
    async fn follow_upstream_listing(
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

    /// Set the retention pin on a membership. Returns `true` if a row was
    /// updated.
    async fn set_pinned(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
        pinned: bool,
    ) -> Result<bool>;

    /// Apply one flag change to several versions of `id` in `feed`, in a
    /// single transaction: every row changes, or — if any is missing or a
    /// write fails — none does. Returns how many rows were updated.
    async fn update_memberships(
        &self,
        feed: &str,
        id: &str,
        versions: &[NuGetVersion],
        change: MembershipChange,
    ) -> Result<u64>;

    /// Whether a version may be served from `feed`: present, enabled and not
    /// pending. (Unlisted-but-enabled versions are still servable by version.)
    async fn is_servable(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<bool>;

    /// Every version of a package id in `feed` — including unlisted, disabled
    /// **and pending** — sorted ascending. For admin views and retention.
    async fn find_all_versions(&self, feed: &str, id: &str) -> Result<Vec<FeedVersion>>;

    /// [`Self::find_all_versions`] for many ids at once (lower-cased), in a
    /// few statements rather than one per id. Grouped by id in no particular
    /// order, each id's versions ascending.
    async fn find_all_versions_of(&self, feed: &str, ids: &[String]) -> Result<Vec<FeedVersion>>;

    /// For every version of the given ids (lower-cased): how many feeds hold
    /// it and how many bytes of attached files it has. What deleting it would
    /// free, for many versions in two statements.
    async fn version_footprints(&self, ids: &[String]) -> Result<Vec<VersionFootprint>>;

    /// Atomically increment the per-feed download counter for a version.
    async fn increment_downloads(&self, feed: &str, id: &str, version: &NuGetVersion)
        -> Result<()>;

    /// Execute a search query within `feed`.
    async fn search(&self, feed: &str, request: &SearchRequest) -> Result<SearchPage>;

    /// Autocomplete package ids in `feed` by prefix/substring.
    /// Package ids matching `query`, for the autocomplete service.
    ///
    /// `include_prerelease`/`include_semver2` mirror the search filters: an id
    /// whose only versions are excluded by them must not be suggested, or the
    /// caller is pointed at a package it will then find nothing in.
    ///
    /// Returns the requested page and the total number of matching ids. The
    /// total is what a caller pages on, so it must count every match rather
    /// than the page — reporting the page size stops a client at the first
    /// page and hides everything after it.
    async fn autocomplete(
        &self,
        feed: &str,
        query: &str,
        include_prerelease: bool,
        include_semver2: bool,
        skip: i64,
        take: i64,
    ) -> Result<(Vec<String>, i64)>;

    /// Every distinct package id in `feed` (original casing), ascending. Used by
    /// the retention sweep, which must visit packages search would not return.
    async fn all_package_ids(&self, feed: &str) -> Result<Vec<String>>;

    /// A page of `feed`'s packages for the admin dashboard, counting every
    /// version (pending, disabled and flagged ones too): those with a pending
    /// version first, then those with a flagged one, then the rest, each part
    /// by id. The counts cover the whole feed, not the page.
    async fn admin_overview(
        &self,
        feed: &str,
        filter: AdminFilter,
        skip: i64,
        take: i64,
    ) -> Result<AdminOverview>;

    /// Aggregate counters for `feed`, for the statistics page.
    async fn stats(&self, feed: &str) -> Result<DatabaseStats>;

    /// The most recently published versions in `feed`, newest first.
    async fn recent_packages(&self, feed: &str, limit: i64) -> Result<Vec<Package>>;

    /// The tags of `feed`'s visible (listed, enabled, approved) versions with
    /// how many packages carry each, most used first, at most `limit`.
    async fn tag_counts(&self, feed: &str, limit: i64) -> Result<Vec<TagCount>>;

    // --- files attached to versions (global, like package data) ---

    /// Attach a file. [`Error::PackageAlreadyExists`](crate::error::Error::PackageAlreadyExists)
    /// when the version already has a file of that name (ignoring case).
    async fn add_file(&self, file: &PackageFile) -> Result<()>;

    /// A version's files, by name.
    async fn files_for(&self, id: &str, version: &NuGetVersion) -> Result<Vec<PackageFile>>;

    /// The files of every version of an id, by version then name.
    async fn files_for_id(&self, id: &str) -> Result<Vec<PackageFile>>;

    /// One of a version's files, by name (ignoring case).
    async fn get_file(
        &self,
        id: &str,
        version: &NuGetVersion,
        name: &str,
    ) -> Result<Option<PackageFile>>;

    /// Detach a file, returning what was detached.
    async fn delete_file(
        &self,
        id: &str,
        version: &NuGetVersion,
        name: &str,
    ) -> Result<Option<PackageFile>>;

    /// How many attached files, across all versions, reference a blob.
    async fn blob_references(&self, sha256: &str) -> Result<i64>;

    /// Count one download of a file.
    async fn increment_file_downloads(
        &self,
        id: &str,
        version: &NuGetVersion,
        name: &str,
    ) -> Result<()>;

    // --- resumable uploads ---

    async fn create_upload(&self, upload: &UploadSession) -> Result<()>;
    async fn get_upload(&self, id: &str) -> Result<Option<UploadSession>>;
    async fn set_upload_received(&self, id: &str, received: u64) -> Result<()>;
    async fn delete_upload(&self, id: &str) -> Result<()>;
    /// The bytes the uploads current at `now` have still to receive, over
    /// every feed: space they were admitted with and have not used yet.
    async fn outstanding_upload_bytes(&self, now: chrono::DateTime<chrono::Utc>) -> Result<u64>;
    /// How many uploads of `feed` are current (not expired) at `now`.
    async fn open_uploads(&self, feed: &str, now: chrono::DateTime<chrono::Utc>) -> Result<u64>;
    /// Uploads whose expiry has passed at `now`.
    async fn expired_uploads(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<UploadSession>>;

    // --- symbols (global; keyed by SSQP signature) ---

    /// Claim a symbol-file mapping: its SSQP `key`/`filename` and the owning
    /// package version (for serving and for cleanup on delete/retention).
    /// Returns `true` when this call created it. An existing mapping is never
    /// changed — a key belongs to whichever version claimed it first — so
    /// `false` means someone else holds it.
    async fn add_symbol(
        &self,
        key: &str,
        filename: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<bool>;

    /// Remove one symbol-file mapping (undoing a claim whose push failed).
    async fn delete_symbol(&self, key: &str, filename: &str) -> Result<()>;

    /// Resolve a symbol file by its SSQP key and filename, returning the owning
    /// package's lower-cased id and normalized version when present.
    async fn find_symbol(&self, key: &str, filename: &str) -> Result<Option<SymbolRef>>;

    /// All symbol `(key, filename)` pairs owned by a package version.
    async fn find_symbols(&self, id: &str, version: &NuGetVersion) -> Result<Vec<SymbolKey>>;

    /// Remove all symbol mappings for a package version. Returns how many rows
    /// were removed.
    async fn delete_symbols(&self, id: &str, version: &NuGetVersion) -> Result<u64>;

    // --- tombstones (per feed) ---

    /// Record that a version was deliberately removed from `feed` (deleted,
    /// pruned or moved out), so a read-through mirror never fetches it back.
    async fn add_tombstone(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<()>;

    /// Whether a version was removed from `feed` and not pushed again since.
    async fn is_tombstoned(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<bool>;

    /// Forget a tombstone: a push of the version brings it back on purpose.
    async fn clear_tombstone(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<()>;

    // --- feeds as a whole ---

    /// Every feed name the database holds memberships or tombstones for,
    /// configured or not, by name.
    async fn stored_feeds(&self) -> Result<Vec<StoredFeed>>;

    /// How many versions both feeds hold.
    async fn shared_versions(&self, a: &str, b: &str) -> Result<i64>;

    /// Give everything recorded under the feed name `from` to `to`: its
    /// memberships, tombstones and unfinished uploads, in one transaction.
    /// Refused with [`Error::BadRequest`](crate::error::Error::BadRequest)
    /// when `to` already holds one of `from`'s versions, since the two
    /// memberships' states cannot both be kept. Returns how many memberships
    /// moved.
    async fn rename_feed(&self, from: &str, to: &str) -> Result<u64>;

    /// Every version `feed` holds, in any state, as (lower-cased id,
    /// version).
    async fn feed_members(&self, feed: &str) -> Result<Vec<(String, NuGetVersion)>>;

    /// Drop every tombstone of `feed`. Returns how many went.
    async fn clear_tombstones(&self, feed: &str) -> Result<u64>;
}

/// What the database holds for one feed name, from
/// [`PackageDatabase::stored_feeds`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFeed {
    pub name: String,
    /// Versions it holds, in any state.
    pub versions: i64,
    /// Of those, the ones no other feed holds: removing the feed's
    /// memberships frees their payload.
    pub exclusive: i64,
    /// Versions deleted from it.
    pub tombstones: i64,
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
    /// Files attached to the feed's visible versions.
    pub file_count: i64,
    /// Their total size, in bytes.
    pub file_bytes: i64,
}

/// What one version occupies beyond its own membership, from
/// [`PackageDatabase::version_footprints`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionFootprint {
    pub lower_id: String,
    pub normalized_version: String,
    /// Feeds holding the version; its payload is freed only when this is 1.
    pub feeds: i64,
    /// Total size of its attached files, in bytes.
    pub file_bytes: u64,
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

/// The refusal for replacing a version that some feed has approved.
pub fn approved_refusal(p: &Package, feeds: &[String]) -> String {
    format!(
        "{} {} is approved in {}, and allow_overwrite = \"pending-only\" replaces \
         a version only while it is pending in every feed that holds it. Push it as \
         a new version.",
        p.id,
        p.normalized_version(),
        feeds.join(", ")
    )
}
