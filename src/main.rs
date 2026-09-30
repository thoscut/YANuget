//! YANuget server binary.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use yanuget::config::{Config, MirrorAuthConfig, MirrorConfig, OverwriteMode};
use yanuget::database::SqliteDatabase;
use yanuget::migrate::MigrateOptions;
use yanuget::retention::RetentionPolicy;
use yanuget::storage::FilesystemStorage;
use yanuget::web::{self, AppState, FeedMeta};

/// Command-line options.
#[derive(Debug, Parser)]
#[command(name = "yanuget", version, about = "A fast, streaming NuGet v3 server")]
struct Cli {
    /// Path to a TOML configuration file. Environment variables (`YANUGET_*`)
    /// override its values.
    #[arg(short, long, env = "YANUGET_CONFIG")]
    config: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

/// Sub-commands. With none given, `yanuget` runs the server.
#[derive(Debug, Subcommand)]
enum Command {
    /// Migrate every package from a source NuGet server into a local feed,
    /// with live progress, ETA and transfer rate.
    Migrate(MigrateArgs),
    /// Probe the readiness endpoint of the server running on this machine
    /// (the configured port and scheme) and exit 0 when it is ready, 1 when
    /// it is not. Meant for container health checks.
    Healthcheck,
}

/// Arguments for `yanuget migrate`.
#[derive(Debug, Args)]
struct MigrateArgs {
    /// Source NuGet V3 service-index URL (e.g. https://host/v3/index.json).
    #[arg(long)]
    source: String,
    /// Target local feed to import into.
    #[arg(long, default_value = "default")]
    feed: String,
    /// HTTP Basic username for the source feed.
    #[arg(long)]
    source_username: Option<String>,
    /// HTTP Basic password for the source feed.
    #[arg(long)]
    source_password: Option<String>,
    /// Bearer token for the source feed.
    #[arg(long)]
    source_token: Option<String>,
    /// Extra source request header as "Name: Value"; may be repeated.
    #[arg(long)]
    source_header: Vec<String>,
    /// Timeout, in seconds, for connecting to the source and for any silence
    /// while it answers. Listing requests must also finish within it; package
    /// downloads may take as long as they need while data keeps arriving.
    #[arg(long, default_value_t = 60)]
    timeout_secs: u64,
    /// Number of packages downloaded and indexed concurrently.
    #[arg(long, default_value_t = 4)]
    concurrency: usize,
    /// Skip pre-release versions (otherwise all versions are migrated).
    #[arg(long)]
    skip_prerelease: bool,
    /// Overwrite versions that already exist in the target feed.
    #[arg(long)]
    overwrite: bool,
    /// Only discover and report what would be migrated; download nothing.
    #[arg(long)]
    dry_run: bool,
    /// Skip any source package larger than this many bytes (default: no limit).
    #[arg(long)]
    max_package_size_bytes: Option<u64>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Migrate(args)) => run_migrate(cli.config.as_deref(), args).await,
        Some(Command::Healthcheck) => run_healthcheck(cli.config.as_deref()).await,
        None => {
            init_tracing();
            run_server(cli.config.as_deref()).await
        }
    }
}

