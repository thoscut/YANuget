//! Server configuration.
//!
//! Configuration is layered: built-in defaults, then an optional TOML file,
//! then environment variables (`YANUGET_*`). Environment variables win so the
//! server is easy to configure in containers.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Overwrite policy for re-pushing an existing package id/version.
///
/// Deserializes from either a bool (`true`/`false`, the historical form) or a
/// string (`"true"`, `"false"`, `"prerelease-only"`), so existing configs keep
/// working while the new tri-state is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverwriteMode {
    /// Never overwrite — versions are immutable (the default).
    #[default]
    Disabled,
    /// Overwrite any existing version.
    Enabled,
    /// Overwrite pre-release versions only; stable releases stay immutable.
    PrereleaseOnly,
}

impl OverwriteMode {
    /// Whether a push may overwrite a version with the given pre-release status.
    pub fn allows(self, is_prerelease: bool) -> bool {
        match self {
            OverwriteMode::Disabled => false,
            OverwriteMode::Enabled => true,
            OverwriteMode::PrereleaseOnly => is_prerelease,
        }
    }

    /// A human-readable label for the settings page.
    pub fn label(self) -> &'static str {
        match self {
            OverwriteMode::Disabled => "No",
            OverwriteMode::Enabled => "Yes",
            OverwriteMode::PrereleaseOnly => "Pre-release only",
        }
    }

    /// Parse a free-form environment-variable value (lenient; unknown ⇒ off).
    fn parse_lenient(v: &str) -> OverwriteMode {
        match v
            .trim()
            .to_ascii_lowercase()
            .replace(['_', ' '], "-")
            .as_str()
        {
            "true" | "all" | "enabled" | "yes" | "on" | "1" => OverwriteMode::Enabled,
            "prerelease-only" | "prerelease" | "prereleaseonly" | "pre" => {
                OverwriteMode::PrereleaseOnly
            }
            _ => OverwriteMode::Disabled,
        }
    }
}

impl Serialize for OverwriteMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(match self {
            OverwriteMode::Disabled => "false",
            OverwriteMode::Enabled => "true",
            OverwriteMode::PrereleaseOnly => "prerelease-only",
        })
    }
}

impl<'de> Deserialize<'de> for OverwriteMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct ModeVisitor;
        impl serde::de::Visitor<'_> for ModeVisitor {
            type Value = OverwriteMode;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(r#"a bool or one of "true", "false", "prerelease-only""#)
            }
            fn visit_bool<E>(self, v: bool) -> std::result::Result<OverwriteMode, E> {
                Ok(if v {
                    OverwriteMode::Enabled
                } else {
                    OverwriteMode::Disabled
                })
            }
            fn visit_str<E: serde::de::Error>(
                self,
                v: &str,
            ) -> std::result::Result<OverwriteMode, E> {
                match v
                    .trim()
                    .to_ascii_lowercase()
                    .replace(['_', ' '], "-")
                    .as_str()
                {
                    "true" | "all" | "enabled" => Ok(OverwriteMode::Enabled),
                    "false" | "none" | "disabled" | "" => Ok(OverwriteMode::Disabled),
                    "prerelease-only" | "prerelease" | "prereleaseonly" => {
                        Ok(OverwriteMode::PrereleaseOnly)
                    }
                    other => Err(E::custom(format!("invalid overwrite mode: {other}"))),
                }
            }
        }
        d.deserialize_any(ModeVisitor)
    }
}

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
    /// Additional accepted push/delete keys (e.g. one per team/developer). Any
    /// of these — or `api_key` — authenticates a write.
    pub api_keys: Vec<String>,
    /// Admin key protecting the `/admin` area (disable/enable/delete versions),
    /// presented via HTTP Basic auth. When `None`, the admin area is disabled.
    pub admin_api_key: Option<String>,
    /// Default number of packages shown per gallery page. Overridable per
    /// request with `?take=`.
    pub gallery_page_size: i64,
    /// Maximum accepted upload size in bytes. `None` means unlimited, which is
    /// the point of YANuget — it streams 25 GiB+ packages straight to disk.
    pub max_package_size_bytes: Option<u64>,
    /// Whether (and which) pushes may overwrite an existing id/version. Off by
    /// default to preserve NuGet's immutability guarantee.
    pub allow_overwrite: OverwriteMode,
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
    /// Per-IP request rate limiting (brute-force mitigation).
    pub rate_limit: RateLimitConfig,
    /// Hosted feeds. When empty, a single implicit feed named `default` is
    /// served at the server root (the historical single-feed behaviour). When
    /// non-empty, each feed is mounted under `/{name}` and the root serves a
    /// feed index. A package version can belong to several feeds at once; the
    /// payload and metadata are stored once and referenced by each feed.
    pub feeds: Vec<FeedConfig>,
}

