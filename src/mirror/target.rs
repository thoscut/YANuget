//! Filling a feed from the upstream: fetching versions and indexing them.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::Instant;

use crate::config::LicensePolicyConfig;
use crate::database::PackageDatabase;
use crate::error::{Error, Result};
use crate::indexing::{self, IndexOptions};
use crate::storage::PackageStorage;
use crate::version::NuGetVersion;

use super::client::permanent_download_failure;
use super::tracker::{Turn, BAD_VERSION_RETRY, WAIT_FOR_FETCH};
use super::MirrorClient;

/// Options governing how mirrored versions are ingested.
#[derive(Debug, Clone, Default)]
pub struct MirrorOptions {
    /// Mirrored versions land pending (require admin approval before serving).
    pub requires_approval: bool,
    /// The feed's license policy, applied to each mirrored version.
    pub license_policy: LicensePolicyConfig,
    /// Id prefixes other feeds reserved, which this feed refuses to mirror.
    pub reserved_elsewhere: Vec<crate::config::ReservedPrefix>,
}

/// Bring `feed` up to date with the upstream's versions of `id`: list them
/// (unless that was done within `refresh_secs`) and fetch and index the
/// newest `max_versions_per_package` the feed does not have. Returns how many
/// versions were mirrored.
///
/// Best-effort: a failure for one version is logged and skipped so a single bad
/// package never blocks the rest. Versions already in the feed are left as-is,
/// and versions deleted from it are never fetched again.
pub async fn ensure_package(
    client: &MirrorClient,
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    temp_dir: &Path,
    id: &str,
    options: &MirrorOptions,
) -> Result<usize> {
    MirrorTarget {
        client,
        storage,
        db,
        feed,
        temp_dir,
        options,
    }
    .ensure_package(id)
    .await
}

/// A feed a mirror client fills: where fetched packages are stored, indexed
/// and staged, and how they are admitted.
pub struct MirrorTarget<'a> {
    pub client: &'a MirrorClient,
    pub storage: &'a dyn PackageStorage,
    pub db: &'a dyn PackageDatabase,
    pub feed: &'a str,
    /// Where downloads are staged: on the package store's filesystem, so
    /// indexing can move them into place with a rename.
    pub temp_dir: &'a Path,
    pub options: &'a MirrorOptions,
}

