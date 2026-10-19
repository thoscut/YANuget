//! Keeping feeds current (`INT-27`): a watch list of package ids, each
//! optionally with a version range, and `sync`, which internalizes the newest
//! upstream version of each into the intake feed and remembers what became of
//! it.
//!
//! ```toml
//! # watch.toml
//! [[package]]
//! id = "git"
//!
//! [[package]]                 # an id may be listed once per range
//! id = "dotnet-8.0-sdk"
//! versions = "[8.0,9.0)"
//! ```
//!
//! Each entry follows the newest listed version in its range; a pre-release
//! only when a bound of the range is one, as NuGet does. A version released
//! and superseded between two syncs is not fetched.
//!
//! The state file records each version a sync handled:
//!
//! | Status | Means | Next sync |
//! | --- | --- | --- |
//! | `pending` | Pushed; the target feed does not serve it yet. | Waits. |
//! | `approved` | The target feed serves it. | Nothing. |
//! | `needs-override` | A package of its closure needs an override. | Tries again once the overrides change. |
//! | `failed` | The run failed. | Tries again. |
//!
//! A version is never pushed twice by a sync, so one an admin deletes or
//! leaves pending is not pushed again. Once a newer version in the same
//! range is approved, the older entries are dropped, and so is a version an
//! override skips: that is how a version nobody will approve stops counting
//! as pending. The state is saved after each package, so a sync is safe to
//! interrupt; a version pushed just before the interruption answers `409` on
//! the next push, which a run takes as already internalized (`INT-20`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use yanuget_pkg::report::{Report, Status};
use yanuget_pkg::{NuGetVersion, VersionRange};

use crate::closure::Source;
use crate::outcome::Outcome;
use crate::overrides::Overrides;
use crate::target::Target;
use crate::upstream::PackageInfo;

/// The state file's schema version.
pub const STATE_SCHEMA: u32 = 1;

/// The most reasons kept for one version in the state file.
const MAX_REASONS: usize = 10;

/// Why a sync cannot go ahead.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SyncError(pub String);

/// The watch list as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchFile {
    #[serde(default)]
    package: Vec<WatchFileEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchFileEntry {
    id: String,
    versions: Option<String>,
}

/// One package to keep current.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Watch {
    pub id: String,
    /// The versions it follows; all when `None`.
    pub versions: Option<VersionRange>,
}

impl Watch {
    /// Whether `version` is one this entry follows, as a candidate for the
    /// newest.
    fn follows(&self, version: &NuGetVersion) -> bool {
        match &self.versions {
            Some(range) => {
                range.contains(version)
                    && (!version.is_prerelease() || range.has_prerelease_bound())
            }
            None => !version.is_prerelease(),
        }
    }

    /// The newest listed version this entry follows.
    fn newest<'a>(&self, versions: &'a [PackageInfo]) -> Option<&'a PackageInfo> {
        versions
            .iter()
            .rev()
            .find(|i| i.listed && self.follows(&i.version))
    }
}

impl std::fmt::Display for Watch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.versions {
            Some(range) => write!(f, "{} {range}", self.id),
            None => f.write_str(&self.id),
        }
    }
}

/// The packages to keep current.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WatchList {
    pub entries: Vec<Watch>,
}