/// Run the HTTP(S) server — the default behaviour when no sub-command is given.
async fn run_server(config_path: Option<&str>) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    tokio::fs::create_dir_all(&config.data_dir).await?;
    tokio::fs::create_dir_all(config.storage_path()).await?;

    let feeds = config.resolved_feeds()?;
    for feed in &feeds {
        if feed.api_keys.is_empty() {
            tracing::warn!(
                feed = %feed.name,
                "no API key configured — package push and delete are UNAUTHENTICATED for this feed"
            );
        }
        warn_short_keys(feed);
    }
    if config.max_package_size_bytes.is_none() {
        tracing::info!("package size limit: unlimited (uploads stream to disk)");
    }
    tracing::info!(
        feeds = feeds.len(),
        names = ?feeds.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        "configured feeds"
    );

    let storage = Arc::new(FilesystemStorage::new(config.storage_path()).await?);
    let db = Arc::new(SqliteDatabase::connect(&config.database_path()).await?);
    let config = Arc::new(config);

    let feeds_meta = Arc::new(
        feeds
            .iter()
            .map(FeedMeta::from_resolved)
            .collect::<Vec<_>>(),
    );

    // A single shutdown signal fanned out to every background task and both
    // serve paths, so retention sweeps stop cleanly instead of being abandoned.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    // Every background task, joined on shutdown so none is cut off mid-write.
    let mut background: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let mut states = Vec::with_capacity(feeds.len());
    for feed in &feeds {
        let state = AppState::for_feed(
            storage.clone(),
            db.clone(),
            config.clone(),
            feed,
            feeds_meta.clone(),
        )
        .await?;
        // Shared with the admin page, so a sweep and a manual cleanup never
        // run at once and the page can say when the last one ran.
        let cleanup = state.feed.cleanup.clone();
        states.push(state);

        // Background retention sweep per feed, when enabled.
        if feed.retention.enabled
            && feed.retention.interval_hours > 0
            && feed.retention.has_limits()
        {
            let storage = storage.clone();
            let db = db.clone();
            let policy = RetentionPolicy::from(&feed.retention);
            let interval = feed.retention.interval_hours;
            let feed_name = feed.name.clone();
            let mut shutdown = shutdown_rx.clone();
            tracing::info!(feed = %feed_name, interval_hours = interval, "retention sweep enabled");
            background.push(tokio::spawn(async move {
                // `interval_hours` comes from configuration as a `u64`; the
                // multiplication into seconds would overflow (a panic in debug,
                // a wrap to a tiny period in release — a sweep every few
                // seconds). Saturating keeps an absurd value meaning "never".
                let period = Duration::from_secs(interval.saturating_mul(3600));
                let mut tick = tokio::time::interval(period);
                loop {
                    tokio::select! {
                        _ = tick.tick() => {}
                        _ = shutdown.changed() => break,
                    }
                    // Not raced against shutdown: dropping a sweep mid-way
                    // abandons it at whatever await it reached, between a
                    // version's files and its rows. It checks for shutdown
                    // between versions instead, and main waits for it.
                    let stopping = shutdown.clone();
                    let stop = move || *stopping.borrow();
                    if let Err(e) = cleanup
                        .sweep(storage.as_ref(), db.as_ref(), &feed_name, &policy, &stop)
                        .await
                    {
                        tracing::error!(feed = %feed_name, error = %e, "retention sweep failed");
                    }
                    if *shutdown.borrow() {
                        break;
                    }
                }
                // Only reached on shutdown. If this task ever ends any other
                // way it panicked, and a dropped `JoinHandle` would swallow
                // that silently — retention would stop for the life of the
                // process while `/settings` kept advertising "every N h".
                tracing::debug!(feed = %feed_name, "retention sweep stopped");
            }));
        }
    }

    background.extend(spawn_file_tasks(
        &config,
        &storage,
        &db,
        &feeds,
        &shutdown_rx,
    ));

    // Versions a failed purge left without any feed: finished off at startup
    // and daily, since nothing else ever revisits them.
    {
        let storage = storage.clone();
        let db = db.clone();
        let mut shutdown = shutdown_rx.clone();
        background.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(24 * 3600));
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = shutdown.changed() => break,
                }
                // Like the retention sweep: a pass that started finishes.
                yanuget::retention::sweep_orphans(storage.as_ref(), db.as_ref()).await;
                if *shutdown.borrow() {
                    break;
                }
            }
        }));
    }

    let app = web::build_app(states);

    // Both paths shut down the same way: stop accepting, give in-flight
    // requests the grace period, then close what is left. The plain-HTTP path
    // used to wait for its last connection indefinitely.
    let handle = axum_server::Handle::new();
    {
        let handle = handle.clone();
        let shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            wait_for_shutdown(shutdown_rx).await;
            handle.graceful_shutdown(Some(yanuget::server::SHUTDOWN_GRACE));
        });
    }
    let limits = yanuget::server::ServeLimits::from_config(&config);
    let mut server = tokio::spawn({
        let (config, handle) = (config.clone(), handle.clone());
        async move { yanuget::server::serve(app, &config, handle, limits).await }
    });

    // Printed once the socket is bound — after any certificate or
    // configuration warning — so the summary is what is left on screen, not
    // scrolled off it, and shows the port actually bound.
    tokio::select! {
        bound = handle.listening() => {
            if let Some(addr) = bound {
                let scheme = config.scheme();
                tracing::info!("YANuget listening on {scheme}://{addr}");
                let feed_names: Vec<&str> = feeds.iter().map(|f| f.name.as_str()).collect();
                print!(
                    "{}",
                    banner(
                        scheme,
                        addr,
                        &feed_names,
                        &config.data_dir,
                        feeds.iter().any(|f| f.api_keys.is_empty()),
                    )
                );
            }
        }
        // Failed before binding (a bad certificate, a port in use).
        result = &mut server => return Ok(result??),
    }
    server.await??;

    // The server has stopped; let background work that was mid-way finish,
    // within the same grace period.
    if tokio::time::timeout(
        yanuget::server::SHUTDOWN_GRACE,
        futures::future::join_all(background),
    )
    .await
    .is_err()
    {
        tracing::warn!("background tasks still running at shutdown; abandoning them");
    }
    Ok(())
}

