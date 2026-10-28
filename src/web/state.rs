//! Per-feed application state: the resolved [`FeedContext`] a feed is served
//! with, the cross-feed [`FeedMeta`] registry, and the cloneable [`AppState`]
//! every handler receives.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::http::{header, HeaderMap};

use super::files;
use super::helpers::{detached, forwarded};
use crate::auth::{AdminAuth, ApiKeyAuth, ReadAuth};
use crate::config::{Config, LicensePolicyConfig, OverwriteMode, ResolvedFeed, RetentionConfig};
use crate::database::PackageDatabase;
use crate::error::{Error, Result};
use crate::mirror::{self, MirrorClient, MirrorOptions};
use crate::nuget::UrlBuilder;
use crate::retention;
use crate::storage::{PackageStorage, TempPath};
use crate::version::NuGetVersion;

/// The resolved, ready-to-serve context for a single feed: its identity, its
/// own put/get/delete authenticators, and its mirror/policy/retention settings.
pub struct FeedContext {
    /// Database key / slug.
    pub name: String,
    /// URL path prefix: `""` for the root feed, else `/{name}`.
    pub prefix: String,
    /// Push (put) authenticator.
    pub auth: ApiKeyAuth,
    /// Download/restore (get) authenticator.
    pub read_auth: ReadAuth,
    /// The preview key: downloads pending versions too, and nothing else.
    pub preview_auth: ReadAuth,
    /// Moderation/promotion (delete) authenticator.
    pub admin: AdminAuth,
    pub allow_overwrite: OverwriteMode,
    pub hard_delete_enabled: bool,
    /// Incoming versions are pending (withheld) until an admin approves them.
    pub requires_approval: bool,
    /// The feed an admin can promote a version into (the next release ring).
    pub promotes_to: Option<String>,
    /// Upstream mirror client, when mirroring is enabled for this feed.
    pub mirror: Option<MirrorClient>,
    pub license_policy: LicensePolicyConfig,
    pub retention: RetentionConfig,
    /// Id prefixes other feeds reserved, which this one refuses.
    pub reserved_elsewhere: Vec<crate::config::ReservedPrefix>,
    /// The extensions attached files may have here (`[feeds.files]`, else
    /// `[files]`).
    pub allowed_extensions: Vec<String>,
    /// The feed's cleanup lock and last report, shared with the background
    /// sweep.
    pub cleanup: Arc<retention::RetentionState>,
    /// What `/metrics` counts for this feed since startup.
    pub counters: FeedCounters,
}

/// Events `/metrics` counts per feed, since the process started.
#[derive(Debug, Default)]
pub struct FeedCounters {
    /// Packages pushed and indexed.
    pub pushes: AtomicU64,
    /// Versions promoted from this feed into its `promotes_to` feed.
    pub promotions: AtomicU64,
    /// Package downloads served, counted like the gallery's download counts.
    pub downloads: AtomicU64,
}