impl WatchList {
    /// The watch list at `path`.
    pub fn load(path: &Path) -> Result<Self, SyncError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| SyncError(format!("watch list {}: {e}", path.display())))?;
        Self::parse(&text).map_err(|e| SyncError(format!("watch list {}: {e}", path.display())))
    }

    /// A watch list from TOML. Every mistake is listed, and any fails the
    /// list, so a sync never goes ahead with an entry silently left out.
    pub fn parse(text: &str) -> Result<Self, String> {
        let file: WatchFile = toml::from_str(text).map_err(|e| e.message().to_string())?;
        let mut entries = Vec::new();
        let mut errors = Vec::new();
        let mut seen = BTreeSet::new();
        for (i, e) in file.package.into_iter().enumerate() {
            let at = format!("package {} ({})", i + 1, e.id);
            if !crate::cli::is_valid_id(&e.id) {
                errors.push(format!("{at}: not a package id"));
                continue;
            }
            let versions = match e.versions.as_deref().map(str::trim) {
                None | Some("") => None,
                Some(v) => match VersionRange::parse(v) {
                    Ok(r) => Some(r),
                    Err(err) => {
                        errors.push(format!("{at}: versions {v:?}: {err}"));
                        continue;
                    }
                },
            };
            if !seen.insert((
                e.id.to_ascii_lowercase(),
                versions.clone().map(|r| r.to_string()),
            )) {
                errors.push(format!("{at}: listed twice with the same versions"));
                continue;
            }
            entries.push(Watch { id: e.id, versions });
        }
        if errors.is_empty() {
            Ok(WatchList { entries })
        } else {
            Err(errors.join("; "))
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Where a version a sync handled stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SyncStatus {
    Pending,
    Approved,
    NeedsOverride,
    Failed,
}

impl SyncStatus {
    pub const ALL: [SyncStatus; 4] = [
        SyncStatus::Pending,
        SyncStatus::Approved,
        SyncStatus::NeedsOverride,
        SyncStatus::Failed,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            SyncStatus::Pending => "pending",
            SyncStatus::Approved => "approved",
            SyncStatus::NeedsOverride => "needs-override",
            SyncStatus::Failed => "failed",
        }
    }
}

/// One version in the state file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub version: NuGetVersion,
    pub status: SyncStatus,
    /// When it took this status (RFC 3339).
    pub since: String,
    /// How often a sync internalized it.
    pub attempts: u32,
    /// [`Overrides::fingerprint`] when it was last tried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overrides: Option<String>,
    /// Why it needs an override or failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
}

impl Entry {
    fn key(&self) -> (String, NuGetVersion) {
        (self.id.to_ascii_lowercase(), self.version.clone())
    }

    fn since(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.since)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }
}

/// What earlier syncs did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    by_key: BTreeMap<(String, NuGetVersion), Entry>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateFile {
    schema: u32,
    versions: Vec<Entry>,
}

impl State {
    /// The state at `path`; empty when there is no file yet.
    pub fn load(path: &Path) -> Result<Self, SyncError> {
        let fail = |m: String| SyncError(format!("state file {}: {m}", path.display()));
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(State::default()),
            Err(e) => return Err(fail(e.to_string())),
        };
        let file: StateFile = serde_json::from_slice(&bytes).map_err(|e| fail(e.to_string()))?;
        if file.schema != STATE_SCHEMA {
            return Err(fail(format!(
                "schema {} is not {STATE_SCHEMA}, the one this version reads",
                file.schema
            )));
        }
        Ok(State {
            by_key: file.versions.into_iter().map(|e| (e.key(), e)).collect(),
        })
    }

    /// Write the state to `path` atomically: a sync cut off mid-write leaves
    /// the previous state.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let file = StateFile {
            schema: STATE_SCHEMA,
            versions: self.by_key.values().cloned().collect(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file).expect("the state serializes");
        bytes.push(b'\n');
        write_atomic(path, &bytes)
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.by_key.values()
    }

    pub fn get(&self, id: &str, version: &NuGetVersion) -> Option<&Entry> {
        self.by_key.get(&(id.to_ascii_lowercase(), version.clone()))
    }

    fn insert(&mut self, entry: Entry) {
        self.by_key.insert(entry.key(), entry);
    }

    /// How many versions have each status.
    pub fn counts(&self) -> BTreeMap<SyncStatus, usize> {
        let mut counts: BTreeMap<_, _> = SyncStatus::ALL.iter().map(|s| (*s, 0)).collect();
        for e in self.by_key.values() {
            *counts.entry(e.status).or_default() += 1;
        }
        counts
    }

    /// The pending version that has waited longest, and since when.
    pub fn oldest_pending(&self) -> Option<(&Entry, DateTime<Utc>)> {
        self.by_key
            .values()
            .filter(|e| e.status == SyncStatus::Pending)
            .filter_map(|e| e.since().map(|t| (e, t)))
            .min_by_key(|(_, t)| *t)
    }
}

