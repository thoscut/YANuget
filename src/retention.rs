//! Package retention: automatic pruning of old versions.
//!
//! The decision of *which* versions to drop is a pure, side-effect-free
//! function ([`prune_plan`]) so it can be exhaustively unit-tested; the I/O
//! that acts on that decision ([`prune_package`], [`prune_all`], [`preview`])
//! is a thin wrapper over the existing storage/database primitives.
//!
//! A version is pruned when it is **excess by count** (beyond the newest N of
//! its release channel) **or** **too old** (published before the age cutoff).
//! As a safety floor the newest stable version is always kept — or, when a
//! package has no stable version, its newest pre-release — so retention can
//! never make a package disappear entirely. A **pinned** version is never
//! pruned, and does not use up one of the "newest N" either: a pin is kept in
//! addition to what the rules keep.
//!
//! The rules only see what clients can see. A **pending** or **disabled**
//! version is outside them like a pin: never pruned, never ranked, and never
//! the "newest" that is kept. Otherwise pushing N builds into a gated feed
//! deleted every approved version, and rejecting the builds then left nothing;
//! disabling a broken newest version did the same by accident.

use chrono::{DateTime, Duration, Utc};

use crate::config::RetentionConfig;
use crate::database::{canonical_id, FeedVersion, PackageDatabase};
use crate::error::Result;
use crate::models::Package;
use crate::storage::PackageStorage;
use crate::version::NuGetVersion;

/// The evaluated retention rules (a snapshot of [`RetentionConfig`]).
#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    pub keep_latest_stable: Option<usize>,
    pub keep_latest_prerelease: Option<usize>,
    pub max_age_days: Option<u64>,
}

impl RetentionPolicy {
    /// Whether any limit is set; with none, pruning is a no-op.
    pub fn has_limits(&self) -> bool {
        self.keep_latest_stable.is_some()
            || self.keep_latest_prerelease.is_some()
            || self.max_age_days.is_some()
    }
}

impl From<&RetentionConfig> for RetentionPolicy {
    fn from(c: &RetentionConfig) -> Self {
        Self {
            keep_latest_stable: c.keep_latest_stable,
            keep_latest_prerelease: c.keep_latest_prerelease,
            max_age_days: c.max_age_days,
        }
    }
}

/// Why a version is pruned. Both halves can hold at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneReason {
    /// Beyond the newest `keep` of its channel; `prerelease` names the channel.
    pub beyond_newest: Option<(usize, bool)>,
    /// Published more than this many days ago.
    pub older_than_days: Option<u64>,
}

impl PruneReason {
    /// The reason in words, e.g. "beyond the newest 5 stable versions, and
    /// older than 90 days".
    pub fn describe(&self) -> String {
        let count = self.beyond_newest.map(|(keep, prerelease)| {
            let channel = if prerelease { "pre-release" } else { "stable" };
            let noun = if keep == 1 { "version" } else { "versions" };
            format!("beyond the newest {keep} {channel} {noun}")
        });
        let age = self
            .older_than_days
            .map(|days| format!("older than {days} day{}", if days == 1 { "" } else { "s" }));
        match (count, age) {
            (Some(c), Some(a)) => format!("{c}, and {a}"),
            (Some(c), None) => c,
            (None, Some(a)) => a,
            (None, None) => String::new(),
        }
    }
}

/// One version a policy would prune, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pruned {
    pub version: NuGetVersion,
    pub reason: PruneReason,
}

/// One version as retention sees it.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub package: &'a Package,
    /// Kept whatever the rules say (see [`Membership::pinned`](crate::database::Membership::pinned)).
    pub pinned: bool,
    /// Enabled and approved: what a client can download. Unlisted versions
    /// count, because a client restoring an exact version still gets them —
    /// they are hidden from search, not withdrawn.
    pub servable: bool,
}

impl<'a> Candidate<'a> {
    /// A servable, unpinned version.
    pub fn ranked(package: &'a Package) -> Self {
        Self {
            package,
            pinned: false,
            servable: true,
        }
    }
}

