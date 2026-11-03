//! Whether the upstream answers: the status the server reports and the
//! probe that keeps it current.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;

use crate::error::Result;

use super::client::redact_url;
use super::upstream::not_found;
use super::MirrorClient;

impl MirrorClient {
    /// Name the feed this client fills, so its log lines say which one.
    pub fn set_feed_name(&mut self, feed: &str) {
        self.feed = feed.to_string();
    }

    /// Whether the upstream answers, as far as this process has seen.
    pub fn upstream_status(&self) -> UpstreamStatus {
        self.health.status()
    }

    /// Versions mirrored into the feed since startup: the one counter of
    /// [`Self::upstream_status`], without its lock and copy.
    pub fn mirrored(&self) -> u64 {
        self.health
            .mirrored
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Ask the upstream for its service index, outside any client request.
    ///
    /// Read-through traffic alone only notices an outage when someone asks
    /// for a package the feed lacks, which on a feed that already holds what
    /// its clients restore can be days later. The server runs this every
    /// `probe_interval_secs` so the status (and the log) stays current.
    /// The cached resources are left alone: this only asks whether the
    /// upstream answers.
    pub async fn probe(&self) -> Result<()> {
        // Any JSON will do; the index is read when the resources are.
        let index = self.get_json::<serde::de::IgnoredAny>(&self.upstream, "service index");
        match index.await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => {
                let e = not_found("service index", &self.upstream);
                self.record_failure(&e.to_string());
                Err(e)
            }
            Err(e) => {
                // A body that is not JSON passed `send` as an answer; it is
                // still not a usable upstream.
                if self.health.status().consecutive_failures == 0 {
                    self.record_failure(&e.to_string());
                }
                Err(e)
            }
        }
    }

    pub(super) fn record_success(&self) {
        if let Some((failures, since)) = self.health.succeed() {
            tracing::info!(
                feed = %self.feed,
                upstream = %redact_url(&self.upstream),
                failures,
                down_since = %since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "mirror upstream reachable again"
            );
        }
    }

    pub(super) fn record_failure(&self, error: &str) {
        if self.health.fail(error) {
            tracing::warn!(
                feed = %self.feed,
                upstream = %redact_url(&self.upstream),
                %error,
                "mirror upstream unreachable; the feed serves only what it already holds"
            );
        }
    }
}

/// Whether the upstream answers, for `/health/status`, `/metrics` and the
/// log: the outcome of every request the client sent, plus counters of what
/// it fetched.
#[derive(Default)]
pub(super) struct UpstreamHealth {
    state: StdMutex<UpstreamStatus>,
    /// Versions downloaded and indexed into the feed.
    pub(super) mirrored: AtomicU64,
    /// Versions that failed to download or index.
    pub(super) mirror_failures: AtomicU64,
}

/// A snapshot of an upstream's reachability and of what was fetched from it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpstreamStatus {
    /// The last answer the upstream gave that was not a failure.
    pub last_success: Option<chrono::DateTime<chrono::Utc>>,
    /// The last failed request.
    pub last_failure: Option<chrono::DateTime<chrono::Utc>>,
    /// What the last failure was, redacted like every upstream log line.
    pub last_error: Option<String>,
    /// Failed requests since the last success; `0` while the upstream answers.
    pub consecutive_failures: u64,
    /// The first failure of the current run of failures.
    pub failing_since: Option<chrono::DateTime<chrono::Utc>>,
    /// Requests sent to the upstream since startup.
    pub requests: u64,
    /// Requests that failed since startup.
    pub failures: u64,
    /// Versions mirrored into the feed since startup.
    pub mirrored: u64,
    /// Versions that failed to download or index since startup.
    pub mirror_failures: u64,
}

impl UpstreamStatus {
    /// `Some(true)` while the upstream answers, `Some(false)` while it fails,
    /// `None` before anything was asked of it.
    pub fn reachable(&self) -> Option<bool> {
        if self.consecutive_failures > 0 {
            Some(false)
        } else if self.last_success.is_some() {
            Some(true)
        } else {
            None
        }
    }
}

impl UpstreamHealth {
    fn lock(&self) -> std::sync::MutexGuard<'_, UpstreamStatus> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn status(&self) -> UpstreamStatus {
        UpstreamStatus {
            mirrored: self.mirrored.load(Ordering::Relaxed),
            mirror_failures: self.mirror_failures.load(Ordering::Relaxed),
            ..self.lock().clone()
        }
    }

    /// Record a success. Returns how many failures in a row it ends, and
    /// since when, when it ends any.
    fn succeed(&self) -> Option<(u64, chrono::DateTime<chrono::Utc>)> {
        let mut state = self.lock();
        state.requests += 1;
        state.last_success = Some(chrono::Utc::now());
        let ended = state
            .failing_since
            .take()
            .map(|since| (state.consecutive_failures, since));
        state.consecutive_failures = 0;
        ended
    }

    /// Record a failure. Returns whether it is the first of a run, which is
    /// when the log says so.
    fn fail(&self, error: &str) -> bool {
        let now = chrono::Utc::now();
        let mut state = self.lock();
        state.requests += 1;
        state.failures += 1;
        state.consecutive_failures += 1;
        state.last_failure = Some(now);
        state.last_error = Some(error.to_string());
        let first = state.failing_since.is_none();
        state.failing_since.get_or_insert(now);
        first
    }
}