impl MirrorTarget<'_> {
    /// See [`ensure_package`].
    pub async fn ensure_package(&self, id: &str) -> Result<usize> {
        self.ensure(id, None).await
    }

    /// Fetch one version a client asked for and the feed does not have —
    /// whether or not it is among the newest `max_versions_per_package`, since
    /// a project pinned to an older version would otherwise never restore.
    /// Returns 1 when it was mirrored.
    pub async fn ensure_version(&self, id: &str, version: &NuGetVersion) -> Result<usize> {
        self.ensure(id, Some(version)).await
    }

    async fn ensure(&self, id: &str, want: Option<&NuGetVersion>) -> Result<usize> {
        // Only mirror well-formed package ids. This rejects anything (path
        // traversal, slashes, control characters) that could escape the
        // upstream's PackageBaseAddress path when interpolated into the
        // request URL.
        if crate::validation::validate_package_id(id).is_err() {
            return Ok(0);
        }
        // Reserved for another feed: indexing would refuse every version, so
        // nothing is worth downloading.
        if self.options.reserved_elsewhere.iter().any(|r| r.covers(id)) {
            return Ok(0);
        }
        let client = self.client;
        let lower_id = id.to_lowercase();
        let key = MirrorClient::key(self.feed, &lower_id);

        // One fetch per package and feed at a time. Without this, N concurrent
        // restores of the same missing package each start their own download
        // of every upstream version — N times the bandwidth and disk for one
        // result. The others wait for it, a bounded while, and then answer
        // from what it stored: answering 404 at once failed every restore
        // that happened to lose the race for a package just being fetched.
        let give_up = Instant::now() + WAIT_FOR_FETCH;
        let _fetching = loop {
            match client.tracker.turn(&key, client.refresh) {
                Turn::Lead(guard) => break guard,
                Turn::Wait(mut done) => {
                    let left = give_up.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Ok(0);
                    }
                    let _ = tokio::time::timeout(left, done.changed()).await;
                    // Whatever that fetch did, a listing waiter is served
                    // unless the list is still stale. A version waiter may
                    // need a fetch of its own: the one it waited on may have
                    // been for another version, or the newest few only.
                    let satisfied = match want {
                        None => !client.list_is_stale(self.feed, &lower_id, false),
                        Some(v) => self
                            .db
                            .exists(self.feed, &lower_id, v)
                            .await
                            .unwrap_or(false),
                    };
                    if satisfied || Instant::now() >= give_up {
                        return Ok(0);
                    }
                }
            }
        };

        match want {
            Some(version) => self.fetch_requested(&lower_id, version).await,
            None => self.fetch_listing(&lower_id).await,
        }
    }

    /// Whether `version` should be fetched: the feed lacks it, it was not
    /// deleted from the feed, and it did not just fail. A database error
    /// counts as "no" — a mirror that cannot tell must not re-add a version
    /// that may have been deleted.
    pub(super) async fn wanted(&self, lower_id: &str, version: &NuGetVersion) -> bool {
        let (db, feed) = (self.db, self.feed);
        if db.exists(feed, lower_id, version).await.unwrap_or(true)
            || db
                .is_tombstoned(feed, lower_id, version)
                .await
                .unwrap_or(true)
        {
            return false;
        }
        let now = Instant::now();
        let normalized = version.normalized().to_lowercase();
        self.client.tracker.with(
            &MirrorClient::key(feed, lower_id),
            self.client.refresh,
            |state| {
                state
                    .bad_versions
                    .get(&normalized)
                    .is_none_or(|until| *until <= now)
            },
        )
    }

    fn remember_bad(&self, lower_id: &str, version: &NuGetVersion) {
        let until = Instant::now() + BAD_VERSION_RETRY;
        let normalized = version.normalized().to_lowercase();
        self.client.tracker.with(
            &MirrorClient::key(self.feed, lower_id),
            self.client.refresh,
            |state| {
                state.bad_versions.retain(|_, u| *u > Instant::now());
                state.bad_versions.insert(normalized, until);
            },
        );
    }

    pub(super) fn backing_off(&self, lower_id: &str) -> bool {
        let now = Instant::now();
        let client = self.client;
        client.tracker.upstream().active(now)
            || client.tracker.with(
                &MirrorClient::key(self.feed, lower_id),
                client.refresh,
                |state| state.backoff.active(now),
            )
    }

    pub(super) fn record_upstream(&self, lower_id: &str, ok: bool) {
        let now = Instant::now();
        self.client.tracker.with(
            &MirrorClient::key(self.feed, lower_id),
            self.client.refresh,
            |state| {
                if ok {
                    state.backoff.succeed();
                } else {
                    state.backoff.fail(now);
                }
            },
        );
    }

    /// The service index, or an error that backs off every package: when it
    /// cannot be read, nothing else can be either.
    pub(super) async fn resources_ready(&self) -> Result<()> {
        match self.client.resources().await {
            Ok(_) => {
                self.client.tracker.upstream().succeed();
                Ok(())
            }
            Err(e) => {
                self.client.tracker.upstream().fail(Instant::now());
                Err(e)
            }
        }
    }

    async fn fetch_requested(&self, lower_id: &str, version: &NuGetVersion) -> Result<usize> {
        if self.backing_off(lower_id) || !self.wanted(lower_id, version).await {
            return Ok(0);
        }
        self.resources_ready().await?;
        let listing = self.upstream_listing(lower_id, false).await;
        match self.fetch_one(lower_id, version, listing.as_deref()).await {
            Fetched::Mirrored => Ok(1),
            Fetched::NotMirrored => Ok(0),
            Fetched::DownloadFailed(e) => {
                // It may be this version, or the upstream: back off both.
                self.record_upstream(lower_id, false);
                Err(e)
            }
        }
    }

    /// Download and index one version. Every failure but the download itself
    /// is logged here; a version that failed for good (see
    /// [`permanent_download_failure`]) is not asked for again for a while, so
    /// a broken one cannot turn each read into a fresh download.
    ///
    /// `listing` is the upstream's, when it could be read: a version it has
    /// unlisted is indexed unlisted, so it is never served as listed here.
    pub(super) async fn fetch_one(
        &self,
        lower_id: &str,
        version: &NuGetVersion,
        listing: Option<&HashMap<String, bool>>,
    ) -> Fetched {
        let (feed, options) = (self.feed, self.options);
        let normalized = version.normalized().to_lowercase();
        let temp_path = self
            .temp_dir
            .join(format!("mirror-{}.tmp", uuid::Uuid::new_v4()));
        let summary = match self
            .client
            .download_nupkg(lower_id, &normalized, &temp_path)
            .await
        {
            Ok(summary) => summary,
            Err(e) => {
                let _ = tokio::fs::remove_file(&temp_path).await;
                // Only a failure that would repeat blocks the version for a
                // while; a transient one may be retried on the next read.
                if permanent_download_failure(&e) {
                    self.remember_bad(lower_id, version);
                }
                self.client
                    .health
                    .mirror_failures
                    .fetch_add(1, Ordering::Relaxed);
                return Fetched::DownloadFailed(e);
            }
        };

        // A delete may have landed while the download ran.
        if self
            .db
            .is_tombstoned(feed, lower_id, version)
            .await
            .unwrap_or(true)
        {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Fetched::NotMirrored;
        }

        let opts = IndexOptions {
            overwrite: crate::config::OverwriteMode::Disabled,
            pending: options.requires_approval,
            license_policy: options.license_policy.clone(),
            // Pin the identity: whatever the upstream returned must be the
            // package we asked for. Otherwise a hostile upstream answers a
            // request for an obscure id with a manifest claiming a popular one,
            // and it lands in the local feed under that trusted name.
            expect: Some(crate::indexing::ExpectedIdentity {
                id: lower_id.to_string(),
                version: version.clone(),
            }),
            reserved_elsewhere: options.reserved_elsewhere.clone(),
            // A mirrored version is never overwritten.
            other_license_policies: Vec::new(),
            unlisted: listing.and_then(|l| l.get(&normalized)) == Some(&false),
        };
        match indexing::index_package(self.storage, self.db, feed, temp_path, summary, &opts).await
        {
            Ok(_) => {
                tracing::info!(%feed, id = %lower_id, version = %normalized, pending = options.requires_approval, "mirrored package");
                self.client.health.mirrored.fetch_add(1, Ordering::Relaxed);
                Fetched::Mirrored
            }
            // A concurrent fetch in another feed, or a push, got there first.
            Err(Error::PackageAlreadyExists | Error::VersionExists(_))
                if self
                    .db
                    .exists(feed, lower_id, version)
                    .await
                    .unwrap_or(false) =>
            {
                Fetched::NotMirrored
            }
            Err(e) => {
                tracing::warn!(%feed, id = %lower_id, version = %normalized, error = %e, "mirror index failed");
                self.remember_bad(lower_id, version);
                self.client
                    .health
                    .mirror_failures
                    .fetch_add(1, Ordering::Relaxed);
                Fetched::NotMirrored
            }
        }
    }
}

/// How fetching one version went.
pub(super) enum Fetched {
    Mirrored,
    /// Present already, deleted meanwhile, or refused (and logged).
    NotMirrored,
    /// The download failed; the caller decides how loudly.
    DownloadFailed(Error),
}