/// Write `bytes` to `path` through a temporary file in the same directory,
/// so a reader (node_exporter, the next sync) sees the old file or the new
/// one, never half of one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&dir)?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other(format!("{} names no file", path.display())))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(name);
    tmp_name.push(".tmp");
    let tmp = dir.join(tmp_name);
    let result = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Hold the lock next to the state file (`<state>.lock`) for as long as the
/// returned file lives, so two syncs never run at once.
pub fn lock(state_file: &Path) -> Result<File, SyncError> {
    let mut name = state_file.as_os_str().to_owned();
    name.push(".lock");
    let path = PathBuf::from(name);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)
            .map_err(|e| SyncError(format!("cannot create {}: {e}", dir.display())))?;
    }
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| SyncError(format!("lock file {}: {e}", path.display())))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(SyncError(format!(
            "another sync holds {}; this one does nothing",
            path.display()
        ))),
        Err(std::fs::TryLockError::Error(e)) => {
            Err(SyncError(format!("lock file {}: {e}", path.display())))
        }
    }
}

/// The feed a sync pushes to, as far as it needs it: which versions it
/// serves.
#[async_trait]
pub trait Feed: Sync {
    /// The versions of `id` the feed serves; `None` when it cannot tell.
    async fn served(&self, id: &str) -> Result<Option<Vec<NuGetVersion>>, String>;
}

#[async_trait]
impl Feed for Target {
    async fn served(&self, id: &str) -> Result<Option<Vec<NuGetVersion>>, String> {
        self.versions(id).await.map_err(|e| e.to_string())
    }
}

/// What internalizes one root and its closure into the target feed: the
/// pipeline `run` uses.
#[async_trait]
pub trait Internalize: Sync {
    /// The report of each package of `root`'s closure, root first.
    async fn internalize(&self, root: &PackageInfo) -> anyhow::Result<Vec<Report>>;
}

/// How a sync runs.
#[derive(Debug, Clone)]
pub struct Options {
    /// Find what is new and say what would happen; change nothing.
    pub dry_run: bool,
    pub now: DateTime<Utc>,
}

/// What a sync did about one watch entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Internalized now; waits for approval.
    Internalized,
    /// Would be internalized (dry run).
    WouldInternalize,
    /// Internalized earlier and still waiting for approval.
    Pending { since: String },
    /// The target feed serves it.
    Approved,
    /// Needs an override; not tried again until the overrides change.
    NeedsOverride,
    /// Failed; tried again on the next sync.
    Failed,
    /// An override skips it.
    Skipped(String),
    /// The upstream lists no version in range.
    NoVersion,
    /// The upstream could not be asked.
    Error(String),
}

impl Action {
    /// What the action means for the run's exit code.
    pub fn outcome(&self) -> Outcome {
        match self {
            Action::NeedsOverride => Outcome::NeedsAttention,
            Action::Failed | Action::Error(_) => Outcome::Failed,
            _ => Outcome::Done,
        }
    }
}

/// One line of a sync's summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub watch: Watch,
    /// The newest version the entry follows, when there is one.
    pub version: Option<NuGetVersion>,
    pub action: Action,
    pub reasons: Vec<String>,
}

