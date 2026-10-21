//! The command line.

use std::fmt;
use std::str::FromStr;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand};
use yanuget_pkg::NuGetVersion;

use crate::closure::{self, Closure};
use crate::config::{Config, CONFIG_ENV};
use crate::outcome::Outcome;
use crate::overrides::Overrides;
use crate::sync::{self, State, SyncStatus, WatchList};
use crate::target::Target;
use crate::upstream::Upstream;

/// Internalize Chocolatey packages into a YANuget feed: capture what their
/// scripts download in an offline sandbox, attach the files, and push a copy
/// that installs without internet access.
///
/// Exit codes: 0 done, 1 failed, 2 wrong usage, 3 needs attention (a package
/// needs an override).
#[derive(Debug, Parser)]
#[command(name = "yanuget-internalize", version)]
pub struct Cli {
    /// The configuration file (TOML). Without one, the defaults and the
    /// YANUGET_INTERNALIZE_* environment variables apply.
    #[arg(long, short, global = true, env = CONFIG_ENV, value_name = "PATH")]
    pub config: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Resolve and capture, and report what a run would do. Publishes
    /// nothing.
    Plan(PlanArgs),
    /// Capture, download, rewrite and push to the target feed.
    Run(RunArgs),
    /// Internalize the newest version of each package on the watch list that
    /// no earlier sync handled, and report what waits for approval. Meant for
    /// a timer: idempotent, safe to interrupt, one sync at a time.
    Sync(SyncArgs),
    /// Work with the configuration.
    Config(ConfigArgs),
}

#[derive(Debug, Args)]
pub struct PlanArgs {
    /// Packages as `id` (the newest version) or `id@version`.
    #[arg(required = true, value_name = "PACKAGE")]
    pub packages: Vec<PackageSpec>,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Packages as `id` (the newest version) or `id@version`.
    #[arg(required = true, value_name = "PACKAGE")]
    pub packages: Vec<PackageSpec>,

    /// Replace a version that is still pending in the target feed (needs
    /// `allow_overwrite = "pending-only"` there, YANuget 0.8.0 or later).
    #[arg(long)]
    pub replace_pending: bool,
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    /// Say what a sync would internalize; change nothing, write no state and
    /// no metrics.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Debug, Args)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommand,
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Load the configuration as a run would and check what it points at:
    /// the sandbox host (/dev/kvm, QEMU, the golden images), the directories
    /// and the target feed. Exits 1 on any error.
    Check {
        /// Do not ask the target feed.
        #[arg(long)]
        offline: bool,
    },
}

/// A package on the command line: `id`, or `id@version`. NuGet ids cannot
/// hold `@`, so the split is unambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageSpec {
    pub id: String,
    pub version: Option<NuGetVersion>,
}

impl FromStr for PackageSpec {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (id, version) = match s.split_once('@') {
            Some((id, v)) => {
                let version = NuGetVersion::parse(v)
                    .map_err(|e| format!("{v:?} is not a NuGet version: {e}"))?;
                (id, Some(version))
            }
            None => (s, None),
        };
        if !is_valid_id(id) {
            return Err(format!("{id:?} is not a NuGet package id"));
        }
        Ok(PackageSpec {
            id: id.to_string(),
            version,
        })
    }
}

impl fmt::Display for PackageSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.version {
            Some(v) => write!(f, "{}@{}", self.id, v.normalized()),
            None => f.write_str(&self.id),
        }
    }
}

/// NuGet's rule: runs of `A-Za-z0-9_` joined by single `.` or `-`, at most
/// 100 characters.
pub fn is_valid_id(id: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    !id.is_empty()
        && id.len() <= 100
        && id
            .split(['.', '-'])
            .all(|part| !part.is_empty() && part.bytes().all(word))
}

