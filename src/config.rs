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

    /// Parse an environment-variable value: the TOML spellings plus the
    /// boolean ones. Anything else is an error, never a silent "off".
    fn parse_env(name: &str, v: &str) -> Result<OverwriteMode> {
        match v
            .trim()
            .to_ascii_lowercase()
            .replace(['_', ' '], "-")
            .as_str()
        {
            "true" | "all" | "enabled" | "yes" | "on" | "1" => Ok(OverwriteMode::Enabled),
            "false" | "none" | "disabled" | "no" | "off" | "0" => Ok(OverwriteMode::Disabled),
            "prerelease-only" | "prerelease" | "prereleaseonly" | "pre" => {
                Ok(OverwriteMode::PrereleaseOnly)
            }
            _ => Err(env_error(
                name,
                &format!("must be true, false or prerelease-only, not {v:?}"),
            )),
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
///
/// `Debug` is written out (below) rather than derived, so the keys it holds
/// never reach a log line or a panic message.
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    /// Abort an upload after this many seconds without a single byte arriving.
    /// `0` waits forever. Only silence counts: a slow but moving transfer of a
    /// 25 GiB package is never cut off.
    pub upload_idle_timeout_secs: u64,
    /// Refuse an upload that would leave less than this many bytes free on the
    /// storage volume (`507 Insufficient Storage`). `0` turns the check off.
    pub min_free_disk_bytes: u64,
    /// Concurrent connections the server accepts; one over the cap is closed
    /// as soon as it is accepted. `0` is unlimited. Bounds the sockets (and
    /// file descriptors) that slow or idle clients can hold.
    pub max_connections: usize,
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
    /// Large files (disk images, archives) attached to package versions.
    pub files: FilesConfig,
    /// Peers whose `X-Forwarded-*`/`X-Real-IP` headers are honoured. Those
    /// headers decide the base URL of every absolute package URL handed to
    /// clients and the identity the rate limiter throttles, so they are only
    /// trusted from a proxy the operator vouched for; from anyone else they are
    /// stripped before any handler sees them.
    ///
    /// Each entry is an IP, a CIDR block, `private` (loopback, link-local and
    /// RFC1918/ULA — where reverse proxies actually live), or `*` to trust
    /// every peer.
    ///
    /// **Empty by default**, which trusts nobody. Trusting whole private ranges
    /// out of the box read as convenient — that is where proxies live — but the
    /// most common deployment is an internal feed on a LAN with no proxy at
    /// all, and there every client machine is inside those ranges. Any of them
    /// could then set `X-Forwarded-For` to a fresh value per request and land
    /// in a fresh rate-limit bucket, which is exactly the throttle documented
    /// as the mitigation for online key guessing.
    ///
    /// Behind a proxy, set this to that proxy's address — and prefer setting
    /// `base_url` as well, so the URLs handed to clients do not depend on a
    /// header at all.
    pub trusted_proxies: Vec<String>,
    /// Origins allowed to read this server from a browser, as
    /// `Access-Control-Allow-Origin` values. Empty by default, which sends no
    /// CORS headers at all.
    ///
    /// CORS only ever constrains browsers — a NuGet client is unaffected either
    /// way. Sending `*`, as this used to, meant any web page an employee with
    /// network reach visited could read `/v3/search` and the package bytes
    /// cross-origin, so a feed whose only protection was being on the intranet
    /// had none against a browser. List the origins that genuinely need it, or
    /// `*` to restore the old behaviour deliberately.
    pub cors_allowed_origins: Vec<String>,
    /// Host names this server answers to. A request whose `Host` (or, from a
    /// trusted proxy, `X-Forwarded-Host`) names anything else gets `421
    /// Misdirected Request`. The host of `base_url` is always included, so
    /// setting `base_url` alone restricts the server to it; with neither set,
    /// any host is accepted. `*` accepts any host explicitly.
    ///
    /// This is what stops DNS rebinding: a hostile page can point a name it
    /// controls at an intranet feed's address and read it from a browser, but
    /// the browser still sends the hostile name as `Host`. `/health` and its
    /// siblings answer whatever the host, for probes that use an address.
    pub allowed_hosts: Vec<String>,
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
#[serde(default, deny_unknown_fields)]
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
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct MirrorConfig {
    /// Master switch. Off by default.
    pub enabled: bool,
    /// Upstream V3 service index to mirror from.
    pub upstream: String,
    /// Per-request timeout (seconds) when talking to the upstream.
    pub timeout_secs: u64,
    /// Credentials for an authenticated upstream feed (default: none).
    pub auth: MirrorAuthConfig,
    /// Allow upstream URLs that point at loopback/link-local/private addresses.
    ///
    /// Off by default: the resource URLs the mirror follows come from the
    /// upstream's own service index, so a hostile upstream could otherwise
    /// aim them at the server's network or a cloud metadata endpoint. Turn it
    /// on when the upstream really is a feed on your private network.
    pub allow_private_upstream: bool,
    /// Cap on a single mirrored package, in bytes. `None` inherits the server's
    /// `max_package_size_bytes`; set it here to bound the mirror more tightly
    /// than pushes, since a mirror fetch is triggered by an ordinary read.
    pub max_package_size_bytes: Option<u64>,
    /// Upper bound on how many upstream versions of one package a single
    /// read-through miss will fetch, newest first. A popular upstream id can
    /// have hundreds of versions and tens of gigabytes behind it; without a
    /// bound one anonymous request for it pulls the lot.
    pub max_versions_per_package: Option<usize>,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            upstream: "https://api.nuget.org/v3/index.json".to_string(),
            timeout_secs: 30,
            auth: MirrorAuthConfig::default(),
            allow_private_upstream: false,
            max_package_size_bytes: None,
            max_versions_per_package: Some(50),
        }
    }
}