/// Decide which versions of a single package id to prune, and why.
///
/// `versions` may be in any order. Pinned and unservable versions are set
/// aside *before* ranking, so they are never pruned and never count towards
/// "the newest N". `now` is injected for deterministic testing.
pub fn prune_plan(
    versions: &[Candidate<'_>],
    policy: &RetentionPolicy,
    now: DateTime<Utc>,
) -> Vec<Pruned> {
    if versions.is_empty() || !policy.has_limits() {
        return Vec::new();
    }

    // The single version that must survive no matter what, chosen among the
    // servable ones, pinned or not: the package keeps its newest version a
    // client can actually get. A pending build does not count — it may never
    // be approved.
    let newest = |pre: bool| {
        versions
            .iter()
            .filter(|c| c.servable && c.package.is_prerelease() == pre)
            .map(|c| &c.package.version)
            .max()
    };
    let protected: Option<&NuGetVersion> = newest(false).or_else(|| newest(true));

    // Newest-first within each channel so "rank" is an index from the top.
    let ranked = versions
        .iter()
        .filter(|c| c.servable && !c.pinned)
        .map(|c| c.package);
    let mut stable: Vec<&Package> = ranked.clone().filter(|p| !p.is_prerelease()).collect();
    let mut prerelease: Vec<&Package> = ranked.filter(|p| p.is_prerelease()).collect();
    stable.sort_by(|a, b| b.version.cmp(&a.version));
    prerelease.sort_by(|a, b| b.version.cmp(&a.version));

    // `Duration::days` panics outside its representable range, and a `u64` day
    // count from configuration can easily exceed it — so build the cutoff with
    // checked arithmetic. An unrepresentably distant cutoff means "never too
    // old", which is the safe reading: it prunes nothing rather than, via a
    // wrapped negative duration, treating every version as expired.
    let cutoff = policy
        .max_age_days
        .and_then(|d| i64::try_from(d).ok())
        .and_then(Duration::try_days)
        .and_then(|d| now.checked_sub_signed(d));

    let mut prune = Vec::new();
    for (channel, keep, is_pre) in [
        (&stable, policy.keep_latest_stable, false),
        (&prerelease, policy.keep_latest_prerelease, true),
    ] {
        for (rank, pkg) in channel.iter().enumerate() {
            if protected.is_some_and(|v| *v == pkg.version) {
                continue;
            }
            let reason = PruneReason {
                beyond_newest: keep.filter(|n| rank >= *n).map(|n| (n, is_pre)),
                older_than_days: cutoff
                    .filter(|c| pkg.published < *c)
                    .and(policy.max_age_days),
            };
            if reason.beyond_newest.is_some() || reason.older_than_days.is_some() {
                prune.push(Pruned {
                    version: pkg.version.clone(),
                    reason,
                });
            }
        }
    }
    prune
}

/// Decide which versions of a single package id to prune, none pinned.
///
/// `packages` may be in any order; the returned versions are those that should
/// be removed. `now` is injected for deterministic testing.
pub fn versions_to_prune(
    packages: &[Package],
    policy: &RetentionPolicy,
    now: DateTime<Utc>,
) -> Vec<NuGetVersion> {
    let versions: Vec<Candidate<'_>> = packages.iter().map(Candidate::ranked).collect();
    prune_plan(&versions, policy, now)
        .into_iter()
        .map(|p| p.version)
        .collect()
}

/// [`prune_plan`] over a feed's versions of one package, with their pins and
/// their pending and disabled states.
pub fn plan_for(
    versions: &[FeedVersion],
    policy: &RetentionPolicy,
    now: DateTime<Utc>,
) -> Vec<Pruned> {
    let candidates: Vec<Candidate<'_>> = versions.iter().map(candidate).collect();
    prune_plan(&candidates, policy, now)
}

fn candidate(v: &FeedVersion) -> Candidate<'_> {
    Candidate {
        package: &v.package,
        pinned: v.pinned,
        servable: is_servable(v),
    }
}

/// Enabled and approved. `FeedVersion::package.enabled` is the membership's.
fn is_servable(v: &FeedVersion) -> bool {
    v.package.enabled && !v.pending
}

/// Remove one package version from a single feed.
///
/// The feed's membership is dropped. When that was the **last** feed referencing
/// the version, the now-orphaned global data is hard-deleted too: its symbol
/// files and mappings, its stored payload/sidecars, and its `packages` row. A
/// version still referenced by another feed is left fully intact.
///
/// Used by both the retention sweep and the API's hard-delete path so symbol
/// cleanup is never forgotten.
pub async fn purge_version(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    // Serialize against a concurrent push of the same version into another feed,
    // so the feed-count check and global GC see a consistent snapshot.
    let _guard = crate::locks::lock_version(id, &version.normalized()).await;
    purge_locked(storage, db, feed, id, version).await
}

/// [`purge_version`] for retention: the same, unless the version was pinned,
/// or stopped being servable, since the plan that chose it was made.
///
/// A cleanup plans every package first and deletes afterwards, and a sweep of
/// a large feed takes a while; an admin pinning a version in the meantime
/// must win. The check is under the version lock, so it cannot go stale again
/// before the delete.
async fn purge_planned(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let _guard = crate::locks::lock_version(id, &version.normalized()).await;
    let Some(membership) = db.get_membership(feed, id, version).await? else {
        return Ok(false);
    };
    if membership.pinned || membership.pending || !membership.enabled {
        tracing::info!(%feed, %id, version = %version.normalized(), "retention kept a version pinned or withheld since it was planned");
        return Ok(false);
    }
    purge_locked(storage, db, feed, id, version).await
}

/// The body of [`purge_version`]; the caller holds the version lock.
async fn purge_locked(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let removed = db.remove_membership(feed, id, version).await?;
    if db.feed_count(id, version).await? == 0 {
        purge_global_data(storage, db, id, version).await?;
    }
    Ok(removed)
}

