//! YANuget server binary.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use yanuget::config::Config;
use yanuget::database::SqliteDatabase;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let config = Config::load(cli.config.as_deref())?;
    tokio::fs::create_dir_all(&config.data_dir).await?;
    tokio::fs::create_dir_all(config.storage_path()).await?;

    let feeds = config.resolved_feeds()?;
    for feed in &feeds {
        if feed.api_key.is_none() {
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
            tracing::info!(feed = %feed_name, interval_hours = interval, "retention sweep enabled");
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(interval * 3600));
                loop {
                    tick.tick().await;
                    if let Err(e) =
                        retention::prune_all(storage.as_ref(), db.as_ref(), &feed_name, &policy)
                            .await
                    {
                        tracing::error!(feed = %feed_name, error = %e, "retention sweep failed");
                    }
                }
            });
        }
    }

    let app = web::build_app(states);
    let addr = config.socket_addr();

    if config.tls_enabled {
        serve_tls(app, addr, &config).await?;
    } else {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        tracing::info!("YANuget listening on http://{addr} (TLS disabled)");
        tracing::info!("service index: http://{addr}/v3/index.json");
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
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
        shutdown_signal().await;
        shutdown.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
    });

    axum_server::bind_rustls(addr, tls)
        .handle(handle)
        .serve(app.into_make_service())
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
