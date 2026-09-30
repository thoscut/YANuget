//! Bulk migration of every package from a source NuGet server into a local feed.
//!
//! Where [`crate::mirror`] caches packages *read-through* — one id at a time, on
//! demand — migration is *proactive*: it discovers **all** of a source server's
//! packages up front, then downloads each `.nupkg` and runs it through the same
//! [`crate::indexing`] pipeline a push or a mirror would, so the streaming and
//! license-policy guarantees apply identically.
//!
//! Discovery pages the source's `SearchQueryService` and walks its
//! `Catalog/3.0.0` resource, and takes the union (see
//! [`MirrorClient::enumerate_package_ids`]). Versions already present in the
//! target feed are skipped, which makes a migration **idempotent and
//! resumable**: re-running it only fetches what is missing. Versions the source
//! has unlisted are unlisted in the target too.
//!
//! The run is driven from the CLI and reports live progress — a package bar with
//! ETA plus a byte counter with transfer rate — via [`indicatif`].

use std::path::Path;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use indicatif::{
    HumanBytes, HumanDuration, MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle,
};

use crate::config::{MirrorConfig, OverwriteMode, ResolvedFeed};
use crate::database::PackageDatabase;
use crate::error::{Error, Result};
use crate::indexing::{self, IndexOptions};
use crate::mirror::MirrorClient;
use crate::storage::PackageStorage;
use crate::version::NuGetVersion;

/// How a migration run behaves.
#[derive(Debug, Clone)]
pub struct MigrateOptions {
    /// Maximum number of packages downloaded/indexed concurrently.
    pub concurrency: usize,
    /// Include pre-release versions (otherwise only stable ones are migrated).
    pub include_prerelease: bool,
    /// Overwrite policy applied to versions that already exist in the target.
    /// When [`OverwriteMode::Disabled`] (the default), existing versions are
    /// skipped during discovery so the run stays idempotent.
    pub overwrite: OverwriteMode,
    /// Only discover and report what *would* be migrated; download nothing.
    pub dry_run: bool,
    /// Suppress the human-facing header/summary prints (used by tests).
    pub quiet: bool,
    /// Free space each download must leave on the storage volume (the
    /// server's `min_free_disk_bytes`); 0 checks nothing.
    pub min_free_disk_bytes: u64,
}

impl Default for MigrateOptions {
    fn default() -> Self {
        Self {
            concurrency: 4,
            include_prerelease: true,
            overwrite: OverwriteMode::Disabled,
            dry_run: false,
            quiet: false,
            min_free_disk_bytes: 0,
        }
    }
}

/// Something that could not be migrated.
#[derive(Debug, Clone)]
pub struct MigrateFailure {
    /// The package id — or, for a discovery failure, the part of the source
    /// that could not be read (`catalog page https://…`).
    pub id: String,
    /// The version, for a version that failed; `None` when a whole package's
    /// version list, or a part of discovery, failed.
    pub version: Option<String>,
    pub error: String,
}

/// The outcome of a migration run.
#[derive(Debug, Clone, Default)]
pub struct MigrateSummary {
    /// Distinct package ids discovered on the source.
    pub discovered_ids: usize,
    /// Total (id, version) pairs the source exposes (after the prerelease
    /// filter), including those already present locally.
    pub total_versions: usize,
    /// Versions newly imported into the target feed.
    pub imported: usize,
    /// Of those, versions unlisted in the target because the source has them
    /// unlisted.
    pub unlisted: usize,
    /// Versions skipped because the target feed already had them, or had them
    /// deleted.
    pub skipped: usize,
    /// Versions that could not be migrated.
    pub failed: usize,
    /// Package ids whose version list could not be read, so none of their
    /// versions were considered.
    pub failed_ids: usize,
    /// Parts of the source that could not be read during discovery (catalog
    /// pages); the ids on them may be missing entirely.
    pub failed_discovery: usize,
    /// Total bytes downloaded from the source.
    pub total_bytes: u64,
    /// Wall-clock duration of the run.
    pub elapsed: Duration,
    /// Every failure, for a final report.
    pub failures: Vec<MigrateFailure>,
}

