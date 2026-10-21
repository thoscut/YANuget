//! `sync` (`INT-27`) against a fake upstream, target feed and pipeline: what
//! it picks, what it remembers, when it tries again and what it forgets.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use reqwest::Url;
use yanuget_internalize::closure::Source;
use yanuget_internalize::outcome::Outcome;
use yanuget_internalize::overrides::Overrides;
use yanuget_internalize::sync::{
    self, Action, Feed, Internalize, Options, State, Summary, SyncStatus, WatchList,
};
use yanuget_internalize::upstream::{DependencyRef, PackageInfo, UpstreamError};
use yanuget_pkg::report::{PackageRef, Report, Status, Upstream as ReportUpstream};
use yanuget_pkg::NuGetVersion;

fn v(s: &str) -> NuGetVersion {
    NuGetVersion::parse(s).unwrap()
}

fn info(id: &str, version: &str, listed: bool) -> PackageInfo {
    PackageInfo {
        id: id.to_string(),
        version: v(version),
        listed,
        hash: None,
        size: None,
        dependencies: Vec::new(),
        download_url: Url::parse("https://upstream.example/p.nupkg").unwrap(),
    }
}

/// The upstream: listed versions by id, and ids that fail.
#[derive(Default)]
struct FakeUpstream {
    versions: Mutex<HashMap<String, Vec<PackageInfo>>>,
    broken: Vec<String>,
}

impl FakeUpstream {
    fn with(packages: &[(&str, &[&str])]) -> Self {
        let up = FakeUpstream::default();
        for (id, versions) in packages {
            up.set(id, versions);
        }
        up
    }

    fn set(&self, id: &str, versions: &[&str]) {
        let infos = versions
            .iter()
            .map(|s| match s.strip_suffix('!') {
                Some(unlisted) => info(id, unlisted, false),
                None => info(id, s, true),
            })
            .collect();
        self.versions
            .lock()
            .unwrap()
            .insert(id.to_ascii_lowercase(), infos);
    }
}

#[async_trait]
impl Source for FakeUpstream {
    async fn versions(&self, id: &str) -> Result<Vec<PackageInfo>, UpstreamError> {
        if self.broken.iter().any(|b| b.eq_ignore_ascii_case(id)) {
            return Err(UpstreamError::Invalid("the upstream answers 500".into()));
        }
        Ok(self
            .versions
            .lock()
            .unwrap()
            .get(&id.to_ascii_lowercase())
            .cloned()
            .unwrap_or_default())
    }

    async fn dependencies(&self, _: &PackageInfo) -> Result<Vec<DependencyRef>, UpstreamError> {
        Ok(Vec::new())
    }
}

/// The target feed: the versions it serves, or `None` when read-gated.
#[derive(Default)]
struct FakeFeed {
    served: Mutex<HashMap<String, Vec<NuGetVersion>>>,
    gated: bool,
}

impl FakeFeed {
    fn approve(&self, id: &str, version: &str) {
        self.served
            .lock()
            .unwrap()
            .entry(id.to_ascii_lowercase())
            .or_default()
            .push(v(version));
    }
}

#[async_trait]
impl Feed for FakeFeed {
    async fn served(&self, id: &str) -> Result<Option<Vec<NuGetVersion>>, String> {
        if self.gated {
            return Ok(None);
        }
        Ok(Some(
            self.served
                .lock()
                .unwrap()
                .get(&id.to_ascii_lowercase())
                .cloned()
                .unwrap_or_default(),
        ))
    }
}

/// The pipeline: what each root gives, `internalized` unless told.
#[derive(Default)]
struct FakePipeline {
    calls: Mutex<Vec<String>>,
    results: Mutex<HashMap<String, Result<Status, String>>>,
}

impl FakePipeline {
    fn will(&self, key: &str, result: Result<Status, &str>) {
        self.results
            .lock()
            .unwrap()
            .insert(key.to_string(), result.map_err(str::to_string));
    }

