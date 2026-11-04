//! Upstream mirroring: read-through caching of a public NuGet feed.
//!
//! When a feed has a `[feeds.mirror]` upstream configured and a client asks for
//! a package the feed does not have yet, the server fetches that package's
//! versions from the upstream V3 feed, streams each `.nupkg` to disk and indexes
//! it into the local feed — after which every later request is served locally.
//!
//! Mirrored versions go through the same [`crate::indexing`] pipeline as a push,
//! so the streaming/large-package guarantees and the feed's license policy
//! apply. When the feed `requires_approval`, mirrored versions land **pending**
//! and are withheld from clients until an admin approves them in `/admin` —
//! turning the mirror into a curated, approval-gated cache.

mod client;
mod health;
mod listing;
mod target;
mod tracker;
mod upstream;

pub use client::redact_url;
pub use health::UpstreamStatus;
pub use target::{ensure_package, MirrorOptions, MirrorTarget};
pub use upstream::Enumeration;

use std::time::Duration;

use tokio::sync::OnceCell;

use client::Origin;
use health::UpstreamHealth;
use tracker::Tracker;
use upstream::MirrorResources;

/// A client for one upstream V3 feed, with its service-index resources resolved
/// lazily on first use and cached thereafter.
pub struct MirrorClient {
    client: reqwest::Client,
    upstream: String,
    /// The service index's origin: the only one the credentials are sent to.
    upstream_origin: Origin,
    /// The configured credentials, attached per request by [`Self::send`] —
    /// never as client-wide defaults, which go wherever a request goes.
    credentials: reqwest::header::HeaderMap,
    /// Permit upstream resource URLs that resolve to private/loopback hosts.
    allow_private_upstream: bool,
    /// Requests go through a proxy, which resolves the names itself: each
    /// host is resolved here as well ([`Self::check_proxied`]).
    proxied: bool,
    /// Names [`Self::check_proxied`] answers without a lookup, as the
    /// guarded resolver does; empty outside tests.
    fixed_dns: Vec<(String, std::net::IpAddr)>,
    /// Cap on a single mirrored `.nupkg`, in bytes.
    max_package_size_bytes: Option<u64>,
    /// Cap on versions fetched for one read-through miss.
    max_versions_per_package: Option<usize>,
    /// Deadline for one metadata request (service index, search or catalog
    /// page, version list), from `timeout_secs`.
    timeout: Duration,
    /// Deadline for one whole `.nupkg` download. `None` leaves only the read
    /// timeout, which bounds how long the upstream may go silent.
    download_deadline: Option<Duration>,
    /// Free space a download must leave on the volume (`min_free_disk_bytes`),
    /// when no [`space`](Self::set_space) is shared with the server's uploads.
    min_free_disk_bytes: u64,
    /// The admission of downloads, shared with the server's uploads.
    space: Option<std::sync::Arc<crate::space::SpaceGuard>>,
    /// How long a package's upstream version list is trusted (`refresh_secs`).
    refresh: Duration,
    resources: OnceCell<MirrorResources>,
    tracker: Tracker,
    /// Whether the upstream answers, and what the mirror fetched from it.
    health: UpstreamHealth,
    /// The feed this client fills, for log lines. Empty outside the server.
    feed: String,
}

impl std::fmt::Debug for MirrorClient {
    /// The upstream without its userinfo, and the credentials by header name
    /// only: a `{:?}` in a log line must not print the secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirrorClient")
            .field("upstream", &redact_url(&self.upstream))
            .field("credentials", &self.credentials.keys().collect::<Vec<_>>())
            .field("allow_private_upstream", &self.allow_private_upstream)
            .field("max_package_size_bytes", &self.max_package_size_bytes)
            .field("max_versions_per_package", &self.max_versions_per_package)
            .field("timeout", &self.timeout)
            .field("download_deadline", &self.download_deadline)
            .finish_non_exhaustive()
    }
}

/// How long one read-through miss may spend fetching before it gives up and
/// answers with what it has.
///
/// `ensure_package` downloads up to `max_versions_per_package` (50 by default)
/// `.nupkg`s one after another, each bounded only by the per-request timeout —
/// so a single `GET /v3/package/{id}/index.json`, which needs no
/// authentication, could hold a connection open for twenty-five minutes. The
/// mirror is a cache: stopping early is not a failure, because the versions
/// already fetched are kept and the next request continues from there.
const MIRROR_BUDGET: Duration = Duration::from_secs(60);