impl std::fmt::Display for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.version {
            Some(v) => write!(f, "{} {}: ", self.watch.id, v.normalized())?,
            None => write!(f, "{}: ", self.watch)?,
        }
        match &self.action {
            Action::Internalized => f.write_str("internalized, waits for approval")?,
            Action::WouldInternalize => f.write_str("would be internalized")?,
            Action::Pending { since } => write!(f, "waits for approval since {since}")?,
            Action::Approved => f.write_str("approved, up to date")?,
            Action::NeedsOverride => {
                f.write_str("needs an override; tried again once the overrides change")?;
            }
            Action::Failed => f.write_str("failed; tried again on the next sync")?,
            Action::Skipped(why) => write!(f, "skipped by its override: {why}")?,
            Action::NoVersion => f.write_str("the upstream lists no version in range")?,
            Action::Error(e) => write!(f, "{e}")?,
        }
        for r in &self.reasons {
            write!(f, "\n  {r}")?;
        }
        Ok(())
    }
}

/// What a sync did.
#[derive(Debug, Clone, Default)]
pub struct Summary {
    pub lines: Vec<Line>,
    pub warnings: Vec<String>,
}

impl Summary {
    pub fn outcome(&self) -> Outcome {
        self.lines
            .iter()
            .map(|l| l.action.outcome())
            .max()
            .unwrap_or(Outcome::Done)
    }
}