impl FeedCounters {
    pub(super) fn count(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

impl FeedContext {
    /// How content addressed by id and version — a payload, its manifest and
    /// icon, its symbols — may be cached.
    ///
    /// `private` when the feed gates reads, so a shared cache or CDN in front
    /// of the server never hands it to someone without the key. `immutable`
    /// only while overwriting is off: with it on, the same URL can serve new
    /// bytes, and a year-long `immutable` would keep the old ones in every
    /// client and proxy that saw them.
    pub(crate) fn content_cache(&self) -> files::CachePolicy {
        files::CachePolicy {
            private: self.read_auth.is_enabled(),
            immutable: self.allow_overwrite.served_versions_are_immutable(),
        }
    }

    /// Build a feed's serving context. Mirrored downloads inherit the
    /// server-wide `max_package_size_bytes` unless the feed's mirror set a
    /// tighter one of its own, and are held to `min_free_disk_bytes`.
    fn from_resolved(feed: &ResolvedFeed, config: &Config) -> Result<Self> {
        // A mirror that cannot be built stops startup rather than quietly
        // serving the feed without it.
        let mut mirror = MirrorClient::try_from_config(&feed.mirror)?;
        if let Some(client) = mirror.as_mut() {
            client.set_default_size_limit(config.max_package_size_bytes);
            client.set_min_free_disk_bytes(config.min_free_disk_bytes);
            client.set_feed_name(&feed.name);
        }
        Ok(Self {
            name: feed.name.clone(),
            prefix: feed.prefix.clone(),
            auth: ApiKeyAuth::new(feed.api_keys.clone()),
            read_auth: ReadAuth::new(feed.read_api_key.clone()),
            preview_auth: ReadAuth::new(feed.preview_api_key.clone()),
            admin: AdminAuth::new(feed.admin_api_key.clone()),
            allow_overwrite: feed.allow_overwrite,
            hard_delete_enabled: feed.hard_delete_enabled,
            requires_approval: feed.requires_approval,
            promotes_to: feed.promotes_to.clone(),
            mirror,
            license_policy: feed.license_policy.clone(),
            retention: feed.retention.clone(),
            reserved_elsewhere: feed.reserved_elsewhere.clone(),
            allowed_extensions: feed.allowed_extensions.clone(),
            cleanup: Arc::default(),
            counters: FeedCounters::default(),
        })
    }
}

/// How a download is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// A version clients may fetch, under the feed's usual cache policy.
    Public,
    /// Any version fetched with the preview key: kept out of shared caches,
    /// never `immutable` (a pending build can still be replaced under
    /// `allow_overwrite = "pending-only"`) and not counted as a download.
    Preview,
}

impl Access {
    /// A preview response depends on the credential sent, so caches must
    /// key on it.
    pub(crate) fn mark(self, response: &mut axum::response::Response) {
        if self == Access::Preview {
            response.headers_mut().append(
                axum::http::header::VARY,
                axum::http::HeaderValue::from_static("Authorization, X-NuGet-ApiKey"),
            );
        }
    }

    /// The cache policy for content served this way.
    pub(crate) fn cache(self, feed: &FeedContext) -> files::CachePolicy {
        match self {
            Access::Public => feed.content_cache(),
            Access::Preview => files::CachePolicy {
                private: true,
                immutable: false,
            },
        }
    }
}

/// Lightweight, cross-feed metadata so a handler can resolve a promotion, copy
/// or move target.
#[derive(Debug, Clone)]
pub struct FeedMeta {
    pub name: String,
    pub prefix: String,
    pub requires_approval: bool,
    /// The target feed's own license policy, so a promotion into it is held to
    /// the same rule a direct push would be.
    pub license_policy: LicensePolicyConfig,
    /// The target feed's own admin key. Copying or moving a version into a
    /// feed takes that feed's admin credentials, not just this one's.
    pub admin: AdminAuth,
}

impl FeedMeta {
    /// The cross-feed view of one resolved feed.
    pub fn from_resolved(feed: &ResolvedFeed) -> Self {
        Self {
            name: feed.name.clone(),
            prefix: feed.prefix.clone(),
            requires_approval: feed.requires_approval,
            license_policy: feed.license_policy.clone(),
            admin: AdminAuth::new(feed.admin_api_key.clone()),
        }
    }
}

/// Shared application state for one feed, cheaply cloneable (everything behind
/// `Arc`). Storage, database and the feed registry are shared by all feeds.
#[derive(Clone)]
pub struct AppState {
    pub storage: Arc<dyn PackageStorage>,
    pub db: Arc<dyn PackageDatabase>,
    pub config: Arc<Config>,
    pub feed: Arc<FeedContext>,
    pub(super) feeds: Arc<Vec<FeedMeta>>,
    pub(super) temp_dir: PathBuf,
}

impl AppState {
    /// Construct state for the implicit single feed served at the root. Used by
    /// tests and simple single-feed deployments.
    pub async fn new(
        storage: Arc<dyn PackageStorage>,
        db: Arc<dyn PackageDatabase>,
        config: Arc<Config>,
    ) -> Result<Self> {
        let feeds = config.resolved_feeds()?;
        // `resolved_feeds` with no `[[feeds]]` yields exactly the root feed.
        let resolved = feeds
            .into_iter()
            .next()
            .expect("at least one resolved feed");
        Self::for_feed(storage, db, config, &resolved, Arc::new(Vec::new())).await
    }