impl MigrateSummary {
    /// Whether anything at all failed: a version, a version list, or part of
    /// discovery.
    pub fn is_complete(&self) -> bool {
        self.failures.is_empty()
    }
}

/// A single unit of work: one source version to download and index.
struct WorkItem {
    id: String,
    lower_id: String,
    /// Parsed once, while listing: it pins the identity the download must
    /// declare, and a copy re-parsed from a display string could fail and
    /// quietly drop that pin.
    version: NuGetVersion,
    /// The source has this version unlisted.
    unlisted: bool,
}

impl WorkItem {
    /// Lowercased, normalized version — the form used in the flat-container path.
    fn path_version(&self) -> String {
        self.version.normalized().to_lowercase()
    }
}

enum OutcomeKind {
    Imported { unlisted: bool },
    Skipped,
    Failed(MigrateFailure),
}

struct Outcome {
    kind: OutcomeKind,
    /// Bytes downloaded for this item (0 if the download itself failed).
    bytes: u64,
}

/// Migrate every package from `source` into `feed`, importing directly into the
/// local `storage`/`db` via the indexing pipeline.
///
/// `temp_dir` must live on the same filesystem as the package store so the
/// indexing pipeline can move each download into place with an atomic rename;
/// callers pass a subdirectory of the storage root. `draw` controls where the
/// progress bars render — pass [`ProgressDrawTarget::hidden`] in tests.
pub async fn run(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &ResolvedFeed,
    temp_dir: &Path,
    source: MirrorConfig,
    opts: MigrateOptions,
    draw: ProgressDrawTarget,
) -> Result<MigrateSummary> {
    let started = Instant::now();
    let concurrency = opts.concurrency.max(1);
    let skip_existing = matches!(opts.overwrite, OverwriteMode::Disabled);

    // A migration copies packages of any size, so a download is bounded by how
    // long the source goes silent, not by how long it takes. A deadline on the
    // whole transfer failed everything the source could not send within
    // `timeout_secs`: at ~2 MiB/s and the default 60 s, anything past ~120 MB.
    let mut client = MirrorClient::for_migration(&source)?;
    client.set_min_free_disk_bytes(opts.min_free_disk_bytes);

    if !opts.quiet {
        println!(
            "Migrating packages from {} into feed '{}'{}",
            crate::mirror::redact_url(&source.upstream),
            feed.name,
            if opts.dry_run { " (dry run)" } else { "" }
        );
    }

    let mp = MultiProgress::with_draw_target(draw);
    let mut summary = MigrateSummary::default();

    // --- Phase 1: discovery --------------------------------------------------
    let discover = mp.add(ProgressBar::new_spinner());
    discover.set_style(spinner_style());
    discover.enable_steady_tick(Duration::from_millis(120));
    discover.set_message("resolving source service index & listing package ids…");

    let enumeration = client.enumerate_package_ids().await?;
    for (what, error) in enumeration.failures {
        summary.failures.push(MigrateFailure {
            id: what,
            version: None,
            error,
        });
        summary.failed_discovery += 1;
    }
    // Ids come from the source's own search/catalog response and are then
    // interpolated into the URLs we fetch. Anything that is not a well-formed
    // NuGet id (slashes, `..`, control characters) could steer those requests
    // off the flat-container path entirely, so it is dropped here rather than
    // sent.
    let total_discovered = enumeration.ids.len();
    let ids: Vec<String> = enumeration
        .ids
        .into_iter()
        .filter(|id| crate::validation::validate_package_id(id).is_ok())
        .collect();
    if ids.len() < total_discovered {
        let dropped = total_discovered - ids.len();
        tracing::warn!(
            dropped,
            "source listed package ids that are not valid NuGet ids"
        );
        if !opts.quiet {
            println!("Skipping {dropped} source entries that are not valid package ids");
        }
    }
    discover.finish_with_message(format!("discovered {} package id(s)", ids.len()));
    summary.discovered_ids = ids.len();

    let version_bar = mp.add(ProgressBar::new(ids.len() as u64));
    version_bar.set_style(count_style("listing versions"));

    // List each id's versions concurrently, filtering prereleases, versions
    // deleted from the target on purpose and (unless overwriting) versions it
    // already has.
    let include_prerelease = opts.include_prerelease;
    let lister = Lister {
        client: &client,
        db,
        feed: &feed.name,
        include_prerelease,
        skip_existing,
        reserved_elsewhere: &feed.reserved_elsewhere,
    };
    let id_results: Vec<IdResult> = stream::iter(ids.iter().cloned())
        .map(|id| {
            let lister = &lister;
            let bar = version_bar.clone();
            async move {
                let result = lister.list(id).await;
                bar.inc(1);
                result
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;
    version_bar.finish_and_clear();

    let mut work = Vec::new();
    let mut already_present = 0usize;
    for r in id_results {
        match r {
            IdResult::Listed { items, present } => {
                already_present += present;
                work.extend(items);
            }
            IdResult::Failed { id, error } => {
                summary.failed_ids += 1;
                summary.failures.push(MigrateFailure {
                    id,
                    version: None,
                    error,
                });
            }
        }
    }
    summary.total_versions = work.len() + already_present;

    if opts.dry_run {
        summary.skipped = already_present;
        summary.elapsed = started.elapsed();
        if !opts.quiet {
            println!(
                "Dry run: {} version(s) would be migrated, {} already present, {} id(s) failed to list.",
                work.len(),
                already_present,
                summary.failed_ids
            );
            print_failures(&summary);
        }
        return Ok(summary);
    }

    // --- Phase 2: transfer ---------------------------------------------------
    let items_bar = mp.add(ProgressBar::new(work.len() as u64));
    items_bar.set_style(items_style());
    items_bar.enable_steady_tick(Duration::from_millis(250));
    let bytes_bar = mp.add(ProgressBar::new_spinner());
    bytes_bar.set_style(bytes_style());
    bytes_bar.enable_steady_tick(Duration::from_millis(250));

    let index_opts = IndexOptions {
        overwrite: opts.overwrite,
        pending: feed.requires_approval,
        license_policy: feed.license_policy.clone(),
        // Pinned per item in `migrate_one` — the source is asked for a specific
        // id/version and must not be able to answer with a different package.
        expect: None,
        reserved_elsewhere: feed.reserved_elsewhere.clone(),
    };

    let outcomes: Vec<Outcome> = stream::iter(work)
        .map(|item| {
            let client = &client;
            let index_opts = &index_opts;
            let feed_name = feed.name.as_str();
            let items_bar = items_bar.clone();
            let bytes_bar = bytes_bar.clone();
            async move {
                items_bar.set_message(format!("{} {}", item.id, item.version.normalized()));
                // The byte counter moves with every chunk, so the rate it shows
                // is the transfer's, not one jump per finished package.
                let progress = move |n: u64| bytes_bar.inc(n);
                let target = Target {
                    client,
                    storage,
                    db,
                    feed: feed_name,
                    temp_dir,
                    index_opts,
                };
                let outcome = target.migrate_one(&item, &progress).await;
                items_bar.inc(1);
                outcome
            }
        })
        .buffer_unordered(concurrency)
        .collect()
        .await;

    items_bar.finish_and_clear();
    bytes_bar.finish_and_clear();

    for outcome in outcomes {
        summary.total_bytes += outcome.bytes;
        match outcome.kind {
            OutcomeKind::Imported { unlisted } => {
                summary.imported += 1;
                summary.unlisted += usize::from(unlisted);
            }
            OutcomeKind::Skipped => summary.skipped += 1,
            OutcomeKind::Failed(failure) => {
                summary.failed += 1;
                summary.failures.push(failure);
            }
        }
    }
    summary.skipped += already_present;
    summary.elapsed = started.elapsed();

    if !opts.quiet {
        print_summary(&summary);
    }
    Ok(summary)
}

/// Lists one id's versions on the source and decides which to copy.
struct Lister<'a> {
    client: &'a MirrorClient,
    db: &'a dyn PackageDatabase,
    feed: &'a str,
    include_prerelease: bool,
    skip_existing: bool,
    /// Id prefixes another feed owns; the target would refuse them.
    reserved_elsewhere: &'a [crate::config::ReservedPrefix],
}

impl Lister<'_> {
    async fn list(&self, id: String) -> IdResult {
        let (db, feed) = (self.db, self.feed);
        // Indexing would refuse every version, so report the package once
        // instead of downloading each version to be refused.
        if let Some(reserved) = self.reserved_elsewhere.iter().find(|r| r.covers(&id)) {
            return IdResult::Failed {
                error: format!(
                    "under the prefix {:?}, reserved for feed {:?}",
                    reserved.prefix, reserved.feed
                ),
                id,
            };
        }
        let lower = id.to_lowercase();
        let versions = match self.client.upstream_versions(&lower).await {
            Ok(versions) => versions,
            Err(e) => {
                return IdResult::Failed {
                    id,
                    error: e.to_string(),
                }
            }
        };
        let mut items = Vec::new();
        let mut present = 0usize;
        for raw in versions {
            let version = match NuGetVersion::parse(&raw) {
                Ok(version) => version,
                // Skipped, not fatal: the package's other versions still come.
                Err(e) => {
                    tracing::warn!(%id, version = %raw, error = %e, "skipping a source version that does not parse");
                    continue;
                }
            };
            if !self.include_prerelease && version.is_prerelease() {
                continue;
            }
            // Deleted from the target on purpose: a re-run must not bring it
            // back, any more than the mirror would.
            let deleted = db
                .is_tombstoned(feed, &lower, &version)
                .await
                .unwrap_or(false);
            let have =
                self.skip_existing && db.exists(feed, &lower, &version).await.unwrap_or(false);
            if deleted || have {
                present += 1;
                continue;
            }
            items.push(WorkItem {
                id: id.clone(),
                lower_id: lower.clone(),
                version,
                unlisted: false,
            });
        }
        // Only worth a registration read when something is to be copied.
        if !items.is_empty() {
            match self.client.upstream_unlisted(&lower).await {
                Ok(unlisted) => {
                    for item in &mut items {
                        item.unlisted = unlisted.contains(&item.path_version());
                    }
                }
                Err(e) => {
                    return IdResult::Failed {
                        id,
                        error: format!("could not read which versions are unlisted: {e}"),
                    }
                }
            }
        }
        IdResult::Listed { items, present }
    }
}

/// Where one version is copied to.
struct Target<'a> {
    client: &'a MirrorClient,
    storage: &'a dyn PackageStorage,
    db: &'a dyn PackageDatabase,
    feed: &'a str,
    temp_dir: &'a Path,
    index_opts: &'a IndexOptions,
}

