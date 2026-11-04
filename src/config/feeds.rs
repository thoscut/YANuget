//! Hosted feeds: their configuration, mirror and license policy, and how
//! they resolve against the server-wide settings.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

use super::env::combine_keys;
use super::sections::{OverwriteMode, RetentionConfig};
use super::validate::{checked_retention, reserved_prefixes, validate_feed_name};
use super::{redact_opt, url_origin, Config};

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
/// `username`/`password` pair for HTTP Basic *or* `token` for a Bearer token,
/// and/or `headers` for arbitrary custom headers (e.g. a private-feed API key).
/// Headers are sent alongside either. They go only to the upstream's own
/// scheme, host and port, never to hosts its service index names.
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

    /// Reject combinations that cannot all be sent. Basic and Bearer both
    /// need `Authorization`, and one of them used to be dropped without a
    /// word — so a token the operator believed was in use never was.
    pub fn validate(&self) -> Result<()> {
        if self.username.is_some() && self.token.is_some() {
            return Err(Error::BadRequest(
                "mirror auth sets both username (HTTP Basic) and token (Bearer); \
                 both need the Authorization header, so set only one"
                    .into(),
            ));
        }
        if self.password.is_some() && self.username.is_none() {
            return Err(Error::BadRequest(
                "mirror auth sets a password without a username".into(),
            ));
        }
        Ok(())
    }
}

impl std::fmt::Debug for MirrorAuthConfig {
    /// Secrets are shown only as present; custom headers by name only, since
    /// their values are keys.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted = |v: &Option<String>| v.as_ref().map(|_| "<redacted>");
        f.debug_struct("MirrorAuthConfig")
            .field("username", &self.username)
            .field("password", &redacted(&self.password))
            .field("token", &redacted(&self.token))
            .field("headers", &self.headers.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Per-feed upstream mirroring (read-through caching of a public NuGet feed).
#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MirrorConfig {
    /// Master switch. Off by default.
    pub enabled: bool,
    /// Upstream V3 service index to mirror from.
    pub upstream: String,
    /// Timeout (seconds) for connecting to the upstream, for any silence while
    /// it answers, and for a whole metadata request.
    pub timeout_secs: u64,
    /// Deadline (seconds) for one whole `.nupkg` download; `0` removes it. A
    /// stalled transfer is cut off by `timeout_secs` either way; this bounds
    /// one that trickles.
    pub download_timeout_secs: u64,
    /// How long (seconds) a package's upstream version list is trusted before
    /// a read lists it again, so new upstream releases appear. It is also how
    /// long an id the upstream does not have is not asked for again.
    pub refresh_secs: u64,
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
    /// Outbound proxy for upstream requests (`http://proxy:3128`). Unset, the
    /// mirror connects directly and ignores `HTTP(S)_PROXY` in the environment.
    pub proxy: Option<String>,
    /// PEM file of extra CA certificates to trust for the upstream, on top of
    /// the system store and the bundled Mozilla roots — for an upstream behind
    /// an internal CA or a TLS-inspecting proxy.
    pub ca_cert_path: Option<PathBuf>,
    /// How often (seconds) the server asks the upstream for its service
    /// index on its own, so an unreachable upstream shows in the log and in
    /// `/health/status` before a client needs it. `0` turns the probe off.
    pub probe_interval_secs: u64,
}

impl MirrorConfig {
    /// Reject a mirror configuration that cannot work as written.
    pub fn validate(&self) -> Result<()> {
        self.auth.validate()
    }
}

impl std::fmt::Debug for MirrorConfig {
    /// The upstream and proxy URLs can carry credentials of their own
    /// (`https://user:token@host/…`), so they are shown without userinfo or
    /// query.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redact = crate::mirror::redact_url;
        f.debug_struct("MirrorConfig")
            .field("enabled", &self.enabled)
            .field("upstream", &redact(&self.upstream))
            .field("timeout_secs", &self.timeout_secs)
            .field("download_timeout_secs", &self.download_timeout_secs)
            .field("refresh_secs", &self.refresh_secs)
            .field("auth", &self.auth)
            .field("allow_private_upstream", &self.allow_private_upstream)
            .field("max_package_size_bytes", &self.max_package_size_bytes)
            .field("max_versions_per_package", &self.max_versions_per_package)
            .field("proxy", &self.proxy.as_deref().map(redact))
            .field("ca_cert_path", &self.ca_cert_path)
            .field("probe_interval_secs", &self.probe_interval_secs)
            .finish_non_exhaustive()
    }
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            upstream: "https://api.nuget.org/v3/index.json".to_string(),
            timeout_secs: 30,
            download_timeout_secs: 3600,
            refresh_secs: 600,
            auth: MirrorAuthConfig::default(),
            allow_private_upstream: false,
            max_package_size_bytes: None,
            max_versions_per_package: Some(50),
            proxy: None,
            ca_cert_path: None,
            probe_interval_secs: 300,
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
    /// Credential that downloads *pending* versions of this feed, and their
    /// attached files, as well as approved ones, and does nothing else: no
    /// push, no approve, no search. For testing a version before approving
    /// it. Sent like the read key. Unset, pending versions are withheld from
    /// every client.
    pub preview_api_key: Option<String>,
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
    /// Attached files in this feed; unset fields fall back to `[files]`.
    pub files: FeedFilesConfig,
}