    fn calls(&self) -> Vec<String> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

#[async_trait]
impl Internalize for FakePipeline {
    async fn internalize(&self, root: &PackageInfo) -> anyhow::Result<Vec<Report>> {
        let key = format!("{}@{}", root.id, root.version.normalized());
        self.calls.lock().unwrap().push(key.clone());
        let result = self
            .results
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .unwrap_or(Ok(Status::Internalized));
        let status = result.map_err(|e| anyhow::anyhow!(e))?;
        let report = |id: &str, status: Status, reasons: Vec<String>| Report {
            schema: yanuget_pkg::report::SCHEMA_VERSION,
            package: PackageRef {
                id: id.to_string(),
                version: root.version.normalized(),
            },
            status,
            reasons,
            upstream: ReportUpstream {
                source: "https://upstream.example/".into(),
                sha512: None,
            },
            environment: None,
            downloads: Vec::new(),
            warnings: Vec::new(),
            overrides: Vec::new(),
            prelude: None,
            generated_at: None,
        };
        // The root is fine; a dependency carries the status, as a closure
        // whose dependency needs an override does.
        Ok(vec![
            report(&root.id, Status::Unchanged, Vec::new()),
            report(
                &format!("{}.install", root.id),
                status,
                match status {
                    Status::Internalized | Status::Unchanged => Vec::new(),
                    _ => vec!["downloads outside the helpers".into()],
                },
            ),
        ])
    }
}

/// One sync's world, with a clock that moves a day per sync.
struct World {
    watch: WatchList,
    upstream: FakeUpstream,
    feed: FakeFeed,
    pipeline: FakePipeline,
    overrides: Overrides,
    state: State,
    now: DateTime<Utc>,
    saves: usize,
}

impl World {
    fn new(watch: &str, upstream: FakeUpstream) -> Self {
        World {
            watch: WatchList::parse(watch).unwrap(),
            upstream,
            feed: FakeFeed::default(),
            pipeline: FakePipeline::default(),
            overrides: Overrides::none(),
            state: State::default(),
            now: DateTime::parse_from_rfc3339("2026-10-19T08:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            saves: 0,
        }
    }

    async fn sync(&mut self, dry_run: bool) -> Summary {
        self.now += Duration::days(1);
        let mut saves = 0;
        let mut save = |_: &State| {
            saves += 1;
            Ok(())
        };
        let summary = sync::sync(
            &self.watch,
            &self.upstream,
            Some(&self.feed),
            &self.overrides,
            &self.pipeline,
            &mut self.state,
            &Options {
                dry_run,
                now: self.now,
            },
            &mut save,
        )
        .await
        .unwrap();
        self.saves = saves;
        summary
    }

    fn status(&self, id: &str, version: &str) -> Option<SyncStatus> {
        self.state.get(id, &v(version)).map(|e| e.status)
    }
}

fn actions(summary: &Summary) -> Vec<String> {
    summary.lines.iter().map(ToString::to_string).collect()
}

#[test]
fn the_watch_list_parses_and_lists_every_mistake() {
    let w = WatchList::parse(
        "[[package]]\nid = \"git\"\n\
         [[package]]\nid = \"dotnet-sdk\"\nversions = \"[8.0,9.0)\"\n\
         [[package]]\nid = \"Dotnet-SDK\"\nversions = \"[9.0,10.0)\"\n\
         [[package]]\nid = \"vlc\"\nversions = \"\"\n",
    )
    .unwrap();
    assert_eq!(w.len(), 4);
    assert!(w.entries[3].versions.is_none());
    assert!(WatchList::parse("").unwrap().is_empty());

    let err = WatchList::parse(
        "[[package]]\nid = \"a..b\"\n\
         [[package]]\nid = \"git\"\nversions = \"1.*\"\n\
         [[package]]\nid = \"vlc\"\n\
         [[package]]\nid = \"VLC\"\n",
    )
    .unwrap_err();
    assert!(err.contains("package 1 (a..b): not a package id"), "{err}");
    assert!(err.contains("package 2 (git): versions"), "{err}");
    assert!(err.contains("package 4 (VLC): listed twice"), "{err}");

    assert!(WatchList::parse("[[package]]\nid = \"git\"\nrange = \"1.0\"\n").is_err());
    assert!(WatchList::parse("[[packages]]\nid = \"git\"\n").is_err());
}

#[tokio::test]
async fn the_newest_listed_version_in_range_is_picked() {
    let mut w = World::new(
        "[[package]]\nid = \"git\"\n\
         [[package]]\nid = \"node\"\nversions = \"[20.0,21.0)\"\n\
         [[package]]\nid = \"tool\"\nversions = \"[2.0.0-beta,)\"\n\
         [[package]]\nid = \"gone\"\n",
        FakeUpstream::with(&[
            ("git", &["2.50.0", "2.51.0", "2.52.0!", "2.53.0-rc1"]),
            ("node", &["20.1.0", "20.9.0", "22.0.0"]),
            ("tool", &["1.0.0", "2.0.0-beta.2"]),
            ("gone", &["1.0.0!"]),
        ]),
    );
    let summary = w.sync(true).await;
    assert_eq!(
        actions(&summary),
        [
            "git 2.51.0: would be internalized",
            "gone: the upstream lists no version in range",
            "node 20.9.0: would be internalized",
            "tool 2.0.0-beta.2: would be internalized",
        ]
    );
    // A dry run internalizes and saves nothing.
    assert!(w.pipeline.calls().is_empty());
    assert_eq!(w.saves, 0);
    assert_eq!(w.state, State::default());
    assert_eq!(summary.outcome(), Outcome::Done);
}

#[tokio::test]
async fn a_version_is_internalized_once_and_waits_until_approved() {
    let mut w = World::new(
        "[[package]]\nid = \"git\"\n",
        FakeUpstream::with(&[("git", &["2.51.0"])]),
    );
    let summary = w.sync(false).await;
    assert_eq!(
        actions(&summary),
        ["git 2.51.0: internalized, waits for approval"]
    );
    assert_eq!(w.pipeline.calls(), ["git@2.51.0"]);
    // Saved after the package and at the end.
    assert_eq!(w.saves, 2);
    assert_eq!(w.status("git", "2.51.0"), Some(SyncStatus::Pending));
    let pushed = w.state.get("git", &v("2.51.0")).unwrap().since.clone();
    assert_eq!(pushed, "2026-10-20T08:00:00Z");

    // Waiting: not pushed again, and the age counts from the push.
    let summary = w.sync(false).await;
    assert_eq!(
        actions(&summary),
        ["git 2.51.0: waits for approval since 2026-10-20T08:00:00Z"]
    );
    assert!(w.pipeline.calls().is_empty());
    let metrics = sync::metrics(&w.state, summary.outcome(), w.now);
    assert!(
        metrics.contains("yanuget_internalize_sync_oldest_pending_seconds 86400\n"),
        "{metrics}"
    );
    assert!(metrics.contains("yanuget_internalize_sync_versions{status=\"pending\"} 1\n"));

    // Approved in the feed.
    w.feed.approve("git", "2.51.0");
    let summary = w.sync(false).await;
    assert_eq!(actions(&summary), ["git 2.51.0: approved, up to date"]);
    assert_eq!(w.status("git", "2.51.0"), Some(SyncStatus::Approved));
    let metrics = sync::metrics(&w.state, summary.outcome(), w.now);
    assert!(metrics.contains("yanuget_internalize_sync_oldest_pending_seconds 0\n"));
    assert!(metrics.contains("yanuget_internalize_sync_versions{status=\"approved\"} 1\n"));
    assert!(metrics.contains("yanuget_internalize_sync_outcome{outcome=\"done\"} 1\n"));
    assert!(metrics.contains(&format!(
        "yanuget_internalize_sync_last_run_timestamp_seconds {}\n",
        w.now.timestamp()
    )));
}

#[tokio::test]
async fn a_newer_approved_version_drops_the_older_ones_in_its_range() {
    let mut w = World::new(
        "[[package]]\nid = \"dotnet\"\nversions = \"[8.0,9.0)\"\n\
         [[package]]\nid = \"dotnet\"\nversions = \"[9.0,10.0)\"\n",
        FakeUpstream::with(&[("dotnet", &["8.0.1", "9.0.1"])]),
    );
    w.sync(false).await;
    assert_eq!(w.pipeline.calls(), ["dotnet@8.0.1", "dotnet@9.0.1"]);

    // 8.0.1 is never approved; 8.0.2 comes and is.
    w.upstream.set("dotnet", &["8.0.1", "8.0.2", "9.0.1"]);
    w.sync(false).await;
    assert_eq!(w.pipeline.calls(), ["dotnet@8.0.2"]);
    assert_eq!(w.status("dotnet", "8.0.1"), Some(SyncStatus::Pending));
    w.feed.approve("dotnet", "8.0.2");
    let summary = w.sync(false).await;
    assert_eq!(
        actions(&summary),
        [
            "dotnet 8.0.2: approved, up to date",
            "dotnet 9.0.1: waits for approval since 2026-10-20T08:00:00Z",
        ]
    );
    // 8.0.1 stops counting as pending; the other range keeps its own.
    assert_eq!(w.status("dotnet", "8.0.1"), None);
    assert_eq!(w.status("dotnet", "9.0.1"), Some(SyncStatus::Pending));

    // An id taken off the watch list is forgotten.
    w.watch = WatchList::parse("[[package]]\nid = \"git\"\n").unwrap();
    w.sync(false).await;
    assert_eq!(w.state.entries().count(), 0);
}

#[tokio::test]
async fn a_failure_is_tried_again_and_an_override_need_waits_for_new_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = World::new(
        "[[package]]\nid = \"vlc\"\n[[package]]\nid = \"zoom\"\n",
        FakeUpstream::with(&[("vlc", &["3.0.21"]), ("zoom", &["6.0.0"])]),
    );
    w.pipeline.will("vlc@3.0.21", Ok(Status::NeedsOverride));
    w.pipeline
        .will("zoom@6.0.0", Err("the sandbox did not start"));
    let summary = w.sync(false).await;
    assert_eq!(
        actions(&summary),
        [
            "vlc 3.0.21: needs an override; tried again once the overrides change\n  \
             vlc.install 3.0.21: downloads outside the helpers",
            "zoom 6.0.0: failed; tried again on the next sync\n  the sandbox did not start",
        ]
    );
    assert_eq!(summary.outcome(), Outcome::Failed);
    assert_eq!(w.status("vlc", "3.0.21"), Some(SyncStatus::NeedsOverride));
    assert_eq!(w.status("zoom", "6.0.0"), Some(SyncStatus::Failed));
    assert_eq!(w.pipeline.calls().len(), 2);