/// Keys shorter than this are guessable online at the rate limiter's pace.
const MIN_KEY_CHARS: usize = 32;

/// Warn about each short key a feed accepts, naming its role but never the key.
///
/// The rate limiter bounds guessing, it does not prevent it: at the default
/// failed-authentication budget a client still gets tens of thousands of
/// guesses a day. A key of 32 random characters makes that irrelevant; a
/// memorable word does not.
fn warn_short_keys(feed: &yanuget::config::ResolvedFeed) {
    let short = |k: &str| k.chars().count() < MIN_KEY_CHARS;
    let roles = [
        ("push", feed.api_keys.iter().any(|k| short(k))),
        ("read", feed.read_api_key.as_deref().is_some_and(short)),
        ("admin", feed.admin_api_key.as_deref().is_some_and(short)),
    ];
    for (role, is_short) in roles {
        if is_short {
            tracing::warn!(
                feed = %feed.name,
                role,
                "a {role} key is shorter than {MIN_KEY_CHARS} characters; use a long random one"
            );
        }
    }
}

/// The block printed on startup: where the server is, what it is serving, and
/// the one thing that is most likely to be wrong.
///
/// The URLs are the point. The first thing anyone does with a fresh package
/// server is paste its service index into a client, and hunting for the path is
/// a poor first minute. A wildcard bind is shown as `localhost`, because
/// `https://0.0.0.0:5000` is not a URL anything will accept.
fn banner(
    scheme: &str,
    addr: std::net::SocketAddr,
    feeds: &[&str],
    data_dir: &std::path::Path,
    unauthenticated: bool,
) -> String {
    use std::fmt::Write as _;
    use std::io::IsTerminal;

    let colour = std::io::stdout().is_terminal();
    let (bold, dim, warn, off) = if colour {
        ("\x1b[1m", "\x1b[2m", "\x1b[33m", "\x1b[0m")
    } else {
        ("", "", "", "")
    };

    let host = match addr.ip() {
        ip if ip.is_unspecified() => "localhost".to_string(),
        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
        ip => ip.to_string(),
    };
    let base = format!("{scheme}://{host}:{}", addr.port());

    let mut out = format!(
        "\n  {bold}YANuget{off} {dim}{version}{off}\n\n",
        version = env!("CARGO_PKG_VERSION"),
    );
    let mut row = |label: &str, value: &str| {
        let _ = writeln!(out, "    {dim}{label:<15}{off}{value}");
    };
    row("Gallery", &format!("{base}/"));
    row("Service index", &format!("{base}/v3/index.json"));
    row("Documentation", &format!("{base}/docs/"));
    row("Feeds", &feeds.join(", "));
    row("Data", &data_dir.display().to_string());

    if unauthenticated {
        let _ = write!(
            out,
            "\n  {warn}!{off}  No API key configured \u{2014} push and delete are open to anyone \
             who can reach this port.\n     Set {bold}YANUGET_API_KEY{off} to require one.\n"
        );
    }
    out.push('\n');
    out
}