/// Action taken when a package violates a feed's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PolicyAction {
    /// Accept the package but flag the violation (visible in admin/gallery).
    #[default]
    Warn,
    /// Reject the push/mirror outright.
    Block,
}

/// A feed's offline license policy. Evaluated from the package's SPDX license
/// expression (or legacy `licenseUrl`); needs no network access.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LicensePolicyConfig {
    /// Master switch. Off by default.
    pub enabled: bool,
    /// Allowlist of license expressions (case-insensitive). When non-empty, a
    /// package's license must match one of these to pass.
    pub allowed: Vec<String>,
    /// Denylist of license expressions (case-insensitive). A match always fails,
    /// even if also present in `allowed`.
    pub blocked: Vec<String>,
    /// Whether packages that declare no license at all are allowed.
    pub allow_unlicensed: bool,
    /// What to do on a violation (warn-and-flag by default).
    pub action: PolicyAction,
}

impl Default for LicensePolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allowed: Vec::new(),
            blocked: Vec::new(),
            allow_unlicensed: true,
            action: PolicyAction::Warn,
        }
    }
}

/// Authentication for an upstream mirror. All fields are optional; set the
/// `username`/`password` pair for HTTP Basic, `token` for a Bearer token, and/or
/// `headers` for arbitrary custom headers (e.g. a private-feed API key). When
/// more than one is set they are all sent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MirrorAuthConfig {
    /// HTTP Basic username (sent with `password`).
    pub username: Option<String>,
    /// HTTP Basic password.
    pub password: Option<String>,
    /// Bearer token, sent as `Authorization: Bearer <token>`.
    pub token: Option<String>,
    /// Arbitrary extra request headers.
    pub headers: std::collections::BTreeMap<String, String>,
}

impl MirrorAuthConfig {
    /// Whether any credential is configured.
    pub fn is_set(&self) -> bool {
        self.username.is_some() || self.token.is_some() || !self.headers.is_empty()
    }
}

/// Per-feed upstream mirroring (read-through caching of a public NuGet feed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MirrorConfig {
    /// Master switch. Off by default.
    pub enabled: bool,
    /// Upstream V3 service index to mirror from.
    pub upstream: String,
    /// Per-request timeout (seconds) when talking to the upstream.
    pub timeout_secs: u64,
    /// Credentials for an authenticated upstream feed (default: none).
    pub auth: MirrorAuthConfig,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            upstream: "https://api.nuget.org/v3/index.json".to_string(),
            timeout_secs: 30,
            auth: MirrorAuthConfig::default(),
        }
    }
}

