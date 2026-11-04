//! Per-package fetch state: when a package was last listed, which
//! upstream failures to back off from, and which fetches are running.

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::watch;

use super::{MirrorClient, MIRROR_BUDGET};

/// How long a request waits for another request's fetch of the same package
/// before answering with what the feed has.
pub(super) const WAIT_FOR_FETCH: Duration = MIRROR_BUDGET;

/// The first retry after an upstream failure; each further failure in a row
/// doubles it, up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(30);
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// How long a version that failed to mirror (absent upstream, over the size
/// cap, the wrong identity, refused by policy) is not asked for again.
pub(super) const BAD_VERSION_RETRY: Duration = Duration::from_secs(15 * 60);

/// Packages whose fetch state is remembered. Past this, entries with nothing
/// left to remember go first, then the oldest; forgetting one only costs an
/// upstream request.
const MAX_TRACKED: usize = 10_000;

/// Consecutive upstream failures, and when asking again is allowed.
#[derive(Debug, Default)]
pub(super) struct Backoff {
    failures: u32,
    until: Option<Instant>,
}

impl Backoff {
    pub(super) fn active(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }

    pub(super) fn fail(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        let factor = 1u32 << (self.failures - 1).min(10);
        self.until = Some(now + (FIRST_BACKOFF * factor).min(MAX_BACKOFF));
    }

    pub(super) fn succeed(&mut self) {
        *self = Self::default();
    }
}

/// What the mirror remembers about one package in one feed.
#[derive(Default)]
pub(super) struct IdState {
    /// When the upstream version list was last fetched *and* worked through.
    /// A fetch the budget cut short does not count, so the next read carries
    /// on; an empty list does, which is the negative cache for unknown ids.
    pub(super) listed_at: Option<Instant>,
    /// Upstream failures for this package.
    pub(super) backoff: Backoff,
    /// Versions that failed to mirror, until when.
    pub(super) bad_versions: HashMap<String, Instant>,
    /// The fetch in progress, if any: its number and a receiver that wakes
    /// when it ends (its sender is dropped).
    inflight: Option<(u64, watch::Receiver<()>)>,
    touched: Option<Instant>,
    /// The upstream's listing of the package as last read, and when. A
    /// refresh reads it anew; the version fetches of read-through requests in
    /// between share it, so a restore missing several versions reads the
    /// registration once rather than once per version.
    pub(super) listing: Option<(Instant, Listing)>,
}

/// The upstream's listing of one package (see
/// [`MirrorClient::upstream_listing`]).
pub(super) type Listing = Arc<HashMap<String, bool>>;

impl IdState {
    /// Whether nothing about this package needs remembering any more.
    fn is_idle(&self, now: Instant, refresh: Duration) -> bool {
        self.inflight.is_none()
            && !self.backoff.active(now)
            && self.bad_versions.values().all(|until| *until <= now)
            && self
                .listed_at
                .is_none_or(|at| now.duration_since(at) >= refresh)
    }
}

/// Per-package fetch state for one mirror client: when each package was
/// last listed, which upstream failures to back off from, and which fetches
/// are running — keyed by (feed, id), so two feeds mirroring the same package
/// never wait on each other.
#[derive(Default)]
pub(super) struct Tracker {
    ids: StdMutex<HashMap<(String, String), IdState>>,
    /// The service index itself failing: backs off every package at once.
    upstream: StdMutex<Backoff>,
    next_fetch: std::sync::atomic::AtomicU64,
}

/// Either this request fetches, or it waits for the one that already is.
pub(super) enum Turn<'a> {
    Lead(FetchGuard<'a>),
    Wait(watch::Receiver<()>),
}

/// Held while a request fetches a package. Dropping it — when the fetch
/// ends, or when the request is cancelled mid-way — wakes every waiter.
pub(super) struct FetchGuard<'a> {
    tracker: &'a Tracker,
    key: (String, String),
    fetch: u64,
    _done: watch::Sender<()>,
}

impl Drop for FetchGuard<'_> {
    fn drop(&mut self) {
        let mut ids = self.tracker.lock();
        if let Some(state) = ids.get_mut(&self.key) {
            if state
                .inflight
                .as_ref()
                .is_some_and(|(n, _)| *n == self.fetch)
            {
                state.inflight = None;
            }
        }
    }
}

