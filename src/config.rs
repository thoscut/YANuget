//! Server configuration.
//!
//! Configuration is layered: built-in defaults, then an optional TOML file,
//! then environment variables (`YANUGET_*`). Environment variables win so the
//! server is easy to configure in containers.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Top-level server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Interface to bind to.
    pub host: IpAddr,
    /// TCP port to listen on.
    pub port: u16,
    /// Externally visible base URL (e.g. `https://nuget.example.com`). When
    /// `None`, it is derived per-request from the `Host`/forwarded headers so
    /// the feed works behind reverse proxies without configuration.
    pub base_url: Option<String>,
    /// Root directory for all server data.
    pub data_dir: PathBuf,
    /// Override for the package storage directory (defaults to
    /// `{data_dir}/packages`).
    pub storage_path: Option<PathBuf>,
    /// Override for the SQLite database file (defaults to
    /// `{data_dir}/yanuget.db`).
    pub database_path: Option<String>,
    /// API key required for push/delete. When `None`, those endpoints are open
    /// (a loud warning is logged at startup).
    pub api_key: Option<String>,
    /// Admin key protecting the `/admin` area (disable/enable/delete versions),
    /// presented via HTTP Basic auth. When `None`, the admin area is disabled.
    pub admin_api_key: Option<String>,
    /// Default number of packages shown per gallery page. Overridable per
    /// request with `?take=`.
    pub gallery_page_size: i64,
    /// Maximum accepted upload size in bytes. `None` means unlimited, which is
    /// the point of YANuget — it streams 25 GiB+ packages straight to disk.
    pub max_package_size_bytes: Option<u64>,
    /// Whether pushing an existing id/version overwrites it. Off by default to
    /// preserve NuGet's immutability guarantee.
    pub allow_overwrite: bool,
    /// Whether `DELETE` hard-deletes (true) or merely unlists (false).
    pub hard_delete_enabled: bool,
    /// Serve over HTTPS. On by default; a self-signed certificate is generated
    /// when no `tls_cert_path`/`tls_key_path` is configured. Set to `false` to
    /// serve plain HTTP (e.g. behind a TLS-terminating reverse proxy).
    pub tls_enabled: bool,
    /// PEM certificate (chain) for TLS. When unset, a cached self-signed
    /// certificate under `{data_dir}/tls` is used.
    pub tls_cert_path: Option<PathBuf>,
    /// PEM private key for TLS. Must be set together with `tls_cert_path`.
    pub tls_key_path: Option<PathBuf>,
    /// Whether the symbol server (push `.snupkg`, serve PDBs) is enabled.
    pub enable_symbol_server: bool,
    /// Whether the human-facing web gallery (`/`, `/packages/...`) is enabled.
    pub enable_web_ui: bool,
    /// Which client the gallery shows first in its "install" snippet. One of
    /// `choco`, `dotnet`, `nuget`. Defaults to `choco`.
    pub primary_client: String,
    /// Automatic pruning of old package versions.
    pub retention: RetentionConfig,
}

/// Configuration for the package retention sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    /// Master switch. Off by default — retention is destructive.
    pub enabled: bool,
    /// Also run the policy for a package id immediately after each push.
    pub prune_on_push: bool,
    /// How often the background sweep runs, in hours. `0` disables the sweep
    /// (push-time pruning, if enabled, still applies).
    pub interval_hours: u64,
    /// Keep at most this many of the newest stable versions per id.
    pub keep_latest_stable: Option<usize>,
    /// Keep at most this many of the newest pre-release versions per id.
    pub keep_latest_prerelease: Option<usize>,
    /// Delete versions published more than this many days ago.
    pub max_age_days: Option<u64>,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            prune_on_push: false,
            interval_hours: 24,
            keep_latest_stable: None,
            keep_latest_prerelease: None,
            max_age_days: None,
        }
    }
}