/// Run the parsed command line.
pub async fn run(cli: Cli) -> anyhow::Result<Outcome> {
    let config_path = cli.config.as_deref();
    match cli.command {
        Command::Config(ConfigArgs {
            command: ConfigCommand::Check { offline },
        }) => {
            let report = crate::check::check(config_path, !offline).await;
            for e in &report.errors {
                println!("error: {e}");
            }
            for w in &report.warnings {
                println!("warning: {w}");
            }
            for n in &report.notes {
                println!("note: {n}");
            }
            if !report.is_ok() {
                bail!(
                    "the configuration has {} error{}",
                    report.errors.len(),
                    if report.errors.len() == 1 { "" } else { "s" }
                );
            }
            println!(
                "configuration ok{}",
                config_path.map_or(String::new(), |p| format!(": {p}"))
            );
            Ok(Outcome::Done)
        }
        Command::Plan(args) => {
            let config = Config::load(config_path).context("cannot load the configuration")?;
            prepare(&config, &args.packages)?;
            let upstream = Upstream::new(&config.upstream, &config.cache_dir)?;
            let target = Target::new(&config.target)?;
            let overrides = match &config.overrides_dir {
                Some(dir) => Overrides::load(dir)?,
                None => Overrides::none(),
            };
            // Metadata only: nothing is downloaded before the plan is shown.
            for spec in &args.packages {
                let root = upstream
                    .resolve(&spec.id, spec.version.as_ref())
                    .await
                    .with_context(|| format!("cannot resolve {spec}"))?;
                let closure = closure::resolve(&upstream, &root)
                    .await
                    .with_context(|| format!("cannot resolve the dependencies of {spec}"))?;
                print_closure(&closure, target.as_ref(), &overrides).await?;
            }
            not_built_yet()
        }
        Command::Run(args) => {
            let config = Config::load(config_path).context("cannot load the configuration")?;
            if config.target.url.is_none() {
                bail!("`run` needs target.url, the feed to push to");
            }
            if config.target.api_key.is_none() {
                bail!("`run` needs target.api_key, a push key of the target feed");
            }
            prepare(&config, &args.packages)?;
            not_built_yet()
        }
        Command::Sync(args) => sync(config_path, args.dry_run).await,
    }
}

/// `sync`: everything but the pipeline itself, which `run` will share.
async fn sync(config_path: Option<&str>, dry_run: bool) -> anyhow::Result<Outcome> {
    let config = Config::load(config_path).context("cannot load the configuration")?;
    let Some(watch_list) = &config.sync.watch_list else {
        bail!("`sync` needs sync.watch_list, the packages to keep current");
    };
    let watch = WatchList::load(watch_list)?;
    if !dry_run {
        if config.target.url.is_none() {
            bail!("`sync` needs target.url, the feed to push to");
        }
        if config.target.api_key.is_none() {
            bail!("`sync` needs target.api_key, a push key of the target feed");
        }
        crate::sandbox::from_config(&config.sandbox)?;
    }
    let state_file = &config.sync.state_file;
    let _lock = if dry_run {
        None
    } else {
        Some(sync::lock(state_file)?)
    };
    let mut state = State::load(state_file)?;
    let upstream = Upstream::new(&config.upstream, &config.cache_dir)?;
    let target = Target::new(&config.target)?;
    let overrides = match &config.overrides_dir {
        Some(dir) => Overrides::load(dir)?,
        None => Overrides::none(),
    };
    let options = sync::Options {
        dry_run,
        now: chrono::Utc::now(),
    };
    let mut save = |s: &State| s.save(state_file);
    let result = sync::sync(
        &watch,
        &upstream,
        target.as_ref().map(|t| t as &dyn sync::Feed),
        &overrides,
        &NotBuilt,
        &mut state,
        &options,
        &mut save,
    )
    .await;

    let outcome = match &result {
        Ok(summary) => summary.outcome(),
        Err(_) => Outcome::Failed,
    };
    if !dry_run {
        if let Some(path) = &config.sync.metrics_file {
            let text = sync::metrics(&state, outcome, chrono::Utc::now());
            if let Err(e) = sync::write_atomic(path, text.as_bytes()) {
                tracing::error!(path = %path.display(), "cannot write the metrics: {e}");
            }
        }
    }
    let summary = result?;
    for line in &summary.lines {
        println!("{line}");
    }
    for w in &summary.warnings {
        tracing::warn!("{w}");
    }
    let pending: Vec<_> = state
        .entries()
        .filter(|e| e.status == SyncStatus::Pending)
        .collect();
    if !pending.is_empty() {
        println!(
            "{} version{} wait{} for approval:",
            pending.len(),
            if pending.len() == 1 { "" } else { "s" },
            if pending.len() == 1 { "s" } else { "" }
        );
        for e in pending {
            println!("  {} {} since {}", e.id, e.version.normalized(), e.since);
        }
    }
    Ok(outcome)
}