/// Ask the local server whether it is ready, for a container `HEALTHCHECK`.
///
/// The image used to run `curl` against `YANUGET_PORT`, which reported a
/// server whose port was set in the TOML file as unhealthy, and kept `curl` in
/// the runtime image for nothing else. Loading the same configuration the
/// server loads means the probe follows the port and scheme wherever they were
/// set. The certificate is deliberately not verified: the probe is checking
/// this process on loopback, and the default certificate is self-signed.
///
/// An `Err` makes `main` exit with status 1, which is what Docker reads as
/// unhealthy.
async fn run_healthcheck(config_path: Option<&str>) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    let url = healthcheck_url(&config);
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        // Inside Docker's 5 s health-check timeout, so a hung server is
        // reported by this process rather than by Docker killing it.
        .timeout(Duration::from_secs(4))
        // A proxy from the environment has no business carrying a loopback
        // probe.
        .no_proxy()
        .build()?;
    let status = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("no answer from {url}"))?
        .status();
    if !status.is_success() {
        anyhow::bail!("{url}: {status}");
    }
    Ok(())
}

/// The readiness URL of the server this configuration describes, on this
/// machine. A wildcard bind is reached through loopback of the same family;
/// a server bound to one address is only reachable there.
fn healthcheck_url(config: &Config) -> String {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    let ip = match config.host {
        IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    format!(
        "{}://{}/health/ready",
        config.scheme(),
        SocketAddr::new(ip, config.port)
    )
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,yanuget=info,tower_http=info"));
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}

/// Quieter logging for the migrate command so warnings don't fight the progress
/// bars. `RUST_LOG` still overrides this when set.
fn init_tracing_quiet() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,yanuget=warn"));
    let _ = tracing_subscriber::registry()
        .with(fmt::layer().with_writer(std::io::stderr))
        .with(filter)
        .try_init();
}