impl RetentionConfig {
    /// Whether any actual limit is configured. With none set, the sweep would
    /// never prune anything, so callers can skip it entirely.
    pub fn has_limits(&self) -> bool {
        self.keep_latest_stable.is_some()
            || self.keep_latest_prerelease.is_some()
            || self.max_age_days.is_some()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: IpAddr::from([0, 0, 0, 0]),
            port: 5000,
            base_url: None,
            data_dir: PathBuf::from("./data"),
            storage_path: None,
            database_path: None,
            api_key: None,
            admin_api_key: None,
            gallery_page_size: 20,
            max_package_size_bytes: None,
            allow_overwrite: false,
            hard_delete_enabled: false,
            tls_enabled: true,
            tls_cert_path: None,
            tls_key_path: None,
            enable_symbol_server: true,
            enable_web_ui: true,
            primary_client: "choco".to_string(),
            retention: RetentionConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration: defaults, overlaid by an optional TOML file, overlaid
    /// by `YANUGET_*` environment variables.
    pub fn load(path: Option<&str>) -> Result<Self> {
        let mut config = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| Error::BadRequest(format!("cannot read config {p}: {e}")))?;
                toml::from_str(&text)
                    .map_err(|e| Error::BadRequest(format!("invalid config {p}: {e}")))?
            }
            None => Config::default(),
        };
        config.apply_env();
        Ok(config)
    }

    fn apply_env(&mut self) {
        if let Ok(v) = std::env::var("YANUGET_HOST") {
            if let Ok(ip) = v.parse() {
                self.host = ip;
            }
        }
        if let Ok(v) = std::env::var("YANUGET_PORT") {
            if let Ok(p) = v.parse() {
                self.port = p;
            }
        }
        if let Ok(v) = std::env::var("YANUGET_BASE_URL") {
            self.base_url = Some(v);
        }
        if let Ok(v) = std::env::var("YANUGET_DATA_DIR") {
            self.data_dir = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("YANUGET_STORAGE_PATH") {
            self.storage_path = Some(PathBuf::from(v));
        }
        if let Ok(v) = std::env::var("YANUGET_DATABASE_PATH") {
            self.database_path = Some(v);
        }
        if let Ok(v) = std::env::var("YANUGET_API_KEY") {
            self.api_key = (!v.is_empty()).then_some(v);
        }
        if let Ok(v) = std::env::var("YANUGET_ADMIN_API_KEY") {
            self.admin_api_key = (!v.is_empty()).then_some(v);
        }
        if let Ok(v) = std::env::var("YANUGET_GALLERY_PAGE_SIZE") {
            if let Ok(n) = v.parse::<i64>() {
                if n > 0 {
                    self.gallery_page_size = n;
                }
            }
        }
        if let Ok(v) = std::env::var("YANUGET_MAX_PACKAGE_SIZE_BYTES") {
            self.max_package_size_bytes = v.parse().ok();
        }
        if let Ok(v) = std::env::var("YANUGET_ALLOW_OVERWRITE") {
            self.allow_overwrite = truthy(&v);
        }
        if let Ok(v) = std::env::var("YANUGET_HARD_DELETE_ENABLED") {
            self.hard_delete_enabled = truthy(&v);
        }
        if let Ok(v) = std::env::var("YANUGET_TLS_ENABLED") {
            self.tls_enabled = truthy(&v);
        }
        if let Ok(v) = std::env::var("YANUGET_TLS_CERT_PATH") {
            self.tls_cert_path = (!v.is_empty()).then(|| PathBuf::from(v));
        }
        if let Ok(v) = std::env::var("YANUGET_TLS_KEY_PATH") {
            self.tls_key_path = (!v.is_empty()).then(|| PathBuf::from(v));
        }
        if let Ok(v) = std::env::var("YANUGET_ENABLE_SYMBOL_SERVER") {
            self.enable_symbol_server = truthy(&v);
        }
        if let Ok(v) = std::env::var("YANUGET_ENABLE_WEB_UI") {
            self.enable_web_ui = truthy(&v);
        }
        if let Ok(v) = std::env::var("YANUGET_PRIMARY_CLIENT") {
            if !v.trim().is_empty() {
                self.primary_client = v.trim().to_ascii_lowercase();
            }
        }
        if let Ok(v) = std::env::var("YANUGET_RETENTION_ENABLED") {
            self.retention.enabled = truthy(&v);
        }
        if let Ok(v) = std::env::var("YANUGET_RETENTION_PRUNE_ON_PUSH") {
            self.retention.prune_on_push = truthy(&v);
        }
        if let Ok(v) = std::env::var("YANUGET_RETENTION_INTERVAL_HOURS") {
            if let Ok(n) = v.parse() {
                self.retention.interval_hours = n;
            }
        }
        if let Ok(v) = std::env::var("YANUGET_RETENTION_KEEP_LATEST_STABLE") {
            self.retention.keep_latest_stable = v.parse().ok();
        }
        if let Ok(v) = std::env::var("YANUGET_RETENTION_KEEP_LATEST_PRERELEASE") {
            self.retention.keep_latest_prerelease = v.parse().ok();
        }
        if let Ok(v) = std::env::var("YANUGET_RETENTION_MAX_AGE_DAYS") {
            self.retention.max_age_days = v.parse().ok();
        }
    }

    /// The socket address to bind.
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }

    /// Resolved package storage directory.
    pub fn storage_path(&self) -> PathBuf {
        self.storage_path
            .clone()
            .unwrap_or_else(|| self.data_dir.join("packages"))
    }

    /// Resolved SQLite database file path.
    pub fn database_path(&self) -> String {
        self.database_path.clone().unwrap_or_else(|| {
            self.data_dir
                .join("yanuget.db")
                .to_string_lossy()
                .into_owned()
        })
    }

    /// An explicit TLS certificate/key pair, when both paths are configured.
    pub fn tls_pair(&self) -> Option<(PathBuf, PathBuf)> {
        match (&self.tls_cert_path, &self.tls_key_path) {
            (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
            _ => None,
        }
    }

    /// The default URL scheme the server is reachable on.
    pub fn scheme(&self) -> &'static str {
        if self.tls_enabled {
            "https"
        } else {
            "http"
        }
    }
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.port, 5000);
        assert!(c.max_package_size_bytes.is_none()); // unlimited by default
        assert!(!c.allow_overwrite);
        assert_eq!(c.storage_path(), PathBuf::from("./data/packages"));
    }

    #[test]
    fn parses_toml() {
        let toml = r#"
            port = 8080
            base_url = "https://nuget.example.com"
            api_key = "secret"
            allow_overwrite = true
            max_package_size_bytes = 26843545600
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.port, 8080);
        assert_eq!(c.base_url.as_deref(), Some("https://nuget.example.com"));
        assert_eq!(c.api_key.as_deref(), Some("secret"));
        assert!(c.allow_overwrite);
        assert_eq!(c.max_package_size_bytes, Some(26_843_545_600));
    }
}
