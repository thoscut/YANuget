//! Bulk migration of every package from a source NuGet server into a local feed.
//!
//! Where [`crate::mirror`] caches packages *read-through* — one id at a time, on
//! demand — migration is *proactive*: it discovers **all** of a source server's
//! packages up front, then downloads each `.nupkg` and runs it through the same
//! [`crate::indexing`] pipeline a push or a mirror would, so the streaming and
//! license-policy guarantees apply identically.
//!
//! Discovery uses the source's `SearchQueryService` (paged), falling back to the
//! `Catalog/3.0.0` resource (see [`MirrorClient::enumerate_package_ids`]).
//! Versions already present in the target feed are skipped, which makes a
//! migration **idempotent and resumable**: re-running it only fetches what is
//! missing.
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
}

impl Default for MigrateOptions {
    fn default() -> Self {
        Self {
            concurrency: 4,
            include_prerelease: true,
            overwrite: OverwriteMode::Disabled,
            dry_run: false,
            quiet: false,
        }
    }
}

/// One failed (id, version) during a migration.
#[derive(Debug, Clone)]
pub struct MigrateFailure {
    pub id: String,
    pub version: String,
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
    /// Versions skipped because they already existed in the target feed.
    pub skipped: usize,
    /// Versions that could not be migrated.
    pub failed: usize,
    /// Total bytes downloaded from the source.
    pub total_bytes: u64,
    /// Wall-clock duration of the run.
    pub elapsed: Duration,
    /// Per-version failures, for a final report.
    pub failures: Vec<MigrateFailure>,
}

/// A single unit of work: one source version to download and index.
struct WorkItem {
    id: String,
    lower_id: String,
    /// Lowercased, normalized version — the form used in the flat-container path.
    normalized: String,
    /// Normalized version for display/reporting.
    display_version: String,
}

enum OutcomeKind {
    Imported,
    Skipped,
    Failed {
        id: String,
        version: String,
        error: String,
    },
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

    let client = MirrorClient::from_config(&source).ok_or_else(|| {
        Error::Other(anyhow::anyhow!(
            "could not build a source client (mirror config disabled?)"
        ))
    })?;

    if !opts.quiet {
        println!(
            "Migrating packages from {} into feed '{}'{}",
            source.upstream,
            feed.name,
            if opts.dry_run { " (dry run)" } else { "" }
        );
    }

    let mp = MultiProgress::with_draw_target(draw);

    // --- Phase 1: discovery --------------------------------------------------
    let discover = mp.add(ProgressBar::new_spinner());
    discover.set_style(spinner_style());
    discover.enable_steady_tick(Duration::from_millis(120));
    discover.set_message("resolving source service index & listing package ids…");

    let ids = client.enumerate_package_ids().await?;
    discover.finish_with_message(format!("discovered {} package id(s)", ids.len()));

    let version_bar = mp.add(ProgressBar::new(ids.len() as u64));
    version_bar.set_style(count_style("listing versions"));