impl Tracker {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), IdState>> {
        // A panic while holding the lock leaves only fetch bookkeeping behind.
        self.ids.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn upstream(&self) -> std::sync::MutexGuard<'_, Backoff> {
        self.upstream.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn with<T>(
        &self,
        key: &(String, String),
        refresh: Duration,
        f: impl FnOnce(&mut IdState) -> T,
    ) -> T {
        let now = Instant::now();
        let mut ids = self.lock();
        if !ids.contains_key(key) && ids.len() >= MAX_TRACKED {
            ids.retain(|_, s| !s.is_idle(now, refresh));
            if ids.len() >= MAX_TRACKED {
                let mut by_age: Vec<((String, String), Option<Instant>)> = ids
                    .iter()
                    .filter(|(_, s)| s.inflight.is_none())
                    .map(|(k, s)| (k.clone(), s.touched))
                    .collect();
                by_age.sort_by_key(|(_, touched)| *touched);
                for (k, _) in by_age.into_iter().take(ids.len() + 1 - MAX_TRACKED * 3 / 4) {
                    ids.remove(&k);
                }
            }
        }
        let state = ids.entry(key.clone()).or_default();
        state.touched = Some(now);
        f(state)
    }

    /// Take this package's fetch, or join the one in progress.
    pub(super) fn turn(&self, key: &(String, String), refresh: Duration) -> Turn<'_> {
        let fetch = self
            .next_fetch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let lead = self.with(key, refresh, |state| {
            if let Some((_, rx)) = &state.inflight {
                return Err(rx.clone());
            }
            let (tx, rx) = watch::channel(());
            state.inflight = Some((fetch, rx));
            Ok(tx)
        });
        match lead {
            Ok(tx) => Turn::Lead(FetchGuard {
                tracker: self,
                key: key.clone(),
                fetch,
                _done: tx,
            }),
            Err(rx) => Turn::Wait(rx),
        }
    }
}

impl MirrorClient {
    pub(super) fn key(feed: &str, lower_id: &str) -> (String, String) {
        (feed.to_string(), lower_id.to_string())
    }

    /// Whether a read of `id` in `feed` should re-list it upstream: nothing is
    /// fetching it, the upstream is not being backed off, and its version list
    /// is older than `refresh_secs` (or was never fetched by this process).
    pub fn wants_refresh(&self, feed: &str, id: &str) -> bool {
        self.list_is_stale(feed, &id.to_lowercase(), true)
    }

    /// Whether the version list of `lower_id` is due to be fetched again;
    /// with `idle`, also that no fetch of it is running right now.
    pub(super) fn list_is_stale(&self, feed: &str, lower_id: &str, idle: bool) -> bool {
        let now = Instant::now();
        if self.tracker.upstream().active(now) {
            return false;
        }
        self.tracker
            .with(&Self::key(feed, lower_id), self.refresh, |state| {
                (!idle || state.inflight.is_none())
                    && !state.backoff.active(now)
                    && state
                        .listed_at
                        .is_none_or(|at| now.duration_since(at) >= self.refresh)
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_a_cap_and_resets_on_success() {
        let now = Instant::now();
        let mut backoff = Backoff::default();
        assert!(!backoff.active(now));
        backoff.fail(now);
        assert_eq!(backoff.until, Some(now + FIRST_BACKOFF));
        backoff.fail(now);
        assert_eq!(backoff.until, Some(now + FIRST_BACKOFF * 2));
        for _ in 0..40 {
            backoff.fail(now);
        }
        assert_eq!(backoff.until, Some(now + MAX_BACKOFF));
        assert!(backoff.active(now));
        backoff.succeed();
        assert!(!backoff.active(now));
    }

    #[test]
    fn the_fetch_tracker_stays_bounded() {
        // Every unknown id an anonymous client names gets an entry (that is
        // the negative cache); the map must not grow without end.
        let tracker = Tracker::default();
        let refresh = Duration::from_secs(600);
        for i in 0..MAX_TRACKED * 2 {
            tracker.with(&("feed".into(), format!("pkg.{i}")), refresh, |s| {
                s.listed_at = Some(Instant::now());
            });
        }
        assert!(tracker.lock().len() <= MAX_TRACKED);
    }

    #[test]
    fn only_one_fetch_per_package_and_feed_runs_at_once() {
        let tracker = Tracker::default();
        let refresh = Duration::from_secs(600);
        let a = ("a".to_string(), "pkg".to_string());
        let b = ("b".to_string(), "pkg".to_string());
        let Turn::Lead(first) = tracker.turn(&a, refresh) else {
            panic!("the first fetch leads");
        };
        let Turn::Wait(waiting) = tracker.turn(&a, refresh) else {
            panic!("a second fetch of the same package waits");
        };
        // Another feed mirroring the same id does not wait on this one.
        assert!(matches!(tracker.turn(&b, refresh), Turn::Lead(_)));
        drop(first);
        assert!(waiting.has_changed().is_err(), "the waiter is woken");
        assert!(matches!(tracker.turn(&a, refresh), Turn::Lead(_)));
    }
}