/// Drive a bulk package migration from a source NuGet server into a local feed.
async fn run_migrate(config_path: Option<&str>, args: MigrateArgs) -> anyhow::Result<()> {
    init_tracing_quiet();

    let config = Config::load(config_path)?;
    tokio::fs::create_dir_all(&config.data_dir).await?;
    tokio::fs::create_dir_all(config.storage_path()).await?;

    let feeds = config.resolved_feeds()?;
    let feed = feeds.iter().find(|f| f.name == args.feed).ok_or_else(|| {
        anyhow::anyhow!(
            "feed '{}' is not configured (known feeds: {})",
            args.feed,
            feeds
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    })?;

    let storage = FilesystemStorage::new(config.storage_path()).await?;
    let db = SqliteDatabase::connect(&config.database_path()).await?;

    // Temp files must share the package store's filesystem so indexing can move
    // each download into place with an atomic rename.
    let temp_dir = config.storage_path().join(".migrate");
    tokio::fs::create_dir_all(&temp_dir).await?;

    let source = build_source_config(&args);
    let opts = MigrateOptions {
        concurrency: args.concurrency,
        include_prerelease: !args.skip_prerelease,
        overwrite: if args.overwrite {
            OverwriteMode::Enabled
        } else {
            feed.allow_overwrite
        },
        dry_run: args.dry_run,
        quiet: false,
    };

    let summary = yanuget::migrate::run(
        &storage,
        &db,
        feed,
        &temp_dir,
        source,
        opts,
        indicatif::ProgressDrawTarget::stderr(),
    )
    .await?;

    // Best-effort cleanup of the scratch directory.
    let _ = tokio::fs::remove_dir_all(&temp_dir).await;

    // Any failure is a non-zero exit, even when everything else got through.
    // Scripts gate on it ("stop the old server once the copy is done"), and a
    // partial copy that exits 0 reads as a finished one. A re-run is cheap: it
    // skips what is already there and retries only what failed.
    if summary.failed > 0 {
        anyhow::bail!(
            "migration incomplete: {} failed (listed above); re-run to retry them, \
             versions already migrated are skipped",
            summary.failed
        );
    }
    Ok(())
}

/// Translate the CLI's source flags into a [`MirrorConfig`] the existing mirror
/// client knows how to authenticate against.
fn build_source_config(args: &MigrateArgs) -> MirrorConfig {
    let mut headers = std::collections::BTreeMap::new();
    for raw in &args.source_header {
        if let Some((name, value)) = raw.split_once(':') {
            headers.insert(name.trim().to_string(), value.trim().to_string());
        }
    }
    MirrorConfig {
        enabled: true,
        upstream: args.source.clone(),
        timeout_secs: args.timeout_secs,
        auth: MirrorAuthConfig {
            username: args.source_username.clone(),
            password: args.source_password.clone(),
            token: args.source_token.clone(),
            headers,
        },
        // A migration is an operator running a command against a source they
        // chose, so a source on the private network is expected and allowed —
        // unlike the read-through mirror, which anonymous requests can trigger.
        allow_private_upstream: true,
        max_package_size_bytes: args.max_package_size_bytes,
        // A migration is meant to copy everything.
        max_versions_per_package: None,
    }
}

/// Resolve once the shutdown signal has been broadcast on `rx`.
/// The attached-files housekeeping: dropping resumable uploads that expired,
/// and — when an inbox is configured — importing what arrived over SSH.
fn spawn_file_tasks(
    config: &Arc<Config>,
    storage: &Arc<FilesystemStorage>,
    db: &Arc<SqliteDatabase>,
    feeds: &[yanuget::config::ResolvedFeed],
    shutdown: &tokio::sync::watch::Receiver<bool>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let mut tasks = Vec::new();
    if !config.files.enabled {
        return tasks;
    }
    let staging = config.storage_path().join(".uploads");

    // Expired uploads: once at startup, then every quarter hour.
    {
        let db = db.clone();
        let staging = staging.clone();
        let mut shutdown = shutdown.clone();
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(15 * 60));
            loop {
                tokio::select! {
                    _ = tick.tick() => { web::sweep_expired_uploads(db.as_ref(), &staging).await; }
                    _ = shutdown.changed() => break,
                }
            }
        }));
    }

    let Some(inbox) = config.files.inbox_dir.clone() else {
        return tasks;
    };
    if let Err(e) = std::fs::create_dir_all(&inbox)
        .map_err(yanuget::Error::from)
        .and_then(|_| yanuget::inbox::check_location(&inbox, &config.storage_path()))
    {
        tracing::error!(inbox = %inbox.display(), error = %e, "file inbox disabled");
        return tasks;
    }
    let names: Vec<String> = feeds.iter().map(|f| f.name.clone()).collect();
    for name in &names {
        let _ = std::fs::create_dir_all(inbox.join(name));
    }
    tracing::info!(inbox = %inbox.display(), every_secs = config.files.inbox_scan_secs, "file inbox enabled");
    let (config, storage, db) = (config.clone(), storage.clone(), db.clone());
    let mut shutdown = shutdown.clone();
    tasks.push(tokio::spawn(async move {
        let period = Duration::from_secs(config.files.inbox_scan_secs.max(5));
        let mut tick = tokio::time::interval(period);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = shutdown.changed() => break,
            }
            // Like the retention sweep, a scan runs to completion rather than
            // being dropped half way through an import.
            let scan = yanuget::inbox::Inbox {
                dir: &inbox,
                storage: storage.as_ref(),
                db: db.as_ref(),
                files: &config.files,
                max_file_size: config.max_file_size_bytes(),
                feeds: &names,
                staging: &staging,
            };
            let report = scan.scan().await;
            if report.imported + report.failed > 0 {
                tracing::info!(
                    imported = report.imported,
                    failed = report.failed,
                    "file inbox scanned"
                );
            }
            if *shutdown.borrow() {
                break;
            }
        }
    }));
    tasks
}

async fn wait_for_shutdown(mut rx: tokio::sync::watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    let _ = rx.changed().await;
}