    /// Construct state for one resolved feed.
    pub async fn for_feed(
        storage: Arc<dyn PackageStorage>,
        db: Arc<dyn PackageDatabase>,
        config: Arc<Config>,
        resolved: &ResolvedFeed,
        feeds: Arc<Vec<FeedMeta>>,
    ) -> Result<Self> {
        // Keep temp uploads on the same filesystem as storage so the final move
        // is an atomic rename rather than a multi-gigabyte copy.
        let temp_dir = config.storage_path().join(".uploads");
        tokio::fs::create_dir_all(&temp_dir).await?;
        sweep_stale_uploads(&temp_dir).await;
        let feed = Arc::new(FeedContext::from_resolved(resolved, &config)?);
        Ok(Self {
            storage,
            db,
            config,
            feed,
            feeds,
            temp_dir,
        })
    }

    /// The feed name used as the database scope for every package query.
    pub(super) fn feed(&self) -> &str {
        &self.feed.name
    }

    /// Resolve the externally visible base URL for this request, including the
    /// feed's path prefix so generated resource URLs stay within the feed.
    pub(super) fn url_builder(&self, headers: &HeaderMap) -> UrlBuilder {
        let root = if let Some(base) = &self.config.base_url {
            base.clone()
        } else {
            // Only a scheme a client can actually fetch from. Anything else
            // would be pasted into every absolute URL handed out.
            let scheme = forwarded(headers, "x-forwarded-proto")
                .map(|s| s.to_ascii_lowercase())
                .filter(|s| s == "http" || s == "https")
                .unwrap_or_else(|| self.config.scheme().to_string());
            let host = forwarded(headers, "x-forwarded-host")
                .or_else(|| {
                    headers
                        .get(header::HOST)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "localhost".into());
            format!("{scheme}://{host}")
        };
        // Embedded icons are served by the gallery's icon route, so they can
        // only be advertised when the gallery is mounted.
        UrlBuilder::with_prefix(root, &self.feed.prefix).with_icons(self.config.enable_web_ui)
    }

    /// Refuse an upload of `incoming` bytes (when the client declared a size)
    /// that would leave the storage volume with less than
    /// `min_free_disk_bytes` free.
    ///
    /// A full disk does not fail cleanly: the database, the temp file and every
    /// other writer on the volume run out together, mid-write. Saying no up
    /// front costs one `statvfs`. With no declared size (a chunked body) only
    /// the reserve itself is checked; the size limit and the idle timeout bound
    /// the rest. When free space cannot be measured the upload is let through,
    /// since a guard that fails closed would take the feed down with it.
    pub(super) fn ensure_disk_space(&self, incoming: Option<u64>) -> Result<()> {
        let reserve = self.config.min_free_disk_bytes;
        if reserve == 0 {
            return Ok(());
        }
        let available = match fs4::available_space(&self.temp_dir) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(error = %e, "could not measure free disk space");
                return Ok(());
            }
        };
        let needed = incoming.unwrap_or(0).saturating_add(reserve);
        if available < needed {
            return Err(Error::InsufficientStorage(format!(
                "{available} bytes free on the storage volume, {needed} needed \
                 (the upload plus the configured reserve)"
            )));
        }
        Ok(())
    }