/// A configured feed. Most fields are optional and fall back to the matching
/// top-level setting when unset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
    /// Package-id prefixes only this feed may bring in (`Contoso.`): every
    /// other feed refuses to push, mirror or migrate an id under one. An id and
    /// version are one namespace across every feed, so without this whoever
    /// stores a version first claims it in all of them.
    pub reserved_id_prefixes: Vec<String>,
}

/// The default feed name used when no `[[feeds]]` are configured.
pub const DEFAULT_FEED: &str = "default";

/// A feed with all fallbacks resolved against the global config, ready to wire
/// into an [`AppState`](crate::web::AppState).
#[derive(Clone)]
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
    /// This feed's own `reserved_id_prefixes`.
    pub reserved_id_prefixes: Vec<String>,
    /// The prefixes every *other* feed reserved, which this one must refuse.
    pub reserved_elsewhere: Vec<ReservedPrefix>,
}

/// A package-id prefix reserved by one feed (`reserved_id_prefixes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservedPrefix {
    pub prefix: String,
    /// The feed it is reserved for.
    pub feed: String,
}

impl ReservedPrefix {
    /// Whether `id` falls under the prefix, ignoring ASCII case. A prefix
    /// ending in `.` also covers the bare id before it (`Contoso.` covers
    /// `Contoso` as well as `Contoso.Utils`).
    pub fn covers(&self, id: &str) -> bool {
        let id = id.as_bytes();
        let prefix = self.prefix.as_bytes();
        let starts = id.len() >= prefix.len() && id[..prefix.len()].eq_ignore_ascii_case(prefix);
        starts
            || prefix
                .strip_suffix(b".")
                .is_some_and(|bare| id.eq_ignore_ascii_case(bare))
    }
}

/// Configuration for the package retention sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
/// the peer address.
///
/// On by default. The limit has to clear a *large restore*, not a typical
/// request rate: a solution with a few hundred packages issues roughly three
/// requests each — registration, flat container, payload — mostly in parallel,
/// and behind corporate NAT or a CI egress gateway every developer shares one
/// bucket. NuGet also treats `429` as terminal: it neither retries nor honours
/// `Retry-After`, so being throttled mid-restore fails the build outright. The
/// default is therefore far above anything legitimate while still bounding
/// online key guessing; a reverse proxy can complement it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Master switch.
    pub enabled: bool,
    /// Maximum requests per client IP per window. Clamped to at least 1.
    pub max_requests: u32,
    /// Window length in seconds.
    pub window_secs: u64,
    /// Failed authentications per client per window: responses of `401` to
    /// requests that carried a credential. Once spent, further credentialed
    /// requests from that client get `429` until the window rolls over. `0`
    /// turns this budget off.
    ///
    /// Much smaller than `max_requests`, because it only has to clear
    /// mistakes: a restore that sends the right key never fails, and one that
    /// sends the wrong key fails on its first request anyway. A `401` to a
    /// request without credentials — the challenge a NuGet client waits for
    /// before sending its key — is not counted.
    pub max_failed_auth: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_requests: 10_000,
            window_secs: 60,
            max_failed_auth: 30,
        }
    }
}

/// Files attached to package versions: the disk images (`.wim`) and archives
/// a package's install script fetches, resumably, at install time.
///
/// A file hangs off one id/version, is stored once however many versions
/// share its content, and goes wherever the version goes — deleted with it,
/// pruned with it, visible in every feed that holds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesConfig {
    /// Accept and serve attached files.
    pub enabled: bool,
    /// Largest file accepted, in bytes. Unset falls back to
    /// `max_package_size_bytes`; both unset means unlimited.
    pub max_file_size_bytes: Option<u64>,
    /// The file extensions accepted, without the dot, case-insensitively.
    pub allowed_extensions: Vec<String>,
    /// How long an unfinished resumable upload may sit before it is dropped.
    pub upload_expiry_hours: u64,
    /// A directory scanned for files dropped over SSH (`scp`, `sftp`,
    /// `rsync`). Unset turns the inbox off.
    pub inbox_dir: Option<PathBuf>,
    /// How often the inbox is scanned, in seconds.
    pub inbox_scan_secs: u64,
}

impl Default for FilesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_file_size_bytes: None,
            allowed_extensions: [
                "wim", "swm", "esd", "iso", "vhd", "vhdx", "zip", "7z", "cab",
            ]
            .map(String::from)
            .to_vec(),
            upload_expiry_hours: 72,
            inbox_dir: None,
            inbox_scan_secs: 30,
        }
    }
}