/// A configured feed. Most fields are optional and fall back to the matching
/// top-level setting when unset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedConfig {
    /// URL slug and database key. Must be a non-empty, path-safe token.
    pub name: String,
    /// API key for pushing/promoting into this feed (put). Falls back to the
    /// global `api_key`/`api_keys`.
    pub api_key: Option<String>,
    /// Additional accepted push keys for this feed. When this feed sets any push
    /// key (here or in `api_key`), the global keys do not apply to it.
    pub api_keys: Vec<String>,
    /// Credential required to download/restore from this feed (get). When unset,
    /// reads are open. Accepted as either an `X-NuGet-ApiKey` header or the
    /// password of HTTP Basic credentials (what `dotnet`/`nuget` send).
    pub read_api_key: Option<String>,
    /// Admin key gating moderation/promotion (delete) for this feed. Falls back
    /// to the global `admin_api_key`.
    pub admin_api_key: Option<String>,
    /// Overwrite policy; falls back to the global `allow_overwrite`.
    pub allow_overwrite: Option<OverwriteMode>,
    /// Hard-delete policy; falls back to the global `hard_delete_enabled`.
    pub hard_delete_enabled: Option<bool>,
    /// When true, versions entering this feed (push, promote or mirror) are
    /// *pending* and withheld from clients until an admin approves them. This is
    /// what turns a feed into a release-ring gate.
    pub requires_approval: bool,
    /// The next ring: the feed an admin can promote a version into. Feeds without
    /// this are simply independent.
    pub promotes_to: Option<String>,
    /// Upstream mirroring for this feed.
    pub mirror: MirrorConfig,
    /// Offline license policy for this feed.
    pub license_policy: LicensePolicyConfig,
    /// Retention for this feed; falls back to the global `[retention]`.
    pub retention: Option<RetentionConfig>,
}

/// The default feed name used when no `[[feeds]]` are configured.
pub const DEFAULT_FEED: &str = "default";