impl Target<'_> {
    /// Download and index one version. Errors are captured into the
    /// [`Outcome`] rather than aborting the whole run.
    async fn migrate_one(
        &self,
        item: &WorkItem,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Outcome {
        let failed = |error: String, bytes| Outcome {
            kind: OutcomeKind::Failed(MigrateFailure {
                id: item.id.clone(),
                version: Some(item.version.normalized()),
                error,
            }),
            bytes,
        };
        let temp_path = self
            .temp_dir
            .join(format!("migrate-{}.tmp", uuid::Uuid::new_v4()));
        let summary = match self
            .client
            .download_nupkg_with_progress(
                &item.lower_id,
                &item.path_version(),
                &temp_path,
                progress,
            )
            .await
        {
            Ok(summary) => summary,
            Err(e) => {
                let _ = tokio::fs::remove_file(&temp_path).await;
                return failed(e.to_string(), 0);
            }
        };
        let bytes = summary.size;

        // Require the downloaded manifest to declare the id/version this item
        // asked the source for, so a rogue source cannot slip a different
        // package into the target feed under a name that is already trusted
        // there.
        let index_opts = IndexOptions {
            expect: Some(indexing::ExpectedIdentity {
                id: item.lower_id.clone(),
                version: item.version.clone(),
            }),
            ..self.index_opts.clone()
        };

        // index_package moves the temp file into storage on success and removes
        // it on failure, so we never leave the download behind.
        match indexing::index_package(
            self.storage,
            self.db,
            self.feed,
            temp_path,
            summary,
            &index_opts,
        )
        .await
        {
            Ok(_) => {
                let mut unlisted = false;
                if item.unlisted {
                    match self
                        .db
                        .set_listed(self.feed, &item.lower_id, &item.version, false)
                        .await
                    {
                        Ok(_) => unlisted = true,
                        Err(e) => {
                            return failed(
                                format!("imported, but could not unlist it as the source has: {e}"),
                                bytes,
                            )
                        }
                    }
                }
                Outcome {
                    kind: OutcomeKind::Imported { unlisted },
                    bytes,
                }
            }
            // A concurrent run (or a non-overwriting policy) already has it.
            Err(Error::PackageAlreadyExists | Error::VersionExists(_))
                if self
                    .db
                    .exists(self.feed, &item.lower_id, &item.version)
                    .await
                    .unwrap_or(false) =>
            {
                Outcome {
                    kind: OutcomeKind::Skipped,
                    bytes,
                }
            }
            // Not in this feed, yet refused as existing: another feed holds the
            // same id and version with different content. That is a package
            // the target will never serve, not one it already has.
            Err(Error::PackageAlreadyExists) => failed(
                "another feed on this server holds this id and version with different \
                 content"
                    .to_string(),
                bytes,
            ),
            Err(e) => failed(e.to_string(), bytes),
        }
    }
}