/// Finish off versions whose global data no feed holds: what a purge that
/// failed after removing the last membership leaves behind. Nothing serves
/// them, and nothing else would ever revisit them. Returns how many were
/// removed.
///
/// Each is re-checked under its version lock, so a push adopting the version
/// at the same moment wins.
pub async fn sweep_orphans(storage: &dyn PackageStorage, db: &dyn PackageDatabase) -> usize {
    const BATCH: i64 = 500;
    let mut removed = 0;
    let orphans = match db.orphaned_versions(BATCH).await {
        Ok(orphans) => orphans,
        Err(e) => {
            tracing::error!(error = %e, "orphan sweep could not list orphaned versions");
            return 0;
        }
    };
    for package in orphans {
        let (id, version) = (&package.id, &package.version);
        let _guard = crate::locks::lock_version(id, &version.normalized()).await;
        let still = match db.feed_count(id, version).await {
            Ok(0) => db.package_data_exists(id, version).await.unwrap_or(false),
            _ => false,
        };
        if !still {
            continue;
        }
        match purge_global_data(storage, db, id, version).await {
            Ok(()) => {
                removed += 1;
                tracing::info!(%id, version = %version.normalized(), "removed a version no feed holds");
            }
            Err(e) => {
                tracing::error!(%id, version = %version.normalized(), error = %e, "orphan sweep failed to remove version");
            }
        }
    }
    removed
}