/// Wait for Ctrl-C (or SIGTERM on Unix) for graceful shutdown.
///
/// A handler that cannot be installed is logged and then simply never fires,
/// rather than panicking this task. Panicking here loses the whole shutdown
/// path: `shutdown_tx` is never sent, the server ignores SIGTERM for the rest
/// of its life, and the orchestrator falls through to SIGKILL — which is
/// exactly the case that leaves a half-written upload behind.
async fn shutdown_signal() {
    let ctrl_c = async {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for Ctrl-C; shutdown must come from SIGTERM");
                std::future::pending::<()>().await
            }
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::error!(error = %e, "cannot listen for SIGTERM; shutdown must come from Ctrl-C");
                std::future::pending::<()>().await
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
    use std::path::Path;

    fn render(addr: SocketAddr, feeds: &[&str], unauthenticated: bool) -> String {
        banner(
            "https",
            addr,
            feeds,
            Path::new("/var/lib/yanuget"),
            unauthenticated,
        )
    }

    #[test]
    fn banner_shows_the_urls_a_client_needs() {
        let out = render(
            SocketAddr::from(([127, 0, 0, 1], 5000)),
            &["default"],
            false,
        );
        assert!(
            out.contains("https://127.0.0.1:5000/v3/index.json"),
            "{out}"
        );
        assert!(out.contains("https://127.0.0.1:5000/docs/"), "{out}");
        assert!(out.contains(env!("CARGO_PKG_VERSION")), "{out}");
        assert!(out.contains("/var/lib/yanuget"), "{out}");
    }

    #[test]
    fn a_wildcard_bind_is_shown_as_a_url_that_works() {
        // `https://0.0.0.0:5000` is not something a client will accept, and
        // pasting it is the obvious thing to do with a printed URL.
        let v4 = render(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 5000),
            &["default"],
            false,
        );
        assert!(v4.contains("https://localhost:5000/"), "{v4}");
        assert!(!v4.contains("0.0.0.0"), "{v4}");

        let v6 = render(
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 5000),
            &["default"],
            false,
        );
        assert!(v6.contains("https://localhost:5000/"), "{v6}");
    }

    #[test]
    fn a_literal_ipv6_address_is_bracketed() {
        let addr = SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5000);
        let out = render(addr, &["default"], false);
        assert!(out.contains("https://[::1]:5000/v3/index.json"), "{out}");
    }

    #[test]
    fn every_feed_is_listed() {
        let out = render(
            SocketAddr::from(([127, 0, 0, 1], 5000)),
            &["dev", "stable"],
            false,
        );
        assert!(out.contains("dev, stable"), "{out}");
    }

    #[test]
    fn an_unauthenticated_feed_is_called_out() {
        let addr = SocketAddr::from(([127, 0, 0, 1], 5000));
        assert!(render(addr, &["default"], true).contains("No API key configured"));
        assert!(!render(addr, &["default"], false).contains("No API key configured"));
    }

    #[test]
    fn the_banner_carries_no_escape_codes_when_not_a_terminal() {
        // Tests capture stdout, so this exercises the non-terminal path — which
        // is also the one that ends up in `docker logs` and journald.
        let out = render(SocketAddr::from(([127, 0, 0, 1], 5000)), &["default"], true);
        assert!(!out.contains('\x1b'), "{out}");
    }

    #[test]
    fn the_healthcheck_follows_the_configured_port_and_scheme() {
        let config = Config {
            port: 8443,
            ..Config::default()
        };
        assert_eq!(
            healthcheck_url(&config),
            "https://127.0.0.1:8443/health/ready"
        );
        let config = Config {
            tls_enabled: false,
            ..config
        };
        assert_eq!(
            healthcheck_url(&config),
            "http://127.0.0.1:8443/health/ready"
        );
    }

    #[test]
    fn the_healthcheck_probes_loopback_for_a_wildcard_bind_and_the_address_otherwise() {
        let v6 = Config {
            host: IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            ..Config::default()
        };
        assert_eq!(healthcheck_url(&v6), "https://[::1]:5000/health/ready");
        let bound = Config {
            host: IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)),
            ..Config::default()
        };
        assert_eq!(
            healthcheck_url(&bound),
            "https://10.1.2.3:5000/health/ready"
        );
    }
}