/// Bring the target feed up to the watch list: internalize the newest version
/// of each entry that no earlier sync handled, or one that failed, or one
/// that needed an override when the overrides have changed since. `save` is
/// called after each package and at the end, unless it is a dry run.
#[allow(clippy::too_many_arguments)]
pub async fn sync(
    watch: &WatchList,
    upstream: &dyn Source,
    feed: Option<&dyn Feed>,
    overrides: &Overrides,
    internalize: &dyn Internalize,
    state: &mut State,
    options: &Options,
    save: &mut (dyn FnMut(&State) -> std::io::Result<()> + Send),
) -> Result<Summary, SyncError> {
    let now = options.now.to_rfc3339_opts(SecondsFormat::Secs, true);
    let fingerprint = overrides.fingerprint();
    let mut summary = Summary::default();
    let mut unreadable = false;
    let mut save = |state: &State| -> Result<(), SyncError> {
        if options.dry_run {
            return Ok(());
        }
        save(state).map_err(|e| SyncError(format!("cannot save the state: {e}")))
    };

    let mut ids: Vec<String> = watch.entries.iter().map(|w| w.id.clone()).collect();
    ids.sort_by_key(|id| id.to_ascii_lowercase());
    ids.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    let mut handled = BTreeSet::new();

    for id in &ids {
        let entries: Vec<&Watch> = watch
            .entries
            .iter()
            .filter(|w| w.id.eq_ignore_ascii_case(id))
            .collect();
        let versions = match upstream.versions(id).await {
            Ok(v) => v,
            Err(e) => {
                for w in entries {
                    summary.lines.push(Line {
                        watch: w.clone(),
                        version: None,
                        action: Action::Error(format!("cannot ask the upstream: {e}")),
                        reasons: Vec::new(),
                    });
                }
                continue;
            }
        };
        let served = match feed {
            Some(feed) => match feed.served(id).await {
                Ok(Some(v)) => Some(v),
                Ok(None) => {
                    unreadable = true;
                    None
                }
                Err(e) => {
                    summary
                        .warnings
                        .push(format!("{id}: cannot ask the target feed: {e}"));
                    None
                }
            },
            None => None,
        };

        // What the feed serves now is approved, whatever a sync made of it.
        if let Some(served) = &served {
            let lower = id.to_ascii_lowercase();
            for e in state.by_key.values_mut() {
                if e.id.eq_ignore_ascii_case(&lower)
                    && e.status != SyncStatus::Approved
                    && served.contains(&e.version)
                {
                    e.status = SyncStatus::Approved;
                    e.since = now.clone();
                    e.reasons.clear();
                }
            }
        }

        for w in entries {
            let Some(info) = w.newest(&versions) else {
                summary.lines.push(Line {
                    watch: w.clone(),
                    version: None,
                    action: Action::NoVersion,
                    reasons: Vec::new(),
                });
                continue;
            };
            let version = info.version.clone();
            let line = |action, reasons| Line {
                watch: w.clone(),
                version: Some(version.clone()),
                action,
                reasons,
            };
            if !handled.insert((id.to_ascii_lowercase(), version.clone())) {
                // Two ranges with the same newest version: the first said it.
                continue;
            }
            if let Some(why) = overrides
                .get(&info.id, &version)
                .and_then(|o| o.skip.clone())
            {
                summary.lines.push(line(Action::Skipped(why), Vec::new()));
                continue;
            }
            let served_now = served.as_ref().is_some_and(|s| s.contains(&version));
            let earlier = state.get(id, &version).cloned();
            let retry = match &earlier {
                _ if served_now => false,
                None => true,
                Some(e) => match e.status {
                    SyncStatus::Failed => true,
                    SyncStatus::NeedsOverride => {
                        e.overrides.as_deref() != Some(fingerprint.as_str())
                    }
                    SyncStatus::Pending | SyncStatus::Approved => false,
                },
            };
            if !retry {
                let action = match &earlier {
                    _ if served_now => {
                        if earlier.is_none() {
                            state.insert(Entry {
                                id: info.id.clone(),
                                version: version.clone(),
                                status: SyncStatus::Approved,
                                since: now.clone(),
                                attempts: 0,
                                overrides: None,
                                reasons: Vec::new(),
                            });
                        }
                        Action::Approved
                    }
                    Some(e) => match e.status {
                        SyncStatus::Pending => Action::Pending {
                            since: e.since.clone(),
                        },
                        SyncStatus::Approved => Action::Approved,
                        SyncStatus::NeedsOverride => Action::NeedsOverride,
                        SyncStatus::Failed => Action::Failed,
                    },
                    None => unreachable!("a version no sync handled is always tried"),
                };
                let reasons = match &action {
                    Action::NeedsOverride => earlier.map(|e| e.reasons).unwrap_or_default(),
                    _ => Vec::new(),
                };
                summary.lines.push(line(action, reasons));
                continue;
            }
            if options.dry_run {
                summary
                    .lines
                    .push(line(Action::WouldInternalize, Vec::new()));
                continue;
            }

            tracing::info!(id = %info.id, version = %version.normalized(), "internalizing");
            let (status, reasons) = match internalize.internalize(info).await {
                Ok(reports) if reports.is_empty() => (
                    SyncStatus::Failed,
                    vec!["the run gave no report".to_string()],
                ),
                Ok(reports) => {
                    let status = match Outcome::of_all(reports.iter().map(|r| r.status)) {
                        Outcome::Done => SyncStatus::Pending,
                        Outcome::NeedsAttention => SyncStatus::NeedsOverride,
                        Outcome::Failed => SyncStatus::Failed,
                    };
                    (status, reasons_of(&reports))
                }
                Err(e) => (SyncStatus::Failed, vec![format!("{e:#}")]),
            };
            state.insert(Entry {
                id: info.id.clone(),
                version: version.clone(),
                status,
                since: now.clone(),
                attempts: earlier.map_or(0, |e| e.attempts).saturating_add(1),
                overrides: Some(fingerprint.clone()),
                reasons: reasons.clone(),
            });
            save(state)?;
            let action = match status {
                SyncStatus::Pending => Action::Internalized,
                SyncStatus::Approved => Action::Approved,
                SyncStatus::NeedsOverride => Action::NeedsOverride,
                SyncStatus::Failed => Action::Failed,
            };
            summary.lines.push(line(action, reasons));
        }
    }

    prune(state, watch, overrides);
    save(state)?;
    if unreadable {
        summary.warnings.push(
            "the target feed is read-gated and the tool holds a push key only, so it cannot \
             tell when a version is approved; pending versions stay pending"
                .into(),
        );
    }
    Ok(summary)
}

