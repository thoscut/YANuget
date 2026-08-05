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
    /// Skip any source package larger than this many bytes (default: no limit).
    #[arg(long)]
    max_package_size_bytes: Option<u64>,
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
                // `interval_hours` comes from configuration as a `u64`; the
                // multiplication into seconds would overflow (a panic in debug,
                // a wrap to a tiny period in release — a sweep every few
                // seconds). Saturating keeps an absurd value meaning "never".
                let period = Duration::from_secs(interval.saturating_mul(3600));
                let mut tick = tokio::time::interval(period);
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

    // Rendered here, printed last — after any certificate or configuration
    // warning — so the summary is what is left on screen, not scrolled off it.
    let feed_names: Vec<&str> = feeds.iter().map(|f| f.name.as_str()).collect();
    let banner = banner(
        if config.tls_enabled { "https" } else { "http" },
        addr,
        &feed_names,
        &config.data_dir,
        feeds.iter().any(|f| f.api_keys.is_empty()),
    );

    if config.tls_enabled {
        serve_tls(app, addr, &config, &banner, shutdown_rx).await?;
    } else {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("YANuget listening on http://{addr} (TLS disabled)");
        print!("{banner}");
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

/// Serve over HTTPS, resolving (and if necessary generating a self-signed)
/// certificate, with the same graceful-shutdown behaviour as the HTTP path.
async fn serve_tls(
    app: axum::Router,
    addr: std::net::SocketAddr,
    config: &Config,
    banner: &str,
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
    print!("{banner}");

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
}
