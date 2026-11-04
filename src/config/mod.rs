//! Server configuration.
//!
//! Configuration is layered: built-in defaults, then an optional TOML file,
//! then environment variables (`YANUGET_*`). Environment variables win so the
//! server is easy to configure in containers.

mod env;
mod feeds;
mod sections;
mod validate;

pub use env::feed_env_name;
pub use feeds::{
    id_has_prefix, FeedConfig, FeedFilesConfig, LicensePolicyConfig, MirrorAuthConfig,
    MirrorConfig, PolicyAction, ReservedPrefix, ResolvedFeed, DEFAULT_FEED,
};
pub use sections::{
    FilesConfig, MonitoringConfig, OverwriteMode, RateLimitConfig, RetentionConfig,
};

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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
    /// `/metrics` (Prometheus) and `/health/status` (JSON). Off by default.
    pub monitoring: MonitoringConfig,
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

pub use yanuget_pkg::files::extension_allowed;

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
            monitoring: MonitoringConfig::default(),
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
            .field("monitoring", &self.monitoring)
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

#[cfg(test)]
mod tests {
    use super::validate::validate_feed_name;
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
        assert_eq!(
            mode(r#"allow_overwrite = "pending-only""#),
            OverwriteMode::PendingOnly
        );

        // The decision helper. Pending-only depends on the version's state,
        // which `allows` does not know, so it says no.
        assert!(!OverwriteMode::PendingOnly.allows(true));
        assert!(OverwriteMode::PendingOnly.served_versions_are_immutable());
        assert!(!OverwriteMode::PrereleaseOnly.served_versions_are_immutable());
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
        assert_eq!(
            OverwriteMode::parse_env("X", "pending_only").unwrap(),
            OverwriteMode::PendingOnly
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

    #[test]
    fn retention_exclude_id_prefixes_are_checked_per_feed() {
        let toml = r#"
            [retention]
            keep_latest_stable = 3
            exclude_id_prefixes = ["Global."]

            [[feeds]]
            name = "stage"

            [[feeds]]
            name = "production"
            [feeds.retention]
            enabled = true
            keep_latest_stable = 2
            exclude_id_prefixes = [" cfturbo", "pumplinx"]
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        let feeds = c.resolved_feeds().unwrap();
        // A feed without its own [feeds.retention] takes the global one.
        assert_eq!(feeds[0].retention.exclude_id_prefixes, ["Global."]);
        assert_eq!(
            feeds[1].retention.exclude_id_prefixes,
            ["cfturbo", "pumplinx"]
        );

        for bad in ["", ".cfturbo", "cft*", "a b"] {
            let mut c = c.clone();
            c.feeds[1].retention.as_mut().unwrap().exclude_id_prefixes = vec![bad.to_string()];
            let err = c.resolved_feeds().expect_err(bad).to_string();
            assert!(
                err.contains("retention.exclude_id_prefixes"),
                "{bad:?}: {err}"
            );
        }

        let env = with_env(&[(
            "YANUGET_RETENTION_EXCLUDE_ID_PREFIXES",
            "Contoso., Fabrikam ,",
        )])
        .unwrap();
        assert_eq!(env.retention.exclude_id_prefixes, ["Contoso.", "Fabrikam"]);
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
                    username: Some("ci".into()),
                    password: Some("basic-secret".into()),
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

    #[test]
    fn a_mirror_with_basic_and_bearer_credentials_is_rejected() {
        // Both need `Authorization`; one used to be dropped without a word.
        let config: Config = toml::from_str(
            r#"
            [[feeds]]
            name = "up"
              [feeds.mirror]
              enabled = true
                [feeds.mirror.auth]
                username = "ci"
                password = "pw"
                token = "tok"
            "#,
        )
        .unwrap();
        let err = config.resolved_feeds().unwrap_err().to_string();
        assert!(err.contains("\"up\"") && err.contains("token"), "{err}");
    }

    /// Apply `vars` as the environment on top of `config`.
    fn env_over(mut c: Config, vars: &[(&str, &str)]) -> Result<Config> {
        let map: std::collections::HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        c.apply_env_from(|name| map.get(name).cloned().ok_or(std::env::VarError::NotPresent))?;
        Ok(c)
    }

    fn two_feeds() -> Config {
        Config {
            feeds: vec![
                FeedConfig {
                    name: "stage".into(),
                    api_key: Some("from-toml".into()),
                    ..Default::default()
                },
                FeedConfig {
                    name: "pre-prod".into(),
                    ..Default::default()
                },
            ],
            ..Config::default()
        }
    }

    #[test]
    fn feed_keys_come_from_the_environment_or_a_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let admin = dir.path().join("admin");
        std::fs::write(&admin, "admin-from-file\n").unwrap();
        let keys = dir.path().join("keys");
        std::fs::write(&keys, "one\ntwo\n\n").unwrap();
        let c = env_over(
            two_feeds(),
            &[
                ("YANUGET_FEED_STAGE_API_KEY", "from-env"),
                (
                    "YANUGET_FEED_PRE_PROD_ADMIN_API_KEY_FILE",
                    admin.to_str().unwrap(),
                ),
                (
                    "YANUGET_FEED_PRE_PROD_API_KEYS_FILE",
                    keys.to_str().unwrap(),
                ),
                ("YANUGET_FEED_PRE_PROD_READ_API_KEY", "reader"),
                ("YANUGET_FEED_PRE_PROD_PREVIEW_API_KEY", "previewer"),
                ("YANUGET_FEED_PRE_PROD_MIRROR_TOKEN", "upstream-token"),
            ],
        )
        .unwrap();
        assert_eq!(c.feeds[0].api_key.as_deref(), Some("from-env"));
        assert_eq!(c.feeds[1].admin_api_key.as_deref(), Some("admin-from-file"));
        assert_eq!(c.feeds[1].api_keys, vec!["one", "two"]);
        assert_eq!(c.feeds[1].read_api_key.as_deref(), Some("reader"));
        assert_eq!(c.feeds[1].preview_api_key.as_deref(), Some("previewer"));
        assert_eq!(
            c.feeds[1].mirror.auth.token.as_deref(),
            Some("upstream-token")
        );
    }

    #[test]
    fn global_keys_come_from_a_key_file_too() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("admin");
        std::fs::write(&key, "global-admin\r\n").unwrap();
        let c = env_over(
            Config::default(),
            &[("YANUGET_ADMIN_API_KEY_FILE", key.to_str().unwrap())],
        )
        .unwrap();
        assert_eq!(c.admin_api_key.as_deref(), Some("global-admin"));
    }

    #[test]
    fn a_key_file_that_is_empty_missing_or_doubled_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty");
        std::fs::write(&empty, "\n").unwrap();
        let missing = dir.path().join("missing");
        for vars in [
            vec![("YANUGET_API_KEY_FILE", empty.to_str().unwrap())],
            vec![("YANUGET_FEED_STAGE_API_KEY_FILE", missing.to_str().unwrap())],
            vec![
                ("YANUGET_FEED_STAGE_API_KEY", "a"),
                ("YANUGET_FEED_STAGE_API_KEY_FILE", empty.to_str().unwrap()),
            ],
        ] {
            assert!(env_over(two_feeds(), &vars).is_err(), "{vars:?}");
        }
    }