    // List each id's versions concurrently, filtering prereleases and (unless
    // overwriting) versions already present in the target feed.
    let include_prerelease = opts.include_prerelease;
    let id_results: Vec<IdResult> = stream::iter(ids.iter().cloned())
        .map(|id| {
            let client = &client;
            let bar = version_bar.clone();
            let feed_name = feed.name.as_str();
            async move {
                let lower = id.to_lowercase();
                let result = match client.upstream_versions(&lower).await {
                    Ok(versions) => {
                        let mut items = Vec::new();
                        let mut present = 0usize;
                        for raw in versions {
                            let Ok(version) = NuGetVersion::parse(&raw) else {
                                continue;
                            };
                            if !include_prerelease && version.is_prerelease() {
                                continue;
                            }
                            if skip_existing
                                && db
                                    .exists(feed_name, &lower, &version)
                                    .await
                                    .unwrap_or(false)
                            {
                                present += 1;
                                continue;
                            }
                            let normalized = version.normalized();
                            items.push(WorkItem {
                                id: id.clone(),
                                lower_id: lower.clone(),
                                normalized: normalized.to_lowercase(),
                                display_version: normalized,
                            });
                        }
                        IdResult::Listed { items, present }
                    }
                    Err(e) => IdResult::Failed {
                        id: id.clone(),
                        error: e.to_string(),
                    },
                };
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
    let mut summary = MigrateSummary {
        discovered_ids: ids.len(),
        ..Default::default()
    };
    for r in id_results {
        match r {
            IdResult::Listed { items, present } => {
                already_present += present;
                work.extend(items);
            }
            IdResult::Failed { id, error } => {
                summary.failures.push(MigrateFailure {
                    id,
                    version: "*".to_string(),
                    error,
                });
            }
        }
    }
    summary.total_versions = work.len() + already_present;

    if opts.dry_run {
        summary.skipped = already_present;
        summary.failed = summary.failures.len();
        summary.elapsed = started.elapsed();
        if !opts.quiet {
            println!(
                "Dry run: {} version(s) would be migrated, {} already present, {} id(s) failed to list.",
                work.len(),
                already_present,
                summary.failures.len()
            );
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
    };

    let outcomes: Vec<Outcome> = stream::iter(work.into_iter())
        .map(|item| {
            let client = &client;
            let index_opts = &index_opts;
            let feed_name = feed.name.as_str();
            let items_bar = items_bar.clone();
            let bytes_bar = bytes_bar.clone();
            async move {
                items_bar.set_message(format!("{} {}", item.id, item.display_version));
                let outcome =
                    migrate_one(client, storage, db, feed_name, temp_dir, index_opts, &item).await;
                items_bar.inc(1);
                if outcome.bytes > 0 {
                    bytes_bar.inc(outcome.bytes);
                }
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
            OutcomeKind::Imported => summary.imported += 1,
            OutcomeKind::Skipped => summary.skipped += 1,
            OutcomeKind::Failed { id, version, error } => {
                summary.failures.push(MigrateFailure { id, version, error });
            }
        }
    }
    summary.skipped += already_present;
    summary.failed = summary.failures.len();
    summary.elapsed = started.elapsed();

    if !opts.quiet {
        print_summary(&summary);
    }
    Ok(summary)
}

/// Download and index one version. Errors are captured into the [`Outcome`]
/// rather than aborting the whole run.
async fn migrate_one(
    client: &MirrorClient,
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    temp_dir: &Path,
    index_opts: &IndexOptions,
    item: &WorkItem,
) -> Outcome {
    let temp_path = temp_dir.join(format!("migrate-{}.tmp", uuid::Uuid::new_v4()));
    let summary = match client
        .download_nupkg(&item.lower_id, &item.normalized, &temp_path)
        .await
    {
        Ok(summary) => summary,
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Outcome {
                kind: OutcomeKind::Failed {
                    id: item.id.clone(),
                    version: item.display_version.clone(),
                    error: e.to_string(),
                },
                bytes: 0,
            };
        }
    };
    let bytes = summary.size;

    // index_package moves the temp file into storage on success and removes it
    // on failure, so we never leave the download behind.
    match indexing::index_package(storage, db, feed, temp_path, summary, index_opts).await {
        Ok(_) => Outcome {
            kind: OutcomeKind::Imported,
            bytes,
        },
        // A concurrent run (or a non-overwriting policy) already has it.
        Err(Error::PackageAlreadyExists) => Outcome {
            kind: OutcomeKind::Skipped,
            bytes,
        },
        Err(e) => Outcome {
            kind: OutcomeKind::Failed {
                id: item.id.clone(),
                version: item.display_version.clone(),
                error: e.to_string(),
            },
            bytes,
        },
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
    if !summary.failures.is_empty() {
        eprintln!("\nFailures ({}):", summary.failures.len());
        for f in &summary.failures {
            eprintln!("  {} {} — {}", f.id, f.version, f.error);
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