/// A feed with all fallbacks resolved against the global config, ready to wire
/// into an [`AppState`](crate::web::AppState).
#[derive(Debug, Clone)]
pub struct ResolvedFeed {
    /// Database key / slug.
    pub name: String,
    /// URL path prefix: `""` for the implicit default feed, else `/{name}`.
    pub prefix: String,
    /// Accepted push/delete keys (empty means writes are open).
    pub api_keys: Vec<String>,
    pub read_api_key: Option<String>,
    pub admin_api_key: Option<String>,
    pub allow_overwrite: OverwriteMode,
    pub hard_delete_enabled: bool,
    pub requires_approval: bool,
    pub promotes_to: Option<String>,
    pub mirror: MirrorConfig,
    pub license_policy: LicensePolicyConfig,
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

/// Per-IP request rate limiting. A fixed window of `window_secs` allows at most
/// `max_requests` requests per client IP, after which requests are answered with
/// `429 Too Many Requests` until the window rolls over. The client IP is taken
/// from `X-Forwarded-For`/`X-Real-IP` (for proxied deployments) and otherwise
/// the peer address. On by default with a generous limit so ordinary restores
/// are unaffected while online key brute-forcing is throttled; a reverse proxy
/// can complement it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RateLimitConfig {
    /// Master switch.
    pub enabled: bool,
    /// Maximum requests per client IP per window. Clamped to at least 1.
    pub max_requests: u32,
    /// Window length in seconds.
    pub window_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_requests: 1000,
            window_secs: 60,
        }
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
            api_keys: Vec::new(),
            admin_api_key: None,
            gallery_page_size: 20,
            max_package_size_bytes: None,
            allow_overwrite: OverwriteMode::Disabled,
            hard_delete_enabled: false,
            tls_enabled: true,
            tls_cert_path: None,
            tls_key_path: None,
            enable_symbol_server: true,
            enable_web_ui: true,
            primary_client: "choco".to_string(),
            retention: RetentionConfig::default(),
            rate_limit: RateLimitConfig::default(),
            feeds: Vec::new(),
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
        if let Ok(v) = std::env::var("YANUGET_API_KEYS") {
            self.api_keys = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
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
            self.allow_overwrite = OverwriteMode::parse_lenient(&v);
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
        if let Ok(v) = std::env::var("YANUGET_RATELIMIT_ENABLED") {
            self.rate_limit.enabled = truthy(&v);
        }
        if let Ok(v) = std::env::var("YANUGET_RATELIMIT_MAX_REQUESTS") {
            if let Ok(n) = v.parse() {
                self.rate_limit.max_requests = n;
            }
        }
        if let Ok(v) = std::env::var("YANUGET_RATELIMIT_WINDOW_SECS") {
            if let Ok(n) = v.parse() {
                self.rate_limit.window_secs = n;
            }
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

    /// Resolve the hosted feeds, applying global fallbacks.
    ///
    /// With no `[[feeds]]` configured this yields a single implicit feed named
    /// [`DEFAULT_FEED`] mounted at the root, preserving the historical
    /// single-feed behaviour. Otherwise each configured feed is mounted under
    /// `/{name}`.
    pub fn resolved_feeds(&self) -> Result<Vec<ResolvedFeed>> {
        if self.feeds.is_empty() {
            return Ok(vec![ResolvedFeed {
                name: DEFAULT_FEED.to_string(),
                prefix: String::new(),
                api_keys: combine_keys(&self.api_key, &self.api_keys),
                read_api_key: None,
                admin_api_key: self.admin_api_key.clone(),
                allow_overwrite: self.allow_overwrite,
                hard_delete_enabled: self.hard_delete_enabled,
                requires_approval: false,
                promotes_to: None,
                mirror: MirrorConfig::default(),
                license_policy: LicensePolicyConfig::default(),
                retention: self.retention.clone(),
            }]);
        }

        let mut seen = std::collections::HashSet::new();
        let mut resolved = Vec::with_capacity(self.feeds.len());
        for f in &self.feeds {
            validate_feed_name(&f.name)?;
            if !seen.insert(f.name.to_ascii_lowercase()) {
                return Err(Error::BadRequest(format!(
                    "duplicate feed name: {}",
                    f.name
                )));
            }
            // A feed that sets any push key of its own uses only those; otherwise
            // it falls back to the global keys.
            let feed_keys = combine_keys(&f.api_key, &f.api_keys);
            let api_keys = if feed_keys.is_empty() {
                combine_keys(&self.api_key, &self.api_keys)
            } else {
                feed_keys
            };
            resolved.push(ResolvedFeed {
                prefix: format!("/{}", f.name),
                name: f.name.clone(),
                api_keys,
                read_api_key: f.read_api_key.clone(),
                admin_api_key: f
                    .admin_api_key
                    .clone()
                    .or_else(|| self.admin_api_key.clone()),
                allow_overwrite: f.allow_overwrite.unwrap_or(self.allow_overwrite),
                hard_delete_enabled: f.hard_delete_enabled.unwrap_or(self.hard_delete_enabled),
                requires_approval: f.requires_approval,
                promotes_to: f.promotes_to.clone(),
                mirror: f.mirror.clone(),
                license_policy: f.license_policy.clone(),
                retention: f
                    .retention
                    .clone()
                    .unwrap_or_else(|| self.retention.clone()),
            });
        }

        // Promotion targets must reference real feeds.
        for f in &resolved {
            if let Some(target) = &f.promotes_to {
                if !resolved.iter().any(|o| o.name == *target) {
                    return Err(Error::BadRequest(format!(
                        "feed {:?} promotes_to unknown feed {:?}",
                        f.name, target
                    )));
                }
            }
        }
        Ok(resolved)
    }
}

/// Validate a feed name: non-empty and made only of URL-path-safe characters so
/// it can be a path segment and a storage/database key.
fn validate_feed_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::BadRequest(format!(
            "invalid feed name {name:?}: use only letters, digits, '-', '_' or '.'"
        )));
    }
    Ok(())
}