impl FilesConfig {
    /// Whether `ext` (without the dot) is one of the accepted extensions.
    pub fn allows_extension(&self, ext: &str) -> bool {
        self.allowed_extensions
            .iter()
            .any(|allowed| allowed.trim_start_matches('.').eq_ignore_ascii_case(ext))
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
            upload_idle_timeout_secs: 300,
            min_free_disk_bytes: 2 * 1024 * 1024 * 1024,
            max_connections: 4096,
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
            files: FilesConfig::default(),
            trusted_proxies: Vec::new(),
            cors_allowed_origins: Vec::new(),
            allowed_hosts: Vec::new(),
            feeds: Vec::new(),
        }
    }
}

impl Config {
    /// The largest attached file accepted, if any limit applies.
    pub fn max_file_size_bytes(&self) -> Option<u64> {
        self.files
            .max_file_size_bytes
            .or(self.max_package_size_bytes)
    }

    /// The upload idle limit, or `None` when it is turned off.
    pub fn upload_idle_timeout(&self) -> Option<std::time::Duration> {
        (self.upload_idle_timeout_secs > 0)
            .then(|| std::time::Duration::from_secs(self.upload_idle_timeout_secs))
    }

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
        config.apply_env()?;
        config.validate()?;
        Ok(config)
    }

    fn apply_env(&mut self) -> Result<()> {
        self.apply_env_from(|name| std::env::var(name))
    }

    /// Overlay `YANUGET_*` values, read through `var` (the process environment
    /// in production, a map in tests).
    ///
    /// Every value that is set has to parse. Skipping one that does not, as
    /// this used to, failed *open*: `YANUGET_TLS_ENABLED=enabled` served plain
    /// HTTP, `YANUGET_MAX_PACKAGE_SIZE_BYTES=10G` meant unlimited, and a typo
    /// in `YANUGET_RATELIMIT_ENABLED` turned the limiter off — all without a
    /// word, while the TOML file rejects a misspelled key outright. A setting
    /// believed on and actually off is worse than a server that refuses to
    /// start and says why.
    fn apply_env_from(
        &mut self,
        var: impl Fn(&str) -> std::result::Result<String, std::env::VarError>,
    ) -> Result<()> {
        let get = |name: &str| -> Result<Option<String>> {
            match var(name) {
                Ok(v) => Ok(Some(v)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => {
                    Err(env_error(name, "is not valid UTF-8"))
                }
            }
        };
        let bool_var = |name: &str| -> Result<Option<bool>> {
            get(name)?.map(|v| parse_env_bool(name, &v)).transpose()
        };

        if let Some(v) = get("YANUGET_HOST")? {
            self.host = parse_env_host("YANUGET_HOST", &v)?;
        }
        if let Some(v) = get("YANUGET_PORT")? {
            self.port = parse_env_int("YANUGET_PORT", &v)?;
        }
        if let Some(v) = get("YANUGET_BASE_URL")? {
            self.base_url = (!v.trim().is_empty()).then(|| v.trim().to_string());
        }
        if let Some(v) = get("YANUGET_DATA_DIR")? {
            self.data_dir = PathBuf::from(v);
        }
        if let Some(v) = get("YANUGET_STORAGE_PATH")? {
            self.storage_path = Some(PathBuf::from(v));
        }
        if let Some(v) = get("YANUGET_DATABASE_PATH")? {
            self.database_path = Some(v);
        }
        if let Some(v) = get("YANUGET_API_KEY")? {
            self.api_key = (!v.trim().is_empty()).then(|| v.trim().to_string());
        }
        if let Some(v) = get("YANUGET_API_KEYS")? {
            self.api_keys = split_list(&v);
        }
        if let Some(v) = get("YANUGET_ADMIN_API_KEY")? {
            self.admin_api_key = (!v.trim().is_empty()).then(|| v.trim().to_string());
        }
        if let Some(v) = get("YANUGET_GALLERY_PAGE_SIZE")? {
            let n: i64 = parse_env_int("YANUGET_GALLERY_PAGE_SIZE", &v)?;
            if n < 1 {
                return Err(env_error("YANUGET_GALLERY_PAGE_SIZE", "must be at least 1"));
            }
            self.gallery_page_size = n;
        }
        if let Some(v) = get("YANUGET_MAX_PACKAGE_SIZE_BYTES")? {
            self.max_package_size_bytes = parse_env_opt_int("YANUGET_MAX_PACKAGE_SIZE_BYTES", &v)?;
        }
        if let Some(v) = get("YANUGET_UPLOAD_IDLE_TIMEOUT_SECS")? {
            self.upload_idle_timeout_secs = parse_env_int("YANUGET_UPLOAD_IDLE_TIMEOUT_SECS", &v)?;
        }
        if let Some(v) = get("YANUGET_MIN_FREE_DISK_BYTES")? {
            self.min_free_disk_bytes = parse_env_int("YANUGET_MIN_FREE_DISK_BYTES", &v)?;
        }
        if let Some(v) = get("YANUGET_MAX_CONNECTIONS")? {
            self.max_connections = parse_env_int("YANUGET_MAX_CONNECTIONS", &v)?;
        }
        if let Some(v) = get("YANUGET_ALLOW_OVERWRITE")? {
            self.allow_overwrite = OverwriteMode::parse_env("YANUGET_ALLOW_OVERWRITE", &v)?;
        }
        if let Some(b) = bool_var("YANUGET_HARD_DELETE_ENABLED")? {
            self.hard_delete_enabled = b;
        }
        if let Some(b) = bool_var("YANUGET_TLS_ENABLED")? {
            self.tls_enabled = b;
        }
        if let Some(v) = get("YANUGET_TLS_CERT_PATH")? {
            self.tls_cert_path = (!v.is_empty()).then(|| PathBuf::from(v));
        }
        if let Some(v) = get("YANUGET_TLS_KEY_PATH")? {
            self.tls_key_path = (!v.is_empty()).then(|| PathBuf::from(v));
        }
        if let Some(b) = bool_var("YANUGET_ENABLE_SYMBOL_SERVER")? {
            self.enable_symbol_server = b;
        }
        if let Some(b) = bool_var("YANUGET_ENABLE_WEB_UI")? {
            self.enable_web_ui = b;
        }
        if let Some(v) = get("YANUGET_PRIMARY_CLIENT")? {
            if !v.trim().is_empty() {
                self.primary_client = v.trim().to_ascii_lowercase();
            }
        }
        if let Some(b) = bool_var("YANUGET_RETENTION_ENABLED")? {
            self.retention.enabled = b;
        }
        if let Some(b) = bool_var("YANUGET_RETENTION_PRUNE_ON_PUSH")? {
            self.retention.prune_on_push = b;
        }
        if let Some(v) = get("YANUGET_RETENTION_INTERVAL_HOURS")? {
            self.retention.interval_hours = parse_env_int("YANUGET_RETENTION_INTERVAL_HOURS", &v)?;
        }
        if let Some(v) = get("YANUGET_RETENTION_KEEP_LATEST_STABLE")? {
            self.retention.keep_latest_stable =
                parse_env_opt_int("YANUGET_RETENTION_KEEP_LATEST_STABLE", &v)?;
        }
        if let Some(v) = get("YANUGET_RETENTION_KEEP_LATEST_PRERELEASE")? {
            self.retention.keep_latest_prerelease =
                parse_env_opt_int("YANUGET_RETENTION_KEEP_LATEST_PRERELEASE", &v)?;
        }
        if let Some(v) = get("YANUGET_RETENTION_MAX_AGE_DAYS")? {
            self.retention.max_age_days = parse_env_opt_int("YANUGET_RETENTION_MAX_AGE_DAYS", &v)?;
        }
        if let Some(b) = bool_var("YANUGET_RATELIMIT_ENABLED")? {
            self.rate_limit.enabled = b;
        }
        if let Some(v) = get("YANUGET_RATELIMIT_MAX_REQUESTS")? {
            self.rate_limit.max_requests = parse_env_int("YANUGET_RATELIMIT_MAX_REQUESTS", &v)?;
        }
        if let Some(v) = get("YANUGET_RATELIMIT_WINDOW_SECS")? {
            self.rate_limit.window_secs = parse_env_int("YANUGET_RATELIMIT_WINDOW_SECS", &v)?;
        }
        if let Some(v) = get("YANUGET_RATELIMIT_MAX_FAILED_AUTH")? {
            self.rate_limit.max_failed_auth =
                parse_env_int("YANUGET_RATELIMIT_MAX_FAILED_AUTH", &v)?;
        }
        if let Some(b) = bool_var("YANUGET_FILES_ENABLED")? {
            self.files.enabled = b;
        }
        if let Some(v) = get("YANUGET_FILES_MAX_FILE_SIZE_BYTES")? {
            self.files.max_file_size_bytes =
                parse_env_opt_int("YANUGET_FILES_MAX_FILE_SIZE_BYTES", &v)?;
        }
        if let Some(v) = get("YANUGET_FILES_ALLOWED_EXTENSIONS")? {
            self.files.allowed_extensions = v
                .split(',')
                .map(|s| s.trim().trim_start_matches('.').to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(v) = get("YANUGET_FILES_UPLOAD_EXPIRY_HOURS")? {
            self.files.upload_expiry_hours =
                parse_env_int("YANUGET_FILES_UPLOAD_EXPIRY_HOURS", &v)?;
        }
        if let Some(v) = get("YANUGET_FILES_INBOX_DIR")? {
            self.files.inbox_dir = (!v.trim().is_empty()).then(|| PathBuf::from(v.trim()));
        }
        if let Some(v) = get("YANUGET_FILES_INBOX_SCAN_SECS")? {
            self.files.inbox_scan_secs = parse_env_int("YANUGET_FILES_INBOX_SCAN_SECS", &v)?;
        }
        // Set (even to the empty string) this replaces the list wholesale, so an
        // operator can pin trust to their proxy — or revoke it entirely.
        if let Some(v) = get("YANUGET_TRUSTED_PROXIES")? {
            self.trusted_proxies = split_list(&v);
        }
        if let Some(v) = get("YANUGET_ALLOWED_HOSTS")? {
            self.allowed_hosts = split_list(&v);
        }
        Ok(())
    }

    /// Reject combinations that parse but cannot mean what they say.
    ///
    /// Runs after both layers, so a TOML value and an environment value are
    /// held to the same rules.
    pub fn validate(&self) -> Result<()> {
        if self.rate_limit.enabled && self.rate_limit.window_secs == 0 {
            // A zero-length window resets on every request, so no client ever
            // reaches the limit: the throttle is on in name only.
            return Err(Error::BadRequest(
                "rate_limit.window_secs must be at least 1 (set rate_limit.enabled = false \
                 to turn the limiter off)"
                    .into(),
            ));
        }
        if self.tls_enabled && self.tls_cert_path.is_some() != self.tls_key_path.is_some() {
            // Half a pair used to fall back to the self-signed certificate,
            // silently serving something other than what was configured.
            return Err(Error::BadRequest(
                "tls_cert_path and tls_key_path must be set together".into(),
            ));
        }
        Ok(())
    }

    /// The resolved set of peers allowed to set forwarding headers.
    pub fn trusted_proxies(&self) -> crate::proxy::TrustedProxies {
        crate::proxy::TrustedProxies::new(self.trusted_proxies.iter().map(String::as_str))
    }

    /// The host names requests may carry, lower-cased and without a port, or
    /// `None` when any host is accepted (nothing configured, or `*`).
    pub fn host_allowlist(&self) -> Option<Vec<String>> {
        let mut hosts: Vec<String> = self
            .allowed_hosts
            .iter()
            .map(|h| normalize_host(h))
            .filter(|h| !h.is_empty())
            .collect();
        if hosts.iter().any(|h| h == "*") {
            return None;
        }
        if let Some(base) = &self.base_url {
            let authority = base
                .split_once("://")
                .map_or(base.as_str(), |(_, rest)| rest)
                .split(['/', '?', '#'])
                .next()
                .unwrap_or("");
            // Userinfo is not part of the host.
            let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
            let host = normalize_host(authority);
            if !host.is_empty() {
                hosts.push(host);
            }
        }
        (!hosts.is_empty()).then_some(hosts)
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
                // A single feed has no other feed to reserve anything from.
                reserved_id_prefixes: Vec::new(),
                reserved_elsewhere: Vec::new(),
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
                reserved_id_prefixes: reserved_prefixes(f)?,
                reserved_elsewhere: Vec::new(),
            });
        }

        // Each feed refuses what the others reserved. Two reservations that
        // overlap would leave the ids under the narrower one to nobody.
        let all: Vec<ReservedPrefix> = resolved
            .iter()
            .flat_map(|f| {
                f.reserved_id_prefixes.iter().map(|p| ReservedPrefix {
                    prefix: p.clone(),
                    feed: f.name.clone(),
                })
            })
            .collect();
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                if a.feed != b.feed
                    && (a.covers(&b.prefix)
                        || b.covers(&a.prefix)
                        || a.prefix.eq_ignore_ascii_case(&b.prefix))
                {
                    return Err(Error::BadRequest(format!(
                        "feeds {:?} and {:?} reserve overlapping id prefixes {:?} and {:?}",
                        a.feed, b.feed, a.prefix, b.prefix
                    )));
                }
            }
        }
        for f in &mut resolved {
            f.reserved_elsewhere = all.iter().filter(|r| r.feed != f.name).cloned().collect();
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

/// Written in place of a secret by the `Debug` impls below.
const REDACTED: &str = "<redacted>";

/// Present or not, never the value.
fn redact_opt(v: &Option<String>) -> Option<&'static str> {
    v.as_ref().map(|_| REDACTED)
}