    /// `writer`, made to fail part-way once the storage volume drops below
    /// `min_free_disk_bytes` (see [`streaming::DiskReserve`]): the check
    /// [`Self::ensure_disk_space`] makes up front cannot see a body that
    /// declared no size.
    ///
    /// [`streaming::DiskReserve`]: crate::streaming::DiskReserve
    pub(super) fn reserve_guard<'a, W>(
        &'a self,
        writer: &'a mut W,
    ) -> crate::streaming::DiskReserve<'a, W> {
        crate::streaming::DiskReserve::new(writer, &self.temp_dir, self.config.min_free_disk_bytes)
    }

    /// Create a fresh temp file for an incoming upload, removed again when the
    /// returned [`TempPath`] is dropped unless it has been moved into the store.
    pub(super) async fn create_temp(&self) -> Result<(TempPath, tokio::fs::File)> {
        let path = self.temp_dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let file = tokio::fs::File::create(&path).await?;
        Ok((TempPath::new(path), file))
    }

    /// Whether a download of `id`/`version` may be served, and how. The
    /// feed's preview key may fetch a pending version as well as an approved
    /// one (never a disabled one), and stands in for the read key on the
    /// download endpoints only. Anyone else needs read access and a servable
    /// version. A miss is `None`; the caller decides whether to mirror.
    pub(super) async fn download_access(
        &self,
        headers: &HeaderMap,
        id: &str,
        version: &crate::version::NuGetVersion,
    ) -> Result<Option<Access>> {
        if self.feed.preview_auth.presented(headers) {
            // Preview treatment whether or not the version is still pending:
            // what the key fetches is a test, not a download, and its answer
            // must not be cached as the version's public, immutable one.
            let held = self.db.get_membership(self.feed(), id, version).await?;
            return Ok(held.filter(|m| m.enabled).map(|_| Access::Preview));
        }
        self.require_read(headers)?;
        Ok(self
            .db
            .is_servable(self.feed(), id, version)
            .await?
            .then_some(Access::Public))
    }

    /// What a download that found nothing servable answers.
    ///
    /// On a feed with open reads and a preview key, a pending version is a
    /// 404 to anyone without the key and a 200 with it, and a 404 does not
    /// count toward `max_failed_auth`: the key could be guessed at the
    /// request limiter's pace instead of the failed-authentication one. So a
    /// request that carries a credential which is neither the feed's read
    /// key nor its preview key is answered `401` (`SEC-28`).
    pub(super) fn download_miss(&self, headers: &HeaderMap) -> Error {
        let credential = headers.contains_key(header::AUTHORIZATION)
            || headers.contains_key(crate::auth::API_KEY_HEADER);
        if self.feed.preview_auth.is_enabled()
            && credential
            && !self.feed.preview_auth.presented(headers)
            && !self.feed.read_auth.presented(headers)
        {
            Error::Unauthorized
        } else {
            Error::PackageNotFound
        }
    }

    /// Reject the request unless valid read credentials are presented (a no-op
    /// when the feed allows open reads).
    pub(super) fn require_read(&self, headers: &HeaderMap) -> Result<()> {
        if self.feed.read_auth.check_headers(headers) {
            Ok(())
        } else {
            Err(Error::Unauthorized)
        }
    }

    fn mirror_options(&self) -> MirrorOptions {
        MirrorOptions {
            requires_approval: self.feed.requires_approval,
            license_policy: self.feed.license_policy.clone(),
            reserved_elsewhere: self.feed.reserved_elsewhere.clone(),
        }
    }

    /// Best-effort read-through mirror: when the feed has an upstream and a
    /// lookup missed, fetch the package's versions and index them. Errors are
    /// logged, never surfaced — a mirror outage degrades to a normal miss.
    pub(super) async fn mirror_if_needed(&self, id: &str) {
        if self.feed.mirror.is_none() {
            return;
        }
        // Not worth a download that indexing would refuse.
        if self.feed.reserved_elsewhere.iter().any(|r| r.covers(id)) {
            return;
        }
        let options = self.mirror_options();
        // Fetching and indexing is the same store-then-record sequence as a
        // push, so it too finishes when the client that asked goes away.
        let (state, owned_id) = (self.clone(), id.to_string());
        let fetched = detached(async move {
            let Some(client) = &state.feed.mirror else {
                return Ok(0);
            };
            mirror::ensure_package(
                client,
                state.storage.as_ref(),
                state.db.as_ref(),
                state.feed(),
                &state.temp_dir,
                &owned_id,
                &options,
            )
            .await
        })
        .await;
        if let Err(e) = fetched {
            tracing::warn!(feed = %self.feed(), id, error = %e, "mirror lookup failed");
        }
    }

    /// A package the feed already holds: re-list it upstream in the
    /// background once its list is older than `refresh_secs`, so new releases
    /// appear without the read that noticed waiting for them.
    pub(super) fn mirror_refresh(&self, id: &str) {
        let Some(client) = &self.feed.mirror else {
            return;
        };
        if !client.wants_refresh(self.feed(), id) {
            return;
        }
        let (state, id) = (self.clone(), id.to_string());
        tokio::spawn(async move { state.mirror_if_needed(&id).await });
    }

    /// A download of a version the feed does not have: fetch that version,
    /// whether or not it is among the newest the listing keeps.
    pub(super) async fn mirror_version(&self, id: &str, version: &NuGetVersion) {
        if self.feed.mirror.is_none() || self.feed.reserved_elsewhere.iter().any(|r| r.covers(id)) {
            return;
        }
        let options = self.mirror_options();
        // Detached for the same reason as `mirror_if_needed`.
        let (state, owned_id, owned_version) = (self.clone(), id.to_string(), version.clone());
        let fetched = detached(async move {
            let Some(client) = &state.feed.mirror else {
                return Ok(0);
            };
            let target = mirror::MirrorTarget {
                client,
                storage: state.storage.as_ref(),
                db: state.db.as_ref(),
                feed: state.feed(),
                temp_dir: &state.temp_dir,
                options: &options,
            };
            target.ensure_version(&owned_id, &owned_version).await
        })
        .await;
        if let Err(e) = fetched {
            tracing::warn!(feed = %self.feed(), id, version = %version.normalized(), error = %e, "mirror fetch failed");
        }
    }
}