fn truthy(v: &str) -> bool {
    matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Combine a single optional key with a list of keys into a deduplicated,
/// non-empty list (order preserved, empties dropped).
fn combine_keys(single: &Option<String>, list: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for k in single
        .iter()
        .map(String::as_str)
        .chain(list.iter().map(String::as_str))
    {
        let k = k.trim();
        if !k.is_empty() && !keys.iter().any(|e| e == k) {
            keys.push(k.to_string());
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.port, 5000);
        assert!(c.max_package_size_bytes.is_none()); // unlimited by default
        assert_eq!(c.allow_overwrite, OverwriteMode::Disabled);
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
        assert_eq!(c.allow_overwrite, OverwriteMode::Enabled);
        assert_eq!(c.max_package_size_bytes, Some(26_843_545_600));
    }

    #[test]
    fn overwrite_mode_parses_bool_and_strings() {
        #[derive(Deserialize)]
        struct W {
            allow_overwrite: OverwriteMode,
        }
        let mode = |s: &str| toml::from_str::<W>(s).unwrap().allow_overwrite;
        assert_eq!(mode("allow_overwrite = true"), OverwriteMode::Enabled);
        assert_eq!(mode("allow_overwrite = false"), OverwriteMode::Disabled);
        assert_eq!(
            mode(r#"allow_overwrite = "prerelease-only""#),
            OverwriteMode::PrereleaseOnly
        );

        // The decision helper.
        assert!(!OverwriteMode::Disabled.allows(true));
        assert!(OverwriteMode::Enabled.allows(false));
        assert!(OverwriteMode::PrereleaseOnly.allows(true));
        assert!(!OverwriteMode::PrereleaseOnly.allows(false));

        // Lenient env parsing.
        assert_eq!(
            OverwriteMode::parse_lenient("prerelease"),
            OverwriteMode::PrereleaseOnly
        );
        assert_eq!(
            OverwriteMode::parse_lenient("garbage"),
            OverwriteMode::Disabled
        );
    }

    #[test]
    fn tls_defaults_on_and_scheme_follows() {
        let c = Config::default();
        assert!(c.tls_enabled);
        assert_eq!(c.scheme(), "https");
        assert!(c.tls_pair().is_none()); // self-signed fallback

        let http = Config {
            tls_enabled: false,
            ..Config::default()
        };
        assert_eq!(http.scheme(), "http");
    }

    #[test]
    fn tls_pair_requires_both_paths() {
        let only_cert = Config {
            tls_cert_path: Some(PathBuf::from("/c.pem")),
            ..Config::default()
        };
        assert!(only_cert.tls_pair().is_none());

        let both = Config {
            tls_cert_path: Some(PathBuf::from("/c.pem")),
            tls_key_path: Some(PathBuf::from("/k.pem")),
            ..Config::default()
        };
        assert_eq!(
            both.tls_pair(),
            Some((PathBuf::from("/c.pem"), PathBuf::from("/k.pem")))
        );
    }

    #[test]
    fn parses_new_toml_options() {
        let toml = r#"
            admin_api_key = "adm"
            gallery_page_size = 7
            tls_enabled = false
            tls_cert_path = "/tls/cert.pem"
            tls_key_path = "/tls/key.pem"

            [retention]
            enabled = true
            keep_latest_stable = 5
            max_age_days = 90
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.admin_api_key.as_deref(), Some("adm"));
        assert_eq!(c.gallery_page_size, 7);
        assert!(!c.tls_enabled);
        assert!(c.tls_pair().is_some());
        assert!(c.retention.enabled);
        assert_eq!(c.retention.keep_latest_stable, Some(5));
        assert_eq!(c.retention.max_age_days, Some(90));
        assert!(c.retention.has_limits());
    }

    #[test]
    fn path_overrides_resolve() {
        let c = Config {
            data_dir: PathBuf::from("/data"),
            ..Config::default()
        };
        assert_eq!(c.storage_path(), PathBuf::from("/data/packages"));
        assert!(c.database_path().ends_with("yanuget.db"));

        let overridden = Config {
            storage_path: Some(PathBuf::from("/mnt/pkgs")),
            database_path: Some("/mnt/db.sqlite".into()),
            ..Config::default()
        };
        assert_eq!(overridden.storage_path(), PathBuf::from("/mnt/pkgs"));
        assert_eq!(overridden.database_path(), "/mnt/db.sqlite");
    }
}