/// Per-id discovery result.
enum IdResult {
    Listed {
        items: Vec<WorkItem>,
        present: usize,
    },
    Failed {
        id: String,
        error: String,
    },
}

fn print_summary(summary: &MigrateSummary) {
    println!(
        "\nMigration complete in {}: {} imported, {} skipped, {} failed | {} transferred ({}/s avg)",
        HumanDuration(summary.elapsed),
        summary.imported,
        summary.skipped,
        summary.failed,
        HumanBytes(summary.total_bytes),
        HumanBytes(average_rate(summary.total_bytes, summary.elapsed)),
    );
    if summary.unlisted > 0 {
        println!(
            "{} imported version(s) unlisted, as they are on the source",
            summary.unlisted
        );
    }
    print_failures(summary);
}

fn print_failures(summary: &MigrateSummary) {
    if summary.failed_ids > 0 {
        eprintln!(
            "\n{} package id(s) could not be listed; none of their versions were copied",
            summary.failed_ids
        );
    }
    if summary.failed_discovery > 0 {
        eprintln!(
            "\n{} part(s) of the source could not be read during discovery; \
             packages listed only there are missing",
            summary.failed_discovery
        );
    }
    if !summary.failures.is_empty() {
        eprintln!("\nFailures ({}):", summary.failures.len());
        for f in &summary.failures {
            match &f.version {
                Some(version) => eprintln!("  {} {} — {}", f.id, version, f.error),
                None => eprintln!("  {} — {}", f.id, f.error),
            }
        }
    }
}