    // The failure is tried again; the override need is not, and still says
    // why.
    w.pipeline.will("zoom@6.0.0", Ok(Status::Internalized));
    let summary = w.sync(false).await;
    assert_eq!(w.pipeline.calls(), ["zoom@6.0.0"]);
    assert_eq!(w.state.get("zoom", &v("6.0.0")).unwrap().attempts, 2);
    assert_eq!(summary.outcome(), Outcome::NeedsAttention);
    assert!(actions(&summary)[0].contains("vlc.install 3.0.21: downloads"));

    // Once the overrides change, it is tried again.
    std::fs::write(
        dir.path().join("vlc.install.toml"),
        "package_parameters = \"/NoUpdates\"\n",
    )
    .unwrap();
    w.overrides = Overrides::load(dir.path()).unwrap();
    w.pipeline.will("vlc@3.0.21", Ok(Status::Internalized));
    let summary = w.sync(false).await;
    assert_eq!(w.pipeline.calls(), ["vlc@3.0.21"]);
    assert_eq!(w.status("vlc", "3.0.21"), Some(SyncStatus::Pending));
    assert_eq!(summary.outcome(), Outcome::Done);
}

#[tokio::test]
async fn an_override_that_skips_a_version_drops_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut w = World::new(
        "[[package]]\nid = \"git\"\n",
        FakeUpstream::with(&[("git", &["2.51.0"])]),
    );
    w.sync(false).await;
    assert_eq!(w.pipeline.calls(), ["git@2.51.0"]);
    assert_eq!(w.status("git", "2.51.0"), Some(SyncStatus::Pending));