/// The reasons of every package that is not done, each named.
fn reasons_of(reports: &[Report]) -> Vec<String> {
    let mut out = Vec::new();
    for r in reports {
        if matches!(r.status, Status::Internalized | Status::Unchanged) {
            continue;
        }
        let name = format!("{} {}", r.package.id, r.package.version);
        if r.reasons.is_empty() {
            out.push(format!("{name}: {}", r.status.as_str()));
        }
        for reason in &r.reasons {
            out.push(format!("{name}: {reason}"));
        }
    }
    if out.len() > MAX_REASONS {
        let more = out.len() - (MAX_REASONS - 1);
        out.truncate(MAX_REASONS - 1);
        out.push(format!("and {more} more; see the run's report"));
    }
    out
}

/// Drop what the state no longer needs: versions no watch entry follows,
/// versions an override skips, and in each entry's range everything older
/// than its newest approved version.
fn prune(state: &mut State, watch: &WatchList, overrides: &Overrides) {
    state.by_key.retain(|_, e| {
        let skipped = overrides
            .get(&e.id, &e.version)
            .is_some_and(|o| o.skip.is_some());
        let mut followed = watch
            .entries
            .iter()
            .filter(|w| w.id.eq_ignore_ascii_case(&e.id) && w.follows(&e.version))
            .peekable();
        !skipped && followed.peek().is_some()
    });
    let mut drop = Vec::new();
    for w in &watch.entries {
        let in_range = |e: &&Entry| e.id.eq_ignore_ascii_case(&w.id) && w.follows(&e.version);
        let Some(newest_approved) = state
            .by_key
            .values()
            .filter(in_range)
            .filter(|e| e.status == SyncStatus::Approved)
            .map(|e| e.version.clone())
            .max()
        else {
            continue;
        };
        drop.extend(
            state
                .by_key
                .values()
                .filter(in_range)
                .filter(|e| e.version < newest_approved)
                .map(Entry::key),
        );
    }
    for key in drop {
        state.by_key.remove(&key);
    }
}

/// The metrics of a sync in the Prometheus text format, for node_exporter's
/// textfile collector: when it ended, how it ended, the versions per status
/// and how long the oldest pending one has waited.
pub fn metrics(state: &State, outcome: Outcome, now: DateTime<Utc>) -> String {
    let mut text = String::new();
    let mut section = |name: &str, help: &str, samples: Vec<(String, String)>| {
        let _ = writeln!(text, "# HELP {name} {help}");
        let _ = writeln!(text, "# TYPE {name} gauge");
        for (labels, value) in samples {
            let _ = writeln!(text, "{name}{labels} {value}");
        }
    };
    section(
        "yanuget_internalize_sync_last_run_timestamp_seconds",
        "When the last sync ended, as a Unix time.",
        vec![(String::new(), now.timestamp().to_string())],
    );
    section(
        "yanuget_internalize_sync_outcome",
        "How the last sync ended: 1 for its outcome, 0 for the others.",
        [Outcome::Done, Outcome::NeedsAttention, Outcome::Failed]
            .into_iter()
            .map(|o| {
                (
                    format!("{{outcome=\"{}\"}}", o.as_str()),
                    u8::from(o == outcome).to_string(),
                )
            })
            .collect(),
    );
    section(
        "yanuget_internalize_sync_versions",
        "Versions in the sync's state, by status.",
        state
            .counts()
            .into_iter()
            .map(|(s, n)| (format!("{{status=\"{}\"}}", s.as_str()), n.to_string()))
            .collect(),
    );
    let waited = state
        .oldest_pending()
        .map_or(0, |(_, since)| (now - since).num_seconds().max(0));
    section(
        "yanuget_internalize_sync_oldest_pending_seconds",
        "How long the oldest pending version has waited for approval; 0 when none waits.",
        vec![(String::new(), waited.to_string())],
    );
    text
}
