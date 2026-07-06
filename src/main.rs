//! YANuget server binary.

use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use yanuget::config::{Config, MirrorAuthConfig, MirrorConfig, OverwriteMode};
use yanuget::database::SqliteDatabase;
use yanuget::migrate::MigrateOptions;
use yanuget::retention::{self, RetentionPolicy};
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
    /// Per-request timeout to the source, in seconds.
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Migrate(args)) => run_migrate(cli.config.as_deref(), args).await,
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
            .map(|f| FeedMeta {
                name: f.name.clone(),
                prefix: f.prefix.clone(),
                requires_approval: f.requires_approval,
            })
            .collect::<Vec<_>>(),
    );

    // A single shutdown signal fanned out to every background task and both
    // serve paths, so retention sweeps stop cleanly instead of being abandoned.
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

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
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(interval * 3600));
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            if let Err(e) = retention::prune_all(
                                storage.as_ref(),
                                db.as_ref(),
                                &feed_name,
                                &policy,
                            )
                            .await
                            {
                                tracing::error!(feed = %feed_name, error = %e, "retention sweep failed");
                            }
                        }
                        _ = shutdown.changed() => break,
                    }
                }
            });
        }
    }

    let app = web::build_app(states);
    let addr = config.socket_addr();

    if config.tls_enabled {
        serve_tls(app, addr, &config, shutdown_rx).await?;
    } else {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("YANuget listening on http://{addr} (TLS disabled)");
        tracing::info!("service index: http://{addr}/v3/index.json");
        // `with_connect_info` exposes the peer address so the rate limiter can
        // key on it when no `X-Forwarded-*` header is present.
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(wait_for_shutdown(shutdown_rx))
        .await?;
    }
    Ok(())
}

/// Serve over HTTPS, resolving (and if necessary generating a self-signed)
/// certificate, with the same graceful-shutdown behaviour as the HTTP path.
async fn serve_tls(
    app: axum::Router,
    addr: std::net::SocketAddr,
    config: &Config,
    shutdown_rx: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // Install the ring crypto provider as the process default before any
    // rustls configuration is built.
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("failed to install rustls crypto provider"))?;

    let sans = yanuget::tls::certificate_sans(config.base_url.as_deref());
    let paths =
        yanuget::tls::ensure_certificate(config.tls_pair(), &config.data_dir, &sans).await?;

    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&paths.cert, &paths.key)
        .await
        .map_err(|e| anyhow::anyhow!("failed to load TLS certificate: {e}"))?;

    tracing::info!("YANuget listening on https://{addr}");
    tracing::info!("service index: https://{addr}/v3/index.json");

    let handle = axum_server::Handle::new();
    let shutdown = handle.clone();
    tokio::spawn(async move {
        wait_for_shutdown(shutdown_rx).await;
        shutdown.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
    });

    axum_server::bind_rustls(addr, tls)
        .handle(handle)
        .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await?;
    Ok(())
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

    // Surface a non-zero exit only when nothing at all got through.
    if summary.failed > 0 && summary.imported == 0 && summary.skipped == 0 {
        anyhow::bail!(
            "migration failed: {} error(s), nothing imported",
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
    }
}

/// Resolve once the shutdown signal has been broadcast on `rx`.
async fn wait_for_shutdown(mut rx: tokio::sync::watch::Receiver<bool>) {
    if *rx.borrow_and_update() {
        return;
    }
    let _ = rx.changed().await;
}

/// Wait for Ctrl-C (or SIGTERM on Unix) for graceful shutdown.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutting down");
}
