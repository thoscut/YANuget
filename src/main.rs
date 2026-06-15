//! YANuget server binary.

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use yanuget::config::Config;
use yanuget::database::SqliteDatabase;
use yanuget::retention::{self, RetentionPolicy};
use yanuget::storage::FilesystemStorage;
use yanuget::web::{self, AppState};

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

    if config.api_key.is_none() {
        tracing::warn!(
            "no API key configured (YANUGET_API_KEY) — package push and delete are UNAUTHENTICATED"
        );
    }
    if config.max_package_size_bytes.is_none() {
        tracing::info!("package size limit: unlimited (uploads stream to disk)");
    }

    let storage = Arc::new(FilesystemStorage::new(config.storage_path()).await?);
    let db = Arc::new(SqliteDatabase::connect(&config.database_path()).await?);
    let config = Arc::new(config);

    let state = AppState::new(storage, db, config.clone()).await?;

    // Background retention sweep: periodically prune old versions per policy.
    if config.retention.enabled
        && config.retention.interval_hours > 0
        && config.retention.has_limits()
    {
        let storage = state.storage.clone();
        let db = state.db.clone();
        let cfg = config.clone();
        tracing::info!(
            interval_hours = cfg.retention.interval_hours,
            "package retention sweep enabled"
        );
        tokio::spawn(async move {
            let policy = RetentionPolicy::from(&cfg.retention);
            let mut tick =
                tokio::time::interval(Duration::from_secs(cfg.retention.interval_hours * 3600));
            loop {
                tick.tick().await;
                if let Err(e) = retention::prune_all(storage.as_ref(), db.as_ref(), &policy).await {
                    tracing::error!(error = %e, "retention sweep failed");
                }
            }
        });
    }

    let app = web::router(state);
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

    let sans = config
        .base_url
        .as_deref()
        .and_then(host_of)
        .map(|h| vec![h, "localhost".to_string()])
        .unwrap_or_else(|| vec!["localhost".to_string()]);
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

/// Extract the host portion of a base URL for use as a certificate SAN.
fn host_of(base_url: &str) -> Option<String> {
    let after_scheme = base_url.split("://").nth(1).unwrap_or(base_url);
    let host = after_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    (!host.is_empty()).then(|| host.to_string())
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