    #[test]
    fn variables_nothing_reads_are_reported() {
        let c = two_feeds();
        let unknown = c.unknown_env_names([
            "YANUGET_ADMIN_API_KEY",
            "YANUGET_ADMIN_APIKEY",
            "YANUGET_MONITORING_TOKEN_FILE",
            "YANUGET_FEED_STAGE_API_KEY",
            "YANUGET_FEED_STAGING_API_KEY",
            "YANUGET_CONFIG",
            "YANUGET_ADMIN_KEY",
            "PATH",
        ]);
        assert_eq!(
            unknown,
            ["YANUGET_ADMIN_APIKEY", "YANUGET_FEED_STAGING_API_KEY"]
        );
    }

    #[test]
    fn a_blank_monitoring_token_in_the_file_is_an_error() {
        // As for `YANUGET_MONITORING_TOKEN`: a template with an empty
        // variable must not open /metrics without a word.
        for blank in ["", "   "] {
            let toml = format!("[monitoring]\nenabled = true\ntoken = {blank:?}\n");
            let c: Config = toml::from_str(&toml).unwrap();
            let err = c.validate().unwrap_err().to_string();
            assert!(err.contains("monitoring.token"), "{blank:?}: {err}");
        }
        let c: Config = toml::from_str("[monitoring]\ntoken = \" tok \"\n").unwrap();
        c.validate().unwrap();
        assert_eq!(c.monitoring.token(), Some("tok"));
    }

    #[test]
    fn a_blank_key_variable_is_an_error_not_a_removal() {
        // Compose writes `${UNSET}` as an empty string. Read as "no key", that
        // would make a read-gated feed public, or drop its push key.
        let mut gated = two_feeds();
        gated.feeds[1].read_api_key = Some("reader".into());
        for name in [
            "YANUGET_API_KEY",
            "YANUGET_API_KEYS",
            "YANUGET_ADMIN_API_KEY",
            "YANUGET_MONITORING_TOKEN",
            "YANUGET_FEED_STAGE_API_KEY",
            "YANUGET_FEED_STAGE_API_KEYS",
            "YANUGET_FEED_PRE_PROD_READ_API_KEY",
            "YANUGET_FEED_PRE_PROD_PREVIEW_API_KEY",
            "YANUGET_FEED_PRE_PROD_ADMIN_API_KEY",
            "YANUGET_FEED_PRE_PROD_MIRROR_PASSWORD",
            "YANUGET_FEED_PRE_PROD_MIRROR_TOKEN",
        ] {
            for blank in ["", "  "] {
                let err = env_over(gated.clone(), &[(name, blank)])
                    .unwrap_err()
                    .to_string();
                assert!(err.contains(name), "{name}={blank:?}: {err}");
            }
        }
        // A list of separators only holds no key either.
        assert!(env_over(gated.clone(), &[("YANUGET_FEED_STAGE_API_KEYS", " , ,")]).is_err());
        // Unset, the configuration file's keys stand.
        let c = env_over(gated, &[]).unwrap();
        assert_eq!(c.feeds[0].api_key.as_deref(), Some("from-toml"));
        assert_eq!(c.feeds[1].read_api_key.as_deref(), Some("reader"));
    }

    #[test]
    fn a_feed_variable_two_feeds_could_claim_is_refused() {
        let mut c = two_feeds();
        c.feeds.push(FeedConfig {
            name: "pre.prod".into(),
            ..Default::default()
        });
        // Unset, the shared spelling is harmless.
        assert!(env_over(c.clone(), &[]).is_ok());
        let err = env_over(c, &[("YANUGET_FEED_PRE_PROD_API_KEY", "k")])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("pre-prod") && err.contains("pre.prod"),
            "{err}"
        );
    }
}