/// Delete upload temp files left over from a previous run.
///
/// Every error path removes its own temp file, but nothing can run when the
/// process does not get to return: a SIGKILL, an OOM, or the forced close after
/// the shutdown grace period while an upload is still streaming. Each leak is as
/// large as the package that was in flight, they accumulate across restarts, and
/// they sit on the same filesystem as the package store — so left alone they
/// eventually fill the volume holding the packages.
///
/// Startup is the safe moment: none of this process's uploads can be in flight
/// yet. Best-effort throughout — a directory we cannot read is not a reason to
/// refuse to start.
async fn sweep_stale_uploads(temp_dir: &std::path::Path) {
    let Ok(mut entries) = tokio::fs::read_dir(temp_dir).await else {
        return;
    };
    let (mut removed, mut bytes) = (0u64, 0u64);
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !is_stale_temp_name(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let size = entry.metadata().await.map(|m| m.len()).unwrap_or(0);
        if tokio::fs::remove_file(&path).await.is_ok() {
            removed += 1;
            bytes = bytes.saturating_add(size);
        }
    }
    if removed > 0 {
        tracing::info!(
            files = removed,
            bytes,
            "removed upload temp files left by a previous run"
        );
    }
}

/// Whether a name in the staging directory is something only a running process
/// uses: every temp and staged file is `*.tmp`, and before those the inbox
/// staged as `inbox-*.part`. A resumable upload's `{id}.part` is not one; it
/// outlives restarts on purpose and goes when the upload expires.
fn is_stale_temp_name(name: &str) -> bool {
    name.ends_with(".tmp") || (name.starts_with("inbox-") && name.ends_with(".part"))
}