/// `[feeds.files]`: what one feed changes about attached files.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FeedFilesConfig {
    /// The file extensions this feed accepts, replacing
    /// `files.allowed_extensions` for it (not adding to it). Lets one feed,
    /// say an internal Chocolatey feed, take `exe` and `msi` while the others
    /// keep the narrower default.
    pub allowed_extensions: Option<Vec<String>>,
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
    pub preview_api_key: Option<String>,
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
    /// The file extensions attached files may have in this feed.
    pub allowed_extensions: Vec<String>,
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
        id_has_prefix(id, &self.prefix)
    }
}

/// Whether `id` falls under the package-id `prefix`, ignoring ASCII case. A
/// prefix ending in `.` also covers the bare id before it (`Contoso.` covers
/// `Contoso` as well as `Contoso.Utils`).
pub fn id_has_prefix(id: &str, prefix: &str) -> bool {
    let id = id.as_bytes();
    let prefix = prefix.as_bytes();
    let starts = id.len() >= prefix.len() && id[..prefix.len()].eq_ignore_ascii_case(prefix);
    starts
        || prefix
            .strip_suffix(b".")
            .is_some_and(|bare| id.eq_ignore_ascii_case(bare))
}

impl Config {
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
                preview_api_key: None,
                admin_api_key: self.admin_api_key.clone(),
                allow_overwrite: self.allow_overwrite,
                hard_delete_enabled: self.hard_delete_enabled,
                requires_approval: false,
                promotes_to: None,
                mirror: MirrorConfig::default(),
                license_policy: LicensePolicyConfig::default(),
                retention: checked_retention(&self.retention, DEFAULT_FEED)?,
                // A single feed has no other feed to reserve anything from.
                reserved_id_prefixes: Vec::new(),
                reserved_elsewhere: Vec::new(),
                allowed_extensions: self.files.allowed_extensions.clone(),
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
            f.mirror
                .validate()
                .map_err(|e| Error::BadRequest(format!("feed {:?}: {e}", f.name)))?;
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
                preview_api_key: f.preview_api_key.clone(),
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
                retention: checked_retention(
                    f.retention.as_ref().unwrap_or(&self.retention),
                    &f.name,
                )?,
                reserved_id_prefixes: reserved_prefixes(f)?,
                reserved_elsewhere: Vec::new(),
                allowed_extensions: f
                    .files
                    .allowed_extensions
                    .clone()
                    .unwrap_or_else(|| self.files.allowed_extensions.clone()),
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

impl std::fmt::Debug for ResolvedFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedFeed")
            .field("name", &self.name)
            .field("prefix", &self.prefix)
            .field("api_keys", &self.api_keys.len())
            .field("read_api_key", &redact_opt(&self.read_api_key))
            .field("preview_api_key", &redact_opt(&self.preview_api_key))
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
            .field("allowed_extensions", &self.allowed_extensions)
            .finish_non_exhaustive()
    }
}