fn average_rate(bytes: u64, elapsed: Duration) -> u64 {
    let secs = elapsed.as_secs_f64();
    if secs > 0.0 {
        (bytes as f64 / secs) as u64
    } else {
        bytes
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.green} {msg}").expect("valid template")
}

fn count_style(label: &str) -> ProgressStyle {
    ProgressStyle::with_template(&format!(
        "{{spinner:.green}} {label} [{{bar:40.cyan/blue}}] {{pos}}/{{len}}"
    ))
    .expect("valid template")
    .progress_chars("##-")
}

fn items_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} pkgs ({percent}%) | {per_sec} | ETA {eta} | {msg}",
    )
    .expect("valid template")
    .progress_chars("##-")
}

fn bytes_style() -> ProgressStyle {
    ProgressStyle::with_template(
        "{spinner:.green} transferred {binary_bytes} @ {binary_bytes_per_sec}",
    )
    .expect("valid template")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn average_rate_is_bytes_per_second() {
        assert_eq!(average_rate(1000, Duration::from_secs(2)), 500);
        // Zero elapsed falls back to the raw byte count (avoids divide-by-zero).
        assert_eq!(average_rate(1000, Duration::ZERO), 1000);
    }

    #[test]
    fn styles_are_valid_templates() {
        // Construction panics on a bad template; this guards the format strings.
        let _ = spinner_style();
        let _ = count_style("listing versions");
        let _ = items_style();
        let _ = bytes_style();
    }
}
