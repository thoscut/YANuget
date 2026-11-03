//! The server-wide sections: overwrite policy, retention, rate limiting,
//! monitoring and attached files.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;

use super::env::env_error;
use super::extension_allowed;

/// Overwrite policy for re-pushing an existing package id/version.
///
/// Deserializes from either a bool (`true`/`false`, the historical form) or a
/// string (`"true"`, `"false"`, `"prerelease-only"`, `"pending-only"`), so
/// existing configs keep working while the newer modes are available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OverwriteMode {
    /// Never overwrite — versions are immutable (the default).
    #[default]
    Disabled,
    /// Overwrite any existing version.
    Enabled,
    /// Overwrite pre-release versions only; stable releases stay immutable.
    PrereleaseOnly,
    /// Replace a version only while it is pending (withheld, never served) in
    /// every feed that holds it, together with its attached files. A version
    /// approved anywhere stays immutable. For intake feeds a tool pushes into
    /// and re-runs against.
    PendingOnly,
}

impl OverwriteMode {
    /// Whether a push may overwrite a version with the given pre-release
    /// status, whatever state the version is in. `PendingOnly` depends on
    /// that state, so it is decided where the state is known, not here.
    pub fn allows(self, is_prerelease: bool) -> bool {
        match self {
            OverwriteMode::Disabled | OverwriteMode::PendingOnly => false,
            OverwriteMode::Enabled => true,
            OverwriteMode::PrereleaseOnly => is_prerelease,
        }
    }

    /// Whether a version that has been served can never change under this
    /// mode, so downloads may be cached as `immutable`.
    pub fn served_versions_are_immutable(self) -> bool {
        matches!(self, OverwriteMode::Disabled | OverwriteMode::PendingOnly)
    }

    /// A human-readable label for the settings page.
    pub fn label(self) -> &'static str {
        match self {
            OverwriteMode::Disabled => "No",
            OverwriteMode::Enabled => "Yes",
            OverwriteMode::PrereleaseOnly => "Pre-release only",
            OverwriteMode::PendingOnly => "Pending versions only",
        }
    }

    /// Parse an environment-variable value: the TOML spellings plus the
    /// boolean ones. Anything else is an error, never a silent "off".
    pub(super) fn parse_env(name: &str, v: &str) -> Result<OverwriteMode> {
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
            "pending-only" | "pending" | "pendingonly" => Ok(OverwriteMode::PendingOnly),
            _ => Err(env_error(
                name,
                &format!("must be true, false, prerelease-only or pending-only, not {v:?}"),
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
            OverwriteMode::PendingOnly => "pending-only",
        })
    }
}

impl<'de> Deserialize<'de> for OverwriteMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct ModeVisitor;
        impl serde::de::Visitor<'_> for ModeVisitor {
            type Value = OverwriteMode;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(
                    r#"a bool or one of "true", "false", "prerelease-only", "pending-only""#,
                )
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
                    "pending-only" | "pending" | "pendingonly" => Ok(OverwriteMode::PendingOnly),
                    other => Err(E::custom(format!("invalid overwrite mode: {other}"))),
                }
            }
        }
        d.deserialize_any(ModeVisitor)
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
    /// Delete versions added to the feed more than this many days ago.
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

/// The monitoring endpoints: `GET /metrics` in the Prometheus text format and
/// `GET /health/status` as JSON, with per-feed package counts and the state of
/// each mirror's upstream.
///
/// Off by default, because both name every feed (read-gated ones included)
/// and each mirror's upstream host, which an anonymous visitor otherwise never
/// learns. `/health`, `/health/live` and `/health/ready` are always served.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MonitoringConfig {
    /// Serve `/metrics` and `/health/status`.
    pub enabled: bool,
    /// When set, both endpoints require `Authorization: Bearer <token>`.
    /// Unset, anyone who can reach the server can read them.
    pub token: Option<String>,
}

impl MonitoringConfig {
    /// The token both endpoints require, trimmed; `None` when unset or blank
    /// (which [`Config::validate`](super::Config::validate) refuses).
    pub fn token(&self) -> Option<&str> {
        self.token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }
}

impl std::fmt::Debug for MonitoringConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MonitoringConfig")
            .field("enabled", &self.enabled)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
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
    /// How many unfinished resumable uploads one feed may hold at once; 0
    /// is no limit. Each holds a partial file and the space it declared.
    pub max_open_uploads: u64,
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
            max_open_uploads: 32,
            inbox_dir: None,
            inbox_scan_secs: 30,
        }
    }
}

impl FilesConfig {
    /// Whether `ext` (without the dot) is one of the accepted extensions.
    pub fn allows_extension(&self, ext: &str) -> bool {
        extension_allowed(&self.allowed_extensions, ext)
    }
}