/// The pipeline `sync` hands each version to, until a sandbox backend
/// captures.
struct NotBuilt;

#[async_trait::async_trait]
impl sync::Internalize for NotBuilt {
    async fn internalize(
        &self,
        _root: &crate::upstream::PackageInfo,
    ) -> anyhow::Result<Vec<yanuget_pkg::report::Report>> {
        bail!("capturing in the sandbox is not built yet")
    }
}

/// What `plan` and `run` share before they fetch anything: the sandbox has to
/// be able to run here.
fn prepare(config: &Config, packages: &[PackageSpec]) -> anyhow::Result<()> {
    let sandbox = crate::sandbox::from_config(&config.sandbox)?;
    tracing::info!(
        sandbox = sandbox.name(),
        packages = %packages.iter().map(ToString::to_string).collect::<Vec<_>>().join(" "),
        "starting"
    );
    Ok(())
}

/// One root and its dependencies, one package a line, each marked when the
/// target feed serves it already or an override skips it.
async fn print_closure(
    closure: &Closure,
    target: Option<&Target>,
    overrides: &Overrides,
) -> anyhow::Result<()> {
    let mut held = 0;
    let mut unknown = false;
    for (i, p) in closure.packages.iter().enumerate() {
        let holds = match target {
            Some(target) => match target.versions(&p.info.id).await? {
                Some(versions) => versions.contains(&p.info.version),
                None => {
                    unknown = true;
                    false
                }
            },
            None => false,
        };
        let skip = overrides
            .get(&p.info.id, &p.info.version)
            .and_then(|o| o.skip.as_deref());
        held += usize::from(holds || skip.is_some());
        let mark = match (holds, skip) {
            (true, _) => " (the target holds it)".to_string(),
            (false, Some(why)) => format!(" (skipped by its override: {why})"),
            (false, None) => String::new(),
        };
        println!(
            "{}{} {}{mark}",
            if i == 0 { "" } else { "  " },
            p.info.id,
            p.info.version.normalized(),
        );
    }
    for (dependent, range) in &closure.provided {
        println!("  chocolatey {range}: left to the Chocolatey CLI (for {dependent})");
    }
    let total = closure.packages.len();
    println!(
        "{total} package{}, {} to internalize",
        if total == 1 { "" } else { "s" },
        total - held
    );
    if unknown {
        tracing::warn!(
            "the target feed is read-gated and the tool holds a push key only, \
             so it cannot tell what the feed holds already"
        );
    }
    Ok(())
}

fn not_built_yet() -> anyhow::Result<Outcome> {
    bail!("nothing was done: capturing in the sandbox is not built yet")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn package_specs_parse() {
        let p: PackageSpec = "git.install".parse().unwrap();
        assert_eq!(p.id, "git.install");
        assert_eq!(p.version, None);

        let p: PackageSpec = "Git.Install@2.51.0".parse().unwrap();
        assert_eq!(p.to_string(), "Git.Install@2.51.0");
        let p: PackageSpec = "vscode@1.0.0-beta.1".parse().unwrap();
        assert!(p.version.unwrap().is_prerelease());

        for bad in [
            "",
            "@1.0.0",
            "git..install",
            ".git",
            "git@",
            "git@x.y",
            "a b",
            "git@1@2",
        ] {
            assert!(bad.parse::<PackageSpec>().is_err(), "{bad:?}");
        }
        assert!(!is_valid_id(&"a".repeat(101)));
        assert!(is_valid_id(&"a".repeat(100)));
    }

    #[test]
    fn the_command_line_parses() {
        Cli::try_parse_from(["yanuget-internalize", "plan", "git", "7zip@24.9.0"]).unwrap();
        Cli::try_parse_from([
            "yanuget-internalize",
            "--config",
            "x.toml",
            "run",
            "--replace-pending",
            "git",
        ])
        .unwrap();
        Cli::try_parse_from(["yanuget-internalize", "config", "check", "--offline"]).unwrap();
        Cli::try_parse_from(["yanuget-internalize", "sync", "--dry-run"]).unwrap();
        assert!(Cli::try_parse_from(["yanuget-internalize", "sync", "git"]).is_err());
        // A package is required, and a bad one is a usage error.
        assert!(Cli::try_parse_from(["yanuget-internalize", "plan"]).is_err());
        assert!(Cli::try_parse_from(["yanuget-internalize", "plan", "a..b"]).is_err());
    }
}