    // The admin will not approve it.
    std::fs::write(
        dir.path().join("git.toml"),
        "versions = \"[2.51.0]\"\nskip = \"breaks the build agents\"\n",
    )
    .unwrap();
    w.overrides = Overrides::load(dir.path()).unwrap();
    let summary = w.sync(false).await;
    assert_eq!(
        actions(&summary),
        ["git 2.51.0: skipped by its override: breaks the build agents"]
    );
    assert_eq!(w.status("git", "2.51.0"), None);
    assert!(w.pipeline.calls().is_empty());
}

#[tokio::test]
async fn what_the_feed_serves_already_is_not_pushed() {
    let mut w = World::new(
        "[[package]]\nid = \"git\"\n",
        FakeUpstream::with(&[("git", &["2.51.0"])]),
    );
    w.feed.approve("git", "2.51.0");
    let summary = w.sync(false).await;
    assert_eq!(actions(&summary), ["git 2.51.0: approved, up to date"]);
    assert!(w.pipeline.calls().is_empty());
    assert_eq!(w.status("git", "2.51.0"), Some(SyncStatus::Approved));
}

#[tokio::test]
async fn an_upstream_error_fails_its_entry_only() {
    let mut w = World::new(
        "[[package]]\nid = \"broken\"\n[[package]]\nid = \"git\"\n",
        FakeUpstream {
            broken: vec!["broken".into()],
            ..FakeUpstream::with(&[("git", &["2.51.0"])])
        },
    );
    let summary = w.sync(false).await;
    assert_eq!(summary.lines[0].watch.id, "broken");
    assert!(matches!(summary.lines[0].action, Action::Error(_)));
    assert!(actions(&summary)[0].contains("cannot ask the upstream"));
    assert_eq!(w.pipeline.calls(), ["git@2.51.0"]);
    assert_eq!(summary.outcome(), Outcome::Failed);
}