/// Only the scheme, host and port of a URL: what an upstream *is*, without
/// whatever credentials its userinfo, path or query may carry (`/_auth/TOKEN/`
/// and `?code=…` are both common). Anything without a scheme is not shown at
/// all.
pub fn url_origin(url: &str) -> String {
    let Some((scheme, rest)) = url.trim().split_once("://") else {
        return "<not a URL>".to_string();
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    format!("{}://{host}", scheme.to_ascii_lowercase())
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("base_url", &self.base_url)
            .field("data_dir", &self.data_dir)
            .field("storage_path", &self.storage_path)
            .field("database_path", &self.database_path)
            .field("api_key", &redact_opt(&self.api_key))
            .field("api_keys", &self.api_keys.len())
            .field("admin_api_key", &redact_opt(&self.admin_api_key))
            .field("gallery_page_size", &self.gallery_page_size)
            .field("max_package_size_bytes", &self.max_package_size_bytes)
            .field("upload_idle_timeout_secs", &self.upload_idle_timeout_secs)
            .field("min_free_disk_bytes", &self.min_free_disk_bytes)
            .field("max_connections", &self.max_connections)
            .field("allow_overwrite", &self.allow_overwrite)
            .field("hard_delete_enabled", &self.hard_delete_enabled)
            .field("tls_enabled", &self.tls_enabled)
            .field("tls_cert_path", &self.tls_cert_path)
            .field("tls_key_path", &self.tls_key_path)
            .field("enable_symbol_server", &self.enable_symbol_server)
            .field("enable_web_ui", &self.enable_web_ui)
            .field("primary_client", &self.primary_client)
            .field("retention", &self.retention)
            .field("rate_limit", &self.rate_limit)
            .field("files", &self.files)
            .field("trusted_proxies", &self.trusted_proxies)
            .field("cors_allowed_origins", &self.cors_allowed_origins)
            .field("allowed_hosts", &self.allowed_hosts)
            .field(
                "feeds",
                &self
                    .feeds
                    .iter()
                    .map(|f| f.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for MirrorAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorAuthConfig")
            .field("username", &self.username)
            .field("password", &redact_opt(&self.password))
            .field("token", &redact_opt(&self.token))
            // Header names say what kind of credential it is; values are it.
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl std::fmt::Debug for ResolvedFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedFeed")
            .field("name", &self.name)
            .field("prefix", &self.prefix)
            .field("api_keys", &self.api_keys.len())
            .field("read_api_key", &redact_opt(&self.read_api_key))
            .field("admin_api_key", &redact_opt(&self.admin_api_key))
            .field("allow_overwrite", &self.allow_overwrite)
            .field("hard_delete_enabled", &self.hard_delete_enabled)
            .field("requires_approval", &self.requires_approval)
            .field("promotes_to", &self.promotes_to)
            .field("mirror_enabled", &self.mirror.enabled)
            .field("mirror_upstream", &url_origin(&self.mirror.upstream))
            .field("license_policy", &self.license_policy)
            .field("retention", &self.retention)
            .field("reserved_id_prefixes", &self.reserved_id_prefixes)
            .field("reserved_elsewhere", &self.reserved_elsewhere)
            .finish_non_exhaustive()
    }
}

/// A feed's `reserved_id_prefixes`, checked: made of package-id characters
/// and starting with a letter, digit or `_`, so each can match real ids.
fn reserved_prefixes(feed: &FeedConfig) -> Result<Vec<String>> {
    let mut prefixes = Vec::with_capacity(feed.reserved_id_prefixes.len());
    for raw in &feed.reserved_id_prefixes {
        let prefix = raw.trim();
        let valid = prefix
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
            && prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if !valid || prefix.len() > crate::validation::MAX_ID_LENGTH {
            return Err(Error::BadRequest(format!(
                "feed {:?}: invalid reserved_id_prefixes entry {raw:?}",
                feed.name
            )));
        }
        prefixes.push(prefix.to_string());
    }
    Ok(prefixes)
}

/// Route path segments a feed may not shadow. A feed is mounted at `/{name}`,
/// so a feed called `health` or `v3` would collide with (or mask) a real route.
/// `_assets` is where the gallery's font is served, at the root.
const RESERVED_FEED_NAMES: [&str; 13] = [
    "health", "admin", "docs", "packages", "stats", "settings", "v3", "api", "download", "metrics",
    "_assets", "tags", "files",
];

/// Validate a feed name: non-empty, made only of URL-path-safe characters (so it
/// can be a path segment and a storage/database key), not a relative-path token,
/// and not the name of an existing route.
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
    // `.` and `..` pass the character check but are path traversal tokens, and
    // the name becomes a URL prefix (`/..`).
    if name.chars().all(|c| c == '.') {
        return Err(Error::BadRequest(format!(
            "invalid feed name {name:?}: a name of only dots is a relative path"
        )));
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED_FEED_NAMES.contains(&lower.as_str()) {
        return Err(Error::BadRequest(format!(
            "feed name {name:?} is reserved: it would shadow the /{lower} route"
        )));
    }
    Ok(())
}

/// A `Host`-style value reduced to what is compared: lower-case, no port, no
/// IPv6 brackets, no trailing dot.
pub fn normalize_host(value: &str) -> String {
    let v = value.trim();
    let host = if let Some(rest) = v.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        match v.rsplit_once(':') {
            // One colon is a port; more is a bare IPv6 address.
            Some((h, port)) if !h.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => h,
            _ => v,
        }
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

fn env_error(name: &str, why: &str) -> Error {
    Error::BadRequest(format!("environment variable {name} {why}"))
}

/// The spellings a boolean environment variable accepts, case-insensitively.
/// Anything else is refused rather than read as `false`.
fn parse_env_bool(name: &str, v: &str) -> Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(env_error(
            name,
            &format!("must be one of 1/true/yes/on or 0/false/no/off, not {v:?}"),
        )),
    }
}

/// A plain decimal integer: digits only, so `10G`, `1e9`, `+5` and `-1` are
/// errors instead of whatever a lenient parse would make of them.
fn parse_env_int<T: std::str::FromStr>(name: &str, v: &str) -> Result<T> {
    let t = v.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(env_error(
            name,
            &format!("must be a plain whole number, not {v:?}"),
        ));
    }
    t.parse()
        .map_err(|_| env_error(name, &format!("is out of range: {v:?}")))
}