/// Hard-delete the data shared by every feed: symbol files and rows, the stored
/// payload and sidecars, and the `packages` row.
///
/// **The caller must already hold the version lock** — this does not take it, so
/// that code which is mid-way through a locked sequence (an overwriting push)
/// can reuse it without deadlocking.
///
/// The order is chosen for recoverability. Files go first and rows last, because
/// the rows are the only index of what there is to delete: aborting with the
/// rows intact leaves a state a retry can finish, whereas deleting the rows
/// first and then failing on the files orphans bytes nothing will ever find
/// again. Every step propagates its error rather than being discarded — the
/// previous version reported success after silently failing to delete anything,
/// so a read-only mount or a permissions change looked exactly like a clean
/// sweep.
pub(crate) async fn purge_global_data(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<()> {
    purge_symbols(storage, db, id, version).await?;
    purge_file_blobs(storage, db, id, version).await?;
    storage.delete(id, &version.normalized()).await?;
    db.delete_package_data(id, version).await?;
    Ok(())
}

/// Delete the blobs of a version's attached files that no other version also
/// references. Their rows go with the package data, after this, so a failure
/// here leaves the rows that say what is left to delete.
///
/// Same contract: the caller must already hold the version lock. Each blob's
/// own lock is taken here, across the count and the delete, since another
/// version may be attaching the same bytes right now.
pub(crate) async fn purge_file_blobs(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<()> {
    let files = db.files_for(id, version).await?;
    let mut blobs: Vec<&str> = files.iter().map(|f| f.sha256.as_str()).collect();
    blobs.sort_unstable();
    blobs.dedup();
    for sha in blobs {
        let here = files.iter().filter(|f| f.sha256 == sha).count() as i64;
        let _blob = crate::locks::lock_blob(sha).await;
        if db.blob_references(sha).await? <= here {
            storage.delete_blob(sha).await?;
        }
    }
    Ok(())
}

/// Drop every symbol file and mapping belonging to one version, leaving the
/// package itself alone.
///
/// Separate from [`purge_global_data`] because an overwriting push needs exactly
/// this and nothing else: the replacement build's PDBs have different SSQP keys,
/// so the old mappings would otherwise survive and — since they still resolve to
/// an id/version that exists — keep serving the *previous* build's PDBs to
/// anyone debugging the new one.
///
/// Same contract: the caller must already hold the version lock.
pub(crate) async fn purge_symbols(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<()> {
    for sym in db.find_symbols(id, version).await? {
        storage.delete_symbol(&sym.key, &sym.filename).await?;
    }
    db.delete_symbols(id, version).await?;
    Ok(())
}

/// What deleting some versions achieved.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Versions removed from the feed.
    pub deleted: usize,
    /// Bytes of storage freed: only versions no other feed still holds.
    pub freed: u64,
    /// Versions whose removal failed (logged).
    pub errors: usize,
}

/// The size of a version's attached files: what deleting it frees on top of
/// the package, at most (a blob another version shares stays).
async fn attached_bytes(db: &dyn PackageDatabase, id: &str, version: &NuGetVersion) -> u64 {
    db.files_for(id, version)
        .await
        .map(|files| files.iter().map(|f| f.size).sum())
        .unwrap_or(0)
}

/// Delete one planned version from `feed`, counting what it freed.
async fn delete_planned(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
    outcome: &mut Outcome,
) {
    let size = db
        .get_package_data(id, version)
        .await
        .ok()
        .flatten()
        .map(|p| p.package_size)
        .unwrap_or(0)
        + attached_bytes(db, id, version).await;
    match purge_planned(storage, db, feed, id, version).await {
        Ok(true) => {
            outcome.deleted += 1;
            if !db.package_data_exists(id, version).await.unwrap_or(true) {
                outcome.freed += size;
            }
            tracing::info!(%feed, %id, version = %version.normalized(), "retention pruned version");
        }
        Ok(false) => {}
        Err(e) => {
            outcome.errors += 1;
            tracing::error!(%feed, %id, version = %version.normalized(), error = %e, "retention failed to prune version");
        }
    }
}

/// Apply the policy to a single package id within `feed`. Returns the number of
/// versions pruned from that feed.
pub async fn prune_package(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    id: &str,
    policy: &RetentionPolicy,
) -> Result<usize> {
    Ok(prune_package_outcome(storage, db, feed, id, policy)
        .await?
        .deleted)
}

async fn prune_package_outcome(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    id: &str,
    policy: &RetentionPolicy,
) -> Result<Outcome> {
    let mut outcome = Outcome::default();
    if !policy.has_limits() {
        return Ok(outcome);
    }
    let versions = db.find_all_versions(feed, id).await?;
    for planned in plan_for(&versions, policy, Utc::now()) {
        delete_planned(storage, db, feed, id, &planned.version, &mut outcome).await;
    }
    Ok(outcome)
}

/// Apply the policy to every package id in `feed`. Returns the total pruned.
pub async fn prune_all(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    policy: &RetentionPolicy,
) -> Result<usize> {
    Ok(prune_all_outcome(storage, db, feed, policy).await?.deleted)
}

async fn prune_all_outcome(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    policy: &RetentionPolicy,
) -> Result<Outcome> {
    let mut total = Outcome::default();
    if !policy.has_limits() {
        return Ok(total);
    }
    for id in db.all_package_ids(feed).await? {
        match prune_package_outcome(storage, db, feed, &id, policy).await {
            Ok(o) => {
                total.deleted += o.deleted;
                total.freed += o.freed;
                total.errors += o.errors;
            }
            Err(e) => {
                total.errors += 1;
                tracing::error!(%feed, %id, error = %e, "retention sweep failed for package");
            }
        }
    }
    if total.deleted > 0 {
        tracing::info!(%feed, pruned = total.deleted, freed = total.freed, "retention sweep complete");
    }
    Ok(total)
}

/// One version the next cleanup would delete, as the admin page shows it.
#[derive(Debug, Clone)]
pub struct Planned {
    /// The package id as published.
    pub id: String,
    pub version: NuGetVersion,
    pub published: DateTime<Utc>,
    pub reason: PruneReason,
    /// Bytes deleting it frees: its size when no other feed holds it, else 0.
    pub frees: u64,
}

/// What the next cleanup of a feed would do, computed without doing it.
#[derive(Debug, Clone, Default)]
pub struct Preview {
    /// Every version it would delete, by package id then version.
    pub planned: Vec<Planned>,
    /// The pinned versions it keeps regardless: `(id, version)`.
    pub pinned: Vec<(String, NuGetVersion)>,
}

impl Preview {
    /// A short, stable fingerprint of exactly which versions would go.
    ///
    /// The page shows the plan, and the "delete" button sends this back. The
    /// server recomputes the plan and deletes only if the fingerprint still
    /// matches — so a push landing between looking and clicking can never make
    /// the click delete something the operator was not shown.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut keys: Vec<String> = self
            .planned
            .iter()
            .map(|p| format!("{}\0{}", canonical_id(&p.id), p.version.normalized()))
            .collect();
        keys.sort();
        let mut hasher = Sha256::new();
        for key in keys {
            hasher.update(key.as_bytes());
            hasher.update(b"\n");
        }
        hex::encode(&hasher.finalize()[..16])
    }

    /// Bytes the whole plan frees.
    pub fn frees(&self) -> u64 {
        self.planned.iter().map(|p| p.frees).sum()
    }
}

/// What the next cleanup of `feed` would delete under `policy`, and why.
///
/// Every GET of the admin page runs this, so it reads the feed a chunk of ids
/// at a time — a few statements per chunk — rather than several per id and
/// per planned version.
pub async fn preview(
    db: &dyn PackageDatabase,
    feed: &str,
    policy: &RetentionPolicy,
    now: DateTime<Utc>,
) -> Result<Preview> {
    const CHUNK: usize = 200;
    let mut out = Preview::default();
    let ids = db.all_package_ids(feed).await?;
    for chunk in ids.chunks(CHUNK) {
        let lower: Vec<String> = chunk.iter().map(|id| canonical_id(id)).collect();
        let mut by_id: std::collections::HashMap<String, Vec<FeedVersion>> =
            std::collections::HashMap::new();
        for v in db.find_all_versions_of(feed, &lower).await? {
            by_id.entry(v.package.lower_id()).or_default().push(v);
        }
        let mut planned: Vec<(&FeedVersion, Pruned)> = Vec::new();
        for id in &lower {
            let Some(versions) = by_id.get(id) else {
                continue;
            };
            for v in versions.iter().filter(|v| v.pinned) {
                out.pinned
                    .push((v.package.id.clone(), v.package.version.clone()));
            }
            for p in plan_for(versions, policy, now) {
                if let Some(fv) = versions.iter().find(|v| v.package.version == p.version) {
                    planned.push((fv, p));
                }
            }
        }
        if planned.is_empty() {
            continue;
        }
        let footprints: std::collections::HashMap<(String, String), (i64, u64)> = db
            .version_footprints(&lower)
            .await?
            .into_iter()
            .map(|f| ((f.lower_id, f.normalized_version), (f.feeds, f.file_bytes)))
            .collect();
        for (fv, p) in planned {
            let key = (fv.package.lower_id(), fv.package.normalized_version());
            let frees = match footprints.get(&key) {
                Some(&(feeds, files)) if feeds <= 1 => fv.package.package_size + files,
                Some(_) => 0,
                None => fv.package.package_size,
            };
            out.planned.push(Planned {
                id: fv.package.id.clone(),
                version: p.version,
                published: fv.package.published,
                reason: p.reason,
                frees,
            });
        }
    }
    Ok(out)
}

/// Delete exactly the versions of a [`Preview`].
pub async fn apply(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    plan: &Preview,
) -> Outcome {
    let mut outcome = Outcome::default();
    for p in &plan.planned {
        delete_planned(storage, db, feed, &p.id, &p.version, &mut outcome).await;
    }
    outcome
}

/// What started a cleanup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The background sweep.
    Schedule,
    /// An admin's "delete now".
    Manual,
}