#[tokio::test]
async fn a_read_gated_feed_is_a_warning() {
    let mut w = World::new(
        "[[package]]\nid = \"git\"\n",
        FakeUpstream::with(&[("git", &["2.51.0"])]),
    );
    w.feed.gated = true;
    let summary = w.sync(false).await;
    assert_eq!(w.pipeline.calls(), ["git@2.51.0"]);
    assert!(
        summary.warnings.iter().any(|w| w.contains("read-gated")),
        "{:?}",
        summary.warnings
    );
}

#[test]
fn the_state_survives_a_round_trip_and_refuses_another_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state").join("sync.json");
    assert_eq!(State::load(&path).unwrap(), State::default());

    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut w = World::new(
        "[[package]]\nid = \"git\"\n",
        FakeUpstream::with(&[("git", &["2.51.0"])]),
    );
    rt.block_on(w.sync(false));
    w.state.save(&path).unwrap();
    assert_eq!(State::load(&path).unwrap(), w.state);
    // No temporary file is left behind.
    let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["sync.json"]);

    std::fs::write(&path, r#"{"schema": 2, "versions": []}"#).unwrap();
    let err = State::load(&path).unwrap_err().to_string();
    assert!(err.contains("schema 2"), "{err}");
    std::fs::write(&path, "{").unwrap();
    assert!(State::load(&path).is_err());
}

#[test]
fn only_one_sync_holds_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("sync.json");
    let held = sync::lock(&state).unwrap();
    let err = sync::lock(&state).unwrap_err().to_string();
    assert!(err.contains("another sync"), "{err}");
    drop(held);
    sync::lock(&state).unwrap();
}

#[test]
fn metrics_are_written_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("textfile").join("internalize.prom");
    let now = Utc::now();
    let text = sync::metrics(&State::default(), Outcome::Failed, now);
    sync::write_atomic(&path, text.as_bytes()).unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert_eq!(written, text);
    for status in ["pending", "approved", "needs-override", "failed"] {
        assert!(
            written.contains(&format!(
                "yanuget_internalize_sync_versions{{status=\"{status}\"}} 0\n"
            )),
            "{written}"
        );
    }
    assert!(written.contains("yanuget_internalize_sync_outcome{outcome=\"failed\"} 1\n"));
    assert!(written.contains("yanuget_internalize_sync_outcome{outcome=\"done\"} 0\n"));
    // Every sample has its HELP and TYPE.
    for line in written.lines().filter(|l| !l.starts_with('#')) {
        let name = line.split(['{', ' ']).next().unwrap();
        assert!(
            written.contains(&format!("# TYPE {name} gauge\n")),
            "{name}"
        );
    }
}