/// Like [`parse_env_int`], with the empty string meaning "unset".
fn parse_env_opt_int<T: std::str::FromStr>(name: &str, v: &str) -> Result<Option<T>> {
    if v.trim().is_empty() {
        Ok(None)
    } else {
        parse_env_int(name, v).map(Some)
    }
}

/// An IP address, or `localhost` for the IPv4 loopback — the one name people
/// reach for, and the one that used to be dropped for `0.0.0.0`, exposing a
/// server meant to be local to the whole network.
fn parse_env_host(name: &str, v: &str) -> Result<IpAddr> {
    let t = v.trim();
    if t.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::from([127, 0, 0, 1]));
    }
    t.parse().map_err(|_| {
        env_error(
            name,
            &format!("must be an IP address or \"localhost\", not {v:?}"),
        )
    })
}

/// A comma-separated environment list, trimmed, empties dropped.
fn split_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
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
    fn a_mistyped_setting_is_an_error_not_a_silent_default() {
        // Silently ignoring an unknown key is how a security setting ends up
        // believed-on and actually off. `admin_api_keys` is not a real key.
        let typo = r#"
            port = 8080
            admin_api_keys = "secret"
        "#;
        let err = toml::from_str::<Config>(typo).unwrap_err().to_string();
        assert!(err.contains("admin_api_keys"), "unhelpful error: {err}");

        // A nested table is checked too.
        let nested = r#"
            [rate_limit]
            enabled = true
            max_request = 5
        "#;
        assert!(toml::from_str::<Config>(nested).is_err());
    }

    #[test]
    fn reserved_id_prefixes_are_refused_by_every_other_feed() {
        let feed = |name: &str, prefixes: &[&str]| FeedConfig {
            name: name.into(),
            reserved_id_prefixes: prefixes.iter().map(|p| p.to_string()).collect(),
            ..Default::default()
        };
        let config = Config {
            feeds: vec![
                feed("internal", &["Contoso."]),
                feed("public", &[]),
                feed("tools", &["Fabrikam.Tools."]),
            ],
            ..Default::default()
        };
        let feeds = config.resolved_feeds().unwrap();
        let elsewhere = |name: &str| {
            let f = feeds.iter().find(|f| f.name == name).unwrap();
            f.reserved_elsewhere
                .iter()
                .map(|r| r.prefix.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(elsewhere("internal"), ["Fabrikam.Tools."]);
        assert_eq!(elsewhere("public"), ["Contoso.", "Fabrikam.Tools."]);

        let contoso = ReservedPrefix {
            prefix: "Contoso.".into(),
            feed: "internal".into(),
        };
        for covered in ["Contoso.Utils", "contoso.utils", "CONTOSO", "contoso.a.b"] {
            assert!(contoso.covers(covered), "{covered:?}");
        }
        for free in ["ContosoUtils", "Contos", "My.Contoso.Utils"] {
            assert!(!contoso.covers(free), "{free:?}");
        }

        // Overlapping reservations would leave the narrower ids to nobody.
        let overlapping = Config {
            feeds: vec![feed("a", &["Contoso."]), feed("b", &["contoso.internal."])],
            ..Default::default()
        };
        assert!(overlapping.resolved_feeds().is_err());
        let invalid = Config {
            feeds: vec![feed("a", &[".Contoso"])],
            ..Default::default()
        };
        assert!(invalid.resolved_feeds().is_err());
    }

    #[test]
    fn feed_names_cannot_traverse_or_shadow_routes() {
        for bad in [".", "..", "...", "health", "ADMIN", "v3", "docs", "metrics"] {
            assert!(
                validate_feed_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        for good in ["stable", "dev", "team-a", "ring_2", "net8.0"] {
            assert!(validate_feed_name(good).is_ok(), "{good:?} should be valid");
        }
    }

    #[test]
    fn no_peer_is_trusted_to_forward_by_default() {
        // The default deployment is a feed on a LAN with no proxy, where
        // trusting private ranges means trusting every client machine — and a
        // client that can set `X-Forwarded-For` can pick its own rate-limit
        // bucket.
        let c = Config::default();
        assert!(c.trusted_proxies().is_empty());

        // Opting in still works, and still refuses a public peer.
        let private = Config {
            trusted_proxies: vec!["private".into()],
            ..Config::default()
        };
        let trusted = private.trusted_proxies();
        assert!(trusted.trusts("127.0.0.1".parse().unwrap()));
        assert!(trusted.trusts("10.9.8.7".parse().unwrap()));
        assert!(!trusted.trusts("198.51.100.4".parse().unwrap()));
    }

    #[test]
    fn the_rate_limit_default_clears_a_large_restore() {
        // ~300 packages x 3 requests, with everyone behind one NAT address,
        // must not hit the ceiling: NuGet treats 429 as terminal.
        let c = RateLimitConfig::default();
        assert!(c.enabled);
        assert!(
            c.max_requests >= 5_000,
            "a large restore would be throttled at {}",
            c.max_requests
        );
    }

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

        // Environment parsing: known spellings only.
        assert_eq!(
            OverwriteMode::parse_env("X", "prerelease").unwrap(),
            OverwriteMode::PrereleaseOnly
        );
        assert_eq!(
            OverwriteMode::parse_env("X", "off").unwrap(),
            OverwriteMode::Disabled
        );
        assert!(OverwriteMode::parse_env("X", "garbage").is_err());
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

    /// Apply `vars` as the environment on top of the defaults.
    fn with_env(vars: &[(&str, &str)]) -> Result<Config> {
        let map: std::collections::HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut c = Config::default();
        c.apply_env_from(|name| map.get(name).cloned().ok_or(std::env::VarError::NotPresent))?;
        c.validate()?;
        Ok(c)
    }

    #[test]
    fn an_environment_value_that_does_not_parse_is_an_error() {
        // Each of these used to be skipped or read as "off", failing open.
        for (name, value) in [
            ("YANUGET_TLS_ENABLED", "enabled"),
            ("YANUGET_RATELIMIT_ENABLED", "ture"),
            ("YANUGET_MAX_PACKAGE_SIZE_BYTES", "10G"),
            ("YANUGET_MAX_PACKAGE_SIZE_BYTES", "+5"),
            ("YANUGET_PORT", "eighty"),
            ("YANUGET_PORT", "70000"),
            ("YANUGET_HOST", "example.com"),
            ("YANUGET_ALLOW_OVERWRITE", "sometimes"),
            ("YANUGET_GALLERY_PAGE_SIZE", "0"),
            ("YANUGET_RETENTION_KEEP_LATEST_STABLE", "-1"),
        ] {
            let err = with_env(&[(name, value)]).unwrap_err().to_string();
            assert!(err.contains(name), "{name}={value}: unhelpful error {err}");
        }
    }

    #[test]
    fn environment_values_in_the_documented_forms_apply() {
        let c = with_env(&[
            ("YANUGET_HOST", "localhost"),
            ("YANUGET_TLS_ENABLED", "Off"),
            ("YANUGET_RATELIMIT_ENABLED", "1"),
            ("YANUGET_MAX_PACKAGE_SIZE_BYTES", " 1048576 "),
            ("YANUGET_ALLOW_OVERWRITE", "prerelease-only"),
            ("YANUGET_RETENTION_MAX_AGE_DAYS", ""),
        ])
        .unwrap();
        assert_eq!(c.host, IpAddr::from([127, 0, 0, 1]));
        assert!(!c.tls_enabled);
        assert!(c.rate_limit.enabled);
        assert_eq!(c.max_package_size_bytes, Some(1_048_576));
        assert_eq!(c.allow_overwrite, OverwriteMode::PrereleaseOnly);
        assert_eq!(c.retention.max_age_days, None);
    }

    #[test]
    fn a_zero_window_and_half_a_tls_pair_are_refused() {
        let err = with_env(&[("YANUGET_RATELIMIT_WINDOW_SECS", "0")]).unwrap_err();
        assert!(err.to_string().contains("window_secs"), "{err}");
        // With the limiter off, the window does not matter.
        assert!(with_env(&[
            ("YANUGET_RATELIMIT_WINDOW_SECS", "0"),
            ("YANUGET_RATELIMIT_ENABLED", "false"),
        ])
        .is_ok());

        let err = with_env(&[("YANUGET_TLS_CERT_PATH", "/c.pem")]).unwrap_err();
        assert!(err.to_string().contains("tls_key_path"), "{err}");
        assert!(with_env(&[
            ("YANUGET_TLS_CERT_PATH", "/c.pem"),
            ("YANUGET_TLS_KEY_PATH", "/k.pem"),
        ])
        .is_ok());
    }

    #[test]
    fn the_host_allowlist_follows_base_url_and_allowed_hosts() {
        assert_eq!(Config::default().host_allowlist(), None);
        let c = Config {
            base_url: Some("https://user:pw@NuGet.Example.com:8443/feed".into()),
            ..Config::default()
        };
        assert_eq!(c.host_allowlist(), Some(vec!["nuget.example.com".into()]));
        let c = Config {
            base_url: Some("https://nuget.example.com".into()),
            allowed_hosts: vec!["localhost".into(), "[::1]:5000".into()],
            ..Config::default()
        };
        assert_eq!(
            c.host_allowlist(),
            Some(vec![
                "localhost".into(),
                "::1".into(),
                "nuget.example.com".into()
            ])
        );
        let any = Config {
            base_url: Some("https://nuget.example.com".into()),
            allowed_hosts: vec!["*".into()],
            ..Config::default()
        };
        assert_eq!(any.host_allowlist(), None);
    }

    #[test]
    fn hosts_normalise_for_comparison() {
        assert_eq!(normalize_host("Feed.Example.COM:443"), "feed.example.com");
        assert_eq!(normalize_host("feed.example.com."), "feed.example.com");
        assert_eq!(normalize_host("[2001:DB8::1]:8443"), "2001:db8::1");
        assert_eq!(normalize_host("2001:db8::1"), "2001:db8::1");
        assert_eq!(normalize_host("10.0.0.1:5000"), "10.0.0.1");
    }

    #[test]
    fn secrets_never_reach_debug_output() {
        let mut c = Config {
            api_key: Some("push-secret".into()),
            api_keys: vec!["other-push-secret".into()],
            admin_api_key: Some("admin-secret".into()),
            ..Config::default()
        };
        c.feeds = vec![FeedConfig {
            name: "stable".into(),
            read_api_key: Some("read-secret".into()),
            mirror: MirrorConfig {
                upstream: "https://u:up-secret@feed.example/_auth/path-secret/index.json".into(),
                auth: MirrorAuthConfig {
                    password: Some("basic-secret".into()),
                    token: Some("token-secret".into()),
                    headers: [("X-Api-Key".to_string(), "header-secret".to_string())].into(),
                    ..MirrorAuthConfig::default()
                },
                ..MirrorConfig::default()
            },
            ..FeedConfig::default()
        }];
        let feeds = c.resolved_feeds().unwrap();
        let dumps = [
            format!("{c:?}"),
            format!("{feeds:?}"),
            format!("{:?}", c.feeds[0].mirror.auth),
        ];
        for dump in &dumps {
            assert!(!dump.contains("secret"), "{dump}");
        }
        assert!(dumps[1].contains("https://feed.example"), "{}", dumps[1]);
    }

    #[test]
    fn a_url_origin_carries_no_credentials() {
        assert_eq!(
            url_origin("https://ci:s3cret@Feed.example.com:8443/_auth/TOKEN/v3/index.json?k=v"),
            "https://Feed.example.com:8443"
        );
        assert_eq!(
            url_origin("HTTPS://api.nuget.org/v3/index.json"),
            "https://api.nuget.org"
        );
        assert_eq!(url_origin("https://host?token=x"), "https://host");
        assert_eq!(url_origin("not a url"), "<not a URL>");
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