/// The report of a finished cleanup.
#[derive(Debug, Clone, Copy)]
pub struct Report {
    pub finished: DateTime<Utc>,
    pub trigger: Trigger,
    pub outcome: Outcome,
}

/// One feed's cleanup state, shared by the background sweep and the admin
/// page: at most one cleanup runs at a time, and the last one is remembered.
#[derive(Debug, Default)]
pub struct RetentionState {
    running: tokio::sync::Mutex<()>,
    last: std::sync::Mutex<Option<Report>>,
}

impl RetentionState {
    /// The last finished cleanup, if any ran since the server started.
    pub fn last(&self) -> Option<Report> {
        *self.last.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether a cleanup is running right now.
    pub fn is_running(&self) -> bool {
        self.running.try_lock().is_err()
    }

    fn record(&self, trigger: Trigger, outcome: Outcome) {
        *self.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(Report {
            finished: Utc::now(),
            trigger,
            outcome,
        });
    }

    /// The scheduled sweep: waits for a manual cleanup to finish, then runs.
    pub async fn sweep(
        &self,
        storage: &dyn PackageStorage,
        db: &dyn PackageDatabase,
        feed: &str,
        policy: &RetentionPolicy,
    ) -> Result<Outcome> {
        let _running = self.running.lock().await;
        let outcome = prune_all_outcome(storage, db, feed, policy).await?;
        self.record(Trigger::Schedule, outcome);
        Ok(outcome)
    }

    /// An admin's cleanup of exactly what they were shown. Returns `None`
    /// when a cleanup is already running, and `Some(Err(plan))` — deleting
    /// nothing — when the plan no longer has `fingerprint`.
    pub async fn run_shown(
        &self,
        storage: &dyn PackageStorage,
        db: &dyn PackageDatabase,
        feed: &str,
        policy: &RetentionPolicy,
        fingerprint: &str,
    ) -> Result<Option<std::result::Result<Outcome, Preview>>> {
        let Ok(_running) = self.running.try_lock() else {
            return Ok(None);
        };
        let plan = preview(db, feed, policy, Utc::now()).await?;
        if plan.fingerprint() != fingerprint {
            return Ok(Some(Err(plan)));
        }
        let outcome = apply(storage, db, feed, &plan).await;
        self.record(Trigger::Manual, outcome);
        Ok(Some(Ok(outcome)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn pkg(version: &str, days_old: i64) -> Package {
        let published = Utc::now() - Duration::days(days_old);
        Package {
            id: "Pkg".into(),
            version: NuGetVersion::parse(version).unwrap(),
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
            published,
            downloads: 0,
            package_types: vec![],
            dependencies: vec![],
        }
    }

    #[test]
    fn an_absurd_max_age_prunes_nothing_instead_of_panicking() {
        // A day count this large is out of `Duration`'s range. Building the
        // cutoff must not panic, and — critically — must not wrap into a future
        // date, which would mark every version as expired and delete the feed.
        // 1.0.0 is ancient; 2.0.0 is current and is the protected newest stable.
        let packages = vec![pkg("1.0.0", 5_000), pkg("2.0.0", 0)];
        for days in [u64::MAX, i64::MAX as u64, 1 << 60] {
            let policy = RetentionPolicy {
                max_age_days: Some(days),
                ..Default::default()
            };
            assert!(
                versions_to_prune(&packages, &policy, Utc::now()).is_empty(),
                "max_age_days = {days} pruned versions"
            );
        }

        // A sane large-but-representable value still works normally.
        let policy = RetentionPolicy {
            max_age_days: Some(1_000),
            ..Default::default()
        };
        assert_eq!(
            names(versions_to_prune(&packages, &policy, Utc::now())),
            vec!["1.0.0".to_string()],
            "the 5000-day-old 1.0.0 should be pruned; 2.0.0 is the protected newest"
        );
    }

    fn names(mut v: Vec<NuGetVersion>) -> Vec<String> {
        v.sort();
        v.iter().map(|x| x.normalized()).collect()
    }

    fn plan_names(plan: &[Pruned]) -> Vec<String> {
        names(plan.iter().map(|p| p.version.clone()).collect())
    }

    #[test]
    fn a_pin_is_kept_and_uses_up_no_slot() {
        let packages = [
            pkg("1.0.0", 0),
            pkg("2.0.0", 0),
            pkg("3.0.0", 0),
            pkg("4.0.0", 0),
        ];
        let policy = RetentionPolicy {
            keep_latest_stable: Some(2),
            ..Default::default()
        };
        let with_pins = |pinned: &[&str]| {
            let versions: Vec<Candidate<'_>> = packages
                .iter()
                .map(|p| Candidate {
                    pinned: pinned.contains(&p.version.normalized().as_str()),
                    ..Candidate::ranked(p)
                })
                .collect();
            plan_names(&prune_plan(&versions, &policy, Utc::now()))
        };
        assert_eq!(with_pins(&[]), vec!["1.0.0", "2.0.0"]);
        // 3.0.0 pinned: it stays, and the newest two *unpinned* stay with it.
        assert_eq!(with_pins(&["3.0.0"]), vec!["1.0.0"]);
        assert_eq!(with_pins(&["1.0.0", "2.0.0"]), Vec::<String>::new());
    }

    #[test]
    fn a_pin_outlives_the_age_limit_and_the_newest_is_still_kept() {
        let packages = [pkg("1.0.0", 900), pkg("2.0.0", 800), pkg("3.0.0", 700)];
        let policy = RetentionPolicy {
            max_age_days: Some(90),
            ..Default::default()
        };
        let versions: Vec<Candidate<'_>> = packages
            .iter()
            .map(|p| Candidate {
                pinned: p.version.normalized() == "1.0.0",
                ..Candidate::ranked(p)
            })
            .collect();
        let plan = prune_plan(&versions, &policy, Utc::now());
        // 3.0.0 is the protected newest, 1.0.0 is pinned: only 2.0.0 goes.
        assert_eq!(plan_names(&plan), vec!["2.0.0"]);
        assert_eq!(
            plan[0].reason,
            PruneReason {
                beyond_newest: None,
                older_than_days: Some(90)
            }
        );
    }

    #[test]
    fn reasons_read_as_words() {
        let both = PruneReason {
            beyond_newest: Some((5, false)),
            older_than_days: Some(90),
        };
        assert_eq!(
            both.describe(),
            "beyond the newest 5 stable versions, and older than 90 days"
        );
        let one = PruneReason {
            beyond_newest: Some((1, true)),
            older_than_days: None,
        };
        assert_eq!(one.describe(), "beyond the newest 1 pre-release version");
        let packages = [pkg("1.0.0-rc.1", 0), pkg("1.0.0-rc.2", 0), pkg("0.9.0", 0)];
        let versions: Vec<Candidate<'_>> = packages.iter().map(Candidate::ranked).collect();
        let policy = RetentionPolicy {
            keep_latest_prerelease: Some(1),
            ..Default::default()
        };
        let plan = prune_plan(&versions, &policy, Utc::now());
        assert_eq!(plan_names(&plan), vec!["1.0.0-rc.1"]);
        assert_eq!(plan[0].reason.beyond_newest, Some((1, true)));
    }

    #[test]
    fn the_fingerprint_names_exactly_the_planned_versions() {
        let planned = |id: &str, v: &str| Planned {
            id: id.into(),
            version: NuGetVersion::parse(v).unwrap(),
            published: Utc::now(),
            reason: PruneReason {
                beyond_newest: Some((1, false)),
                older_than_days: None,
            },
            frees: 1,
        };
        let a = Preview {
            planned: vec![planned("A", "1.0.0"), planned("B", "2.0.0")],
            pinned: vec![],
        };
        // Order and id casing do not matter; the set of versions does.
        let b = Preview {
            planned: vec![planned("b", "2.0.0"), planned("a", "1.0.0")],
            pinned: vec![],
        };
        assert_eq!(a.fingerprint(), b.fingerprint());
        let c = Preview {
            planned: vec![planned("A", "1.0.0")],
            pinned: vec![],
        };
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert_ne!(Preview::default().fingerprint(), c.fingerprint());
    }

    #[test]
    fn no_limits_prunes_nothing() {
        let packages = vec![pkg("1.0.0", 0), pkg("2.0.0", 0)];
        let out = versions_to_prune(&packages, &RetentionPolicy::default(), Utc::now());
        assert!(out.is_empty());
    }

    #[test]
    fn keeps_newest_n_stable() {
        let packages = vec![
            pkg("1.0.0", 0),
            pkg("1.1.0", 0),
            pkg("1.2.0", 0),
            pkg("2.0.0", 0),
        ];
        let policy = RetentionPolicy {
            keep_latest_stable: Some(2),
            ..Default::default()
        };
        // Keep 2.0.0 and 1.2.0; prune 1.1.0 and 1.0.0.
        assert_eq!(
            names(versions_to_prune(&packages, &policy, Utc::now())),
            vec!["1.0.0", "1.1.0"]
        );
    }

    #[test]
    fn stable_and_prerelease_channels_are_independent() {
        let packages = vec![
            pkg("1.0.0", 0),
            pkg("2.0.0", 0),
            pkg("3.0.0-rc.1", 0),
            pkg("3.0.0-rc.2", 0),
        ];
        let policy = RetentionPolicy {
            keep_latest_stable: Some(1),
            keep_latest_prerelease: Some(1),
            ..Default::default()
        };
        // Keep newest stable (2.0.0) and newest prerelease (3.0.0-rc.2).
        assert_eq!(
            names(versions_to_prune(&packages, &policy, Utc::now())),
            vec!["1.0.0", "3.0.0-rc.1"]
        );
    }

    #[test]
    fn prunes_by_age_but_protects_newest() {
        let packages = vec![pkg("1.0.0", 100), pkg("2.0.0", 100), pkg("3.0.0", 1)];
        let policy = RetentionPolicy {
            max_age_days: Some(30),
            ..Default::default()
        };
        // 1.0.0 is old → pruned. 2.0.0 is old but... not protected (3.0.0 is
        // the newest stable). So both old ones go.
        assert_eq!(
            names(versions_to_prune(&packages, &policy, Utc::now())),
            vec!["1.0.0", "2.0.0"]
        );
    }

    #[test]
    fn never_prunes_the_only_version() {
        let packages = vec![pkg("1.0.0", 9999)];
        let policy = RetentionPolicy {
            keep_latest_stable: Some(0),
            max_age_days: Some(1),
            ..Default::default()
        };
        assert!(versions_to_prune(&packages, &policy, Utc::now()).is_empty());
    }

    #[test]
    fn protects_newest_prerelease_when_no_stable() {
        let packages = vec![pkg("1.0.0-a", 9999), pkg("1.0.0-b", 9999)];
        let policy = RetentionPolicy {
            keep_latest_prerelease: Some(0),
            ..Default::default()
        };
        // Both exceed the count cap of 0, but the newest (1.0.0-b) is protected.
        assert_eq!(
            names(versions_to_prune(&packages, &policy, Utc::now())),
            vec!["1.0.0-a"]
        );
    }

    #[test]
    fn age_uses_injected_now() {
        let base = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let mut old = pkg("1.0.0", 0);
        old.published = base;
        let mut new = pkg("2.0.0", 0);
        new.published = base + Duration::days(40);
        let policy = RetentionPolicy {
            max_age_days: Some(30),
            ..Default::default()
        };
        let now = base + Duration::days(50);
        // 1.0.0 is 50 days old (> 30) and not protected (2.0.0 is newest).
        assert_eq!(
            names(versions_to_prune(&[old, new], &policy, now)),
            vec!["1.0.0"]
        );
    }

    // --- I/O paths: the body the scheduled sweep runs (#3) ---

    use crate::database::SqliteDatabase;
    use crate::storage::FilesystemStorage;

    const FEED: &str = "default";

    async fn store_dummy(storage: &FilesystemStorage, id: &str, version: &str) {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("p.nupkg");
        tokio::fs::write(&tmp, b"payload").await.unwrap();
        storage.store_package(id, version, tmp).await.unwrap();
    }

    #[tokio::test]
    async fn prune_all_sweeps_every_package() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();

        for id in ["Sweep.A", "Sweep.B"] {
            for v in ["1.0.0", "1.1.0", "1.2.0"] {
                let mut p = pkg(v, 0);
                p.id = id.to_string();
                db.add_to_feed(FEED, &p).await.unwrap();
                store_dummy(&storage, id, v).await;
            }
        }

        let policy = RetentionPolicy {
            keep_latest_stable: Some(1),
            ..Default::default()
        };
        let pruned = prune_all(&storage, &db, FEED, &policy).await.unwrap();
        assert_eq!(pruned, 4); // two older versions per package

        for id in ["sweep.a", "sweep.b"] {
            let remaining = db.find_all_versions(FEED, id).await.unwrap();
            assert_eq!(remaining.len(), 1);
            assert_eq!(remaining[0].package.normalized_version(), "1.2.0");
            // The pruned payloads are gone from storage.
            assert!(!storage.package_exists(id, "1.0.0").await);
            assert!(storage.package_exists(id, "1.2.0").await);
        }
    }

    #[tokio::test]
    async fn purge_keeps_version_referenced_by_another_feed() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();

        let p = pkg("1.0.0", 0);
        db.add_to_feed("dev", &p).await.unwrap();
        db.add_to_feed("stable", &p).await.unwrap();
        store_dummy(&storage, "Pkg", "1.0.0").await;

        // Purging from one feed leaves the payload and the other feed intact.
        assert!(purge_version(&storage, &db, "dev", "Pkg", &p.version)
            .await
            .unwrap());
        assert!(storage.package_exists("pkg", "1.0.0").await);
        assert!(db.package_data_exists("pkg", &p.version).await.unwrap());
        assert!(db.exists("stable", "pkg", &p.version).await.unwrap());

        // Purging from the last feed removes the global data + payload.
        assert!(purge_version(&storage, &db, "stable", "Pkg", &p.version)
            .await
            .unwrap());
        assert!(!storage.package_exists("pkg", "1.0.0").await);
        assert!(!db.package_data_exists("pkg", &p.version).await.unwrap());
    }

    #[tokio::test]
    async fn purge_version_also_removes_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();

        let p = pkg("1.0.0", 0);
        db.add_to_feed(FEED, &p).await.unwrap();
        store_dummy(&storage, "Pkg", "1.0.0").await;
        db.add_symbol("KEYFFFFFFFF", "pkg.pdb", "Pkg", &p.version)
            .await
            .unwrap();
        storage
            .store_symbol("KEYFFFFFFFF", "pkg.pdb", b"pdb")
            .await
            .unwrap();

        let removed = purge_version(&storage, &db, FEED, "Pkg", &p.version)
            .await
            .unwrap();
        assert!(removed);
        assert!(db.find_all_versions(FEED, "pkg").await.unwrap().is_empty());
        assert!(db
            .find_symbol("KEYFFFFFFFF", "pkg.pdb")
            .await
            .unwrap()
            .is_none());
        assert!(matches!(
            storage.get_symbol("KEYFFFFFFFF", "pkg.pdb").await,
            Err(crate::error::Error::PackageNotFound)
        ));
        // purge of a non-existent version reports "not removed".
        assert!(!purge_version(&storage, &db, FEED, "Pkg", &p.version)
            .await
            .unwrap());
    }

    #[test]
    fn pending_and_disabled_versions_are_outside_the_rules() {
        let packages = [
            pkg("1.0.0", 0),
            pkg("2.0.0", 0),
            pkg("3.0.0", 0),
            pkg("4.0.0", 0),
        ];
        let policy = RetentionPolicy {
            keep_latest_stable: Some(1),
            ..Default::default()
        };
        // 3.0.0 awaits approval and 4.0.0 was disabled: neither is ranked,
        // neither is the newest that is kept, and neither is pruned.
        let versions: Vec<Candidate<'_>> = packages
            .iter()
            .map(|p| Candidate {
                servable: p.version.core().0 < 3,
                ..Candidate::ranked(p)
            })
            .collect();
        assert_eq!(
            plan_names(&prune_plan(&versions, &policy, Utc::now())),
            vec!["1.0.0"]
        );
        // With nothing servable there is nothing to rank or prune.
        let withheld: Vec<Candidate<'_>> = packages
            .iter()
            .map(|p| Candidate {
                servable: false,
                ..Candidate::ranked(p)
            })
            .collect();
        assert!(prune_plan(&withheld, &policy, Utc::now()).is_empty());
    }

    #[tokio::test]
    async fn pushing_pending_builds_does_not_prune_the_approved_ones() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        let approved = pkg("1.0.0", 0);
        db.add_to_feed(FEED, &approved).await.unwrap();
        store_dummy(&storage, "Pkg", "1.0.0").await;
        for v in ["2.0.0", "3.0.0"] {
            let p = pkg(v, 0);
            db.upsert_package_data(&p).await.unwrap();
            db.add_membership(&crate::database::Membership {
                pending: true,
                ..crate::database::Membership::active(FEED, &p)
            })
            .await
            .unwrap();
            store_dummy(&storage, "Pkg", v).await;
        }
        let policy = RetentionPolicy {
            keep_latest_stable: Some(1),
            ..Default::default()
        };
        assert_eq!(
            prune_package(&storage, &db, FEED, "Pkg", &policy)
                .await
                .unwrap(),
            0
        );
        assert_eq!(db.find_all_versions(FEED, "pkg").await.unwrap().len(), 3);
        assert!(db
            .is_servable(FEED, "pkg", &approved.version)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_pin_set_after_planning_wins() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        for v in ["1.0.0", "2.0.0", "3.0.0"] {
            db.add_to_feed(FEED, &pkg(v, 0)).await.unwrap();
            store_dummy(&storage, "Pkg", v).await;
        }
        let policy = RetentionPolicy {
            keep_latest_stable: Some(1),
            ..Default::default()
        };
        let plan = preview(&db, FEED, &policy, Utc::now()).await.unwrap();
        assert_eq!(plan.planned.len(), 2);
        // Each version is stored in no other feed, so deleting it frees it.
        assert!(plan.planned.iter().all(|p| p.frees == 1));
        // An admin pins 1.0.0 while the cleanup is under way.
        let v1 = NuGetVersion::parse("1.0.0").unwrap();
        db.set_pinned(FEED, "pkg", &v1, true).await.unwrap();

        let outcome = apply(&storage, &db, FEED, &plan).await;
        assert_eq!(outcome.deleted, 1);
        assert!(db.exists(FEED, "pkg", &v1).await.unwrap());
        assert!(storage.package_exists("pkg", "1.0.0").await);
        assert!(!storage.package_exists("pkg", "2.0.0").await);
    }

    #[tokio::test]
    async fn the_preview_counts_a_shared_version_as_freeing_nothing() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        for v in ["1.0.0", "2.0.0"] {
            db.add_to_feed(FEED, &pkg(v, 0)).await.unwrap();
        }
        db.add_to_feed("other", &pkg("1.0.0", 0)).await.unwrap();
        let policy = RetentionPolicy {
            keep_latest_stable: Some(1),
            ..Default::default()
        };
        let plan = preview(&db, FEED, &policy, Utc::now()).await.unwrap();
        assert_eq!(plan.planned.len(), 1);
        assert_eq!(plan.planned[0].frees, 0);
    }

    #[tokio::test]
    async fn the_orphan_sweep_finishes_an_interrupted_purge() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        let orphan = pkg("1.0.0", 0);
        let kept = pkg("2.0.0", 0);
        for p in [&orphan, &kept] {
            db.add_to_feed(FEED, p).await.unwrap();
            store_dummy(&storage, "Pkg", &p.version.normalized()).await;
        }
        // The purge removed the membership and then failed.
        db.remove_membership(FEED, "pkg", &orphan.version)
            .await
            .unwrap();

        assert_eq!(sweep_orphans(&storage, &db).await, 1);
        assert!(!db
            .package_data_exists("pkg", &orphan.version)
            .await
            .unwrap());
        assert!(!storage.package_exists("pkg", "1.0.0").await);
        assert!(storage.package_exists("pkg", "2.0.0").await);
        assert_eq!(sweep_orphans(&storage, &db).await, 0);
    }
}
