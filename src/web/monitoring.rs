//! The monitoring endpoints behind `[monitoring]`: `GET /metrics` in the
//! Prometheus text format and `GET /health/status` as JSON.
//!
//! Both report the same things: whether the database answers, how many
//! packages each feed holds, what was pushed, promoted and downloaded since
//! startup, and whether each mirror's upstream answers. The plain probes
//! (`/health`, `/health/live`, `/health/ready`) stay as they are: an upstream
//! outage must not make an orchestrator restart a server that still serves
//! everything it holds.
//!
//! The database's share (the per-feed aggregate scans) is cached for a few
//! seconds, so a scraper or an anonymous caller hammering the endpoints costs
//! one scan per interval, not one per request.

use std::fmt::Write as _;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;

use super::AppState;
use crate::database::DatabaseStats;
use crate::mirror::{redact_url, UpstreamStatus};

/// The Prometheus text exposition format.
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

/// How long one round of aggregate scans answers for.
const SCAN_TTL: Duration = Duration::from_secs(5);

struct Monitor {
    states: Vec<AppState>,
    token: Option<String>,
    started: Instant,
    scans: Recent<Scan>,
}

/// What the database said: whether it answered, and each feed's statistics
/// (in `states` order).
#[derive(Clone)]
struct Scan {
    db_up: bool,
    stats: Vec<Option<DatabaseStats>>,
}

/// A value computed at most once per `ttl`, single-flight: a caller that
/// arrives while another refreshes waits for that refresh and takes its
/// result rather than starting its own.
///
/// The value is also refreshed when `writes` changed since it was computed,
/// so what this process itself changed shows up at once.
struct Recent<T> {
    slot: tokio::sync::Mutex<Option<(Instant, u64, T)>>,
}

impl<T: Clone> Recent<T> {
    fn new() -> Self {
        Self {
            slot: tokio::sync::Mutex::new(None),
        }
    }

    async fn get(
        &self,
        ttl: Duration,
        writes: u64,
        refresh: impl std::future::Future<Output = T>,
    ) -> T {
        // Held across the refresh: that is what makes it single-flight.
        let mut slot = self.slot.lock().await;
        if let Some((taken, w, value)) = slot.as_ref() {
            if *w == writes && taken.elapsed() < ttl {
                return value.clone();
            }
        }
        let value = refresh.await;
        *slot = Some((Instant::now(), writes, value.clone()));
        value
    }
}

/// One feed's numbers, gathered once per request.
struct FeedReport<'a> {
    state: &'a AppState,
    /// `None` when the database could not answer.
    stats: Option<DatabaseStats>,
    mirror: Option<(String, UpstreamStatus)>,
}

/// The routes, when `[monitoring]` is enabled; an empty router otherwise.
pub(super) fn routes(states: &[AppState]) -> Router {
    let Some(first) = states.first() else {
        return Router::new();
    };
    let config = &first.config.monitoring;
    if !config.enabled {
        return Router::new();
    }
    let monitor = Arc::new(Monitor {
        states: states.to_vec(),
        token: config
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        started: Instant::now(),
        scans: Recent::new(),
    });
    Router::new()
        .route("/metrics", get(metrics))
        .route("/health/status", get(status))
        .with_state(monitor)
}

impl Monitor {
    /// Whether the configured token (if any) was presented.
    fn authorized(&self, headers: &HeaderMap) -> bool {
        self.token
            .as_deref()
            .is_none_or(|token| crate::auth::bearer_matches(headers, token))
    }

    /// The changes this process made that the scans would show: pushes,
    /// promotions and mirrored versions, over every feed. Each only grows, so
    /// an unchanged sum means none of them moved. Downloads are left out on
    /// purpose: anyone can make them, and they would defeat the cache.
    fn writes(&self) -> u64 {
        self.states
            .iter()
            .map(|state| {
                let counters = &state.feed.counters;
                counters.pushes.load(Ordering::Relaxed)
                    + counters.promotions.load(Ordering::Relaxed)
                    + state
                        .feed
                        .mirror
                        .as_ref()
                        .map_or(0, |m| m.upstream_status().mirrored)
            })
            .sum()
    }

    /// The database probe and the per-feed aggregate scans, uncached.
    async fn scan(&self) -> Scan {
        let db_up = match self.states.first() {
            Some(state) => match state.db.ping().await {
                Ok(()) => true,
                Err(e) => {
                    tracing::error!(error = %e, "monitoring: database probe failed");
                    false
                }
            },
            None => false,
        };
        let mut stats = Vec::with_capacity(self.states.len());
        for state in &self.states {
            stats.push(if db_up {
                match state.db.stats(state.feed()).await {
                    Ok(stats) => Some(stats),
                    Err(e) => {
                        tracing::warn!(feed = %state.feed(), error = %e, "monitoring: feed statistics failed");
                        None
                    }
                }
            } else {
                None
            });
        }
        Scan { db_up, stats }
    }

    /// Everything both endpoints report: the database's share from the last
    /// [`SCAN_TTL`], the in-memory counters and mirror states as they are now.
    async fn gather(&self) -> (bool, Vec<FeedReport<'_>>) {
        let scan = self.scans.get(SCAN_TTL, self.writes(), self.scan()).await;
        let feeds = self
            .states
            .iter()
            .zip(scan.stats)
            .map(|(state, stats)| FeedReport {
                state,
                stats,
                mirror: state
                    .feed
                    .mirror
                    .as_ref()
                    .map(|m| (redact_url(m.upstream()), m.upstream_status())),
            })
            .collect();
        (scan.db_up, feeds)
    }
}

async fn status(State(monitor): State<Arc<Monitor>>, headers: HeaderMap) -> Response {
    if !monitor.authorized(&headers) {
        return unauthorized();
    }
    let (db_up, feeds) = monitor.gather().await;
    let degraded = feeds.iter().any(|f| {
        f.mirror
            .as_ref()
            .is_some_and(|(_, m)| m.reachable() == Some(false))
    });
    let (code, overall) = if !db_up {
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    } else if degraded {
        (StatusCode::OK, "degraded")
    } else {
        (StatusCode::OK, "ok")
    };
    let feeds: Vec<serde_json::Value> = feeds.iter().map(feed_json).collect();
    let body = json!({
        "status": overall,
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_seconds": monitor.started.elapsed().as_secs(),
        "database": if db_up { "ok" } else { "unavailable" },
        "feeds": feeds,
    });
    (code, Json(body)).into_response()
}

fn feed_json(report: &FeedReport<'_>) -> serde_json::Value {
    let counters = &report.state.feed.counters;
    let mut feed = json!({
        "name": report.state.feed(),
        "pushes": counters.pushes.load(Ordering::Relaxed),
        "promotions": counters.promotions.load(Ordering::Relaxed),
        "downloads": counters.downloads.load(Ordering::Relaxed),
    });
    if let Some(target) = &report.state.feed.promotes_to {
        feed["promotes_to"] = json!(target);
    }
    if let Some(stats) = &report.stats {
        feed["packages"] = json!(stats.package_count);
        feed["versions"] = json!(stats.version_count);
        feed["size_bytes"] = json!(stats.total_size);
    }
    if let Some((upstream, m)) = &report.mirror {
        let state = match m.reachable() {
            Some(true) => "ok",
            Some(false) => "unreachable",
            None => "unknown",
        };
        feed["mirror"] = json!({
            "upstream": upstream,
            "status": state,
            "last_success": m.last_success.map(rfc3339),
            "last_failure": m.last_failure.map(rfc3339),
            "last_error": m.last_error,
            "failing_since": m.failing_since.map(rfc3339),
            "consecutive_failures": m.consecutive_failures,
            "mirrored": m.mirrored,
        });
    }
    feed
}

fn unauthorized() -> Response {
    let mut response = (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    response
        .headers_mut()
        .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    response
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

async fn metrics(State(monitor): State<Arc<Monitor>>, headers: HeaderMap) -> Response {
    if !monitor.authorized(&headers) {
        return unauthorized();
    }
    let (db_up, feeds) = monitor.gather().await;
    let body = render_metrics(db_up, monitor.started.elapsed().as_secs(), &feeds);
    ([(header::CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)], body).into_response()
}

/// One metric family: its `HELP`/`TYPE` header and a sample per feed that has
/// a value for it.
fn family<'a, T: std::fmt::Display>(
    out: &mut String,
    name: &str,
    kind: &str,
    help: &str,
    feeds: &'a [FeedReport<'a>],
    value: impl Fn(&FeedReport<'a>) -> Option<(String, T)>,
) {
    let samples: Vec<(String, T)> = feeds.iter().filter_map(&value).collect();
    if samples.is_empty() {
        return;
    }
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    for (labels, v) in samples {
        let _ = writeln!(out, "{name}{{{labels}}} {v}");
    }
}

fn feed_label(report: &FeedReport<'_>) -> String {
    format!("feed=\"{}\"", escape_label(report.state.feed()))
}

fn render_metrics(db_up: bool, uptime: u64, feeds: &[FeedReport<'_>]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# HELP yanuget_build_info The running version.");
    let _ = writeln!(out, "# TYPE yanuget_build_info gauge");
    let _ = writeln!(
        out,
        "yanuget_build_info{{version=\"{}\"}} 1",
        escape_label(env!("CARGO_PKG_VERSION"))
    );
    let _ = writeln!(
        out,
        "# HELP yanuget_uptime_seconds Seconds since the server started."
    );
    let _ = writeln!(out, "# TYPE yanuget_uptime_seconds gauge");
    let _ = writeln!(out, "yanuget_uptime_seconds {uptime}");
    let _ = writeln!(
        out,
        "# HELP yanuget_database_up Whether the database answers (1) or not (0)."
    );
    let _ = writeln!(out, "# TYPE yanuget_database_up gauge");
    let _ = writeln!(out, "yanuget_database_up {}", u8::from(db_up));

    let stat = |f: fn(&DatabaseStats) -> i64| {
        move |r: &FeedReport<'_>| r.stats.as_ref().map(|s| (feed_label(r), f(s)))
    };
    family(
        &mut out,
        "yanuget_feed_packages",
        "gauge",
        "Distinct package ids served by the feed.",
        feeds,
        stat(|s| s.package_count),
    );
    family(
        &mut out,
        "yanuget_feed_versions",
        "gauge",
        "Package versions served by the feed (pending and disabled ones excluded).",
        feeds,
        stat(|s| s.version_count),
    );
    family(
        &mut out,
        "yanuget_feed_listed_versions",
        "gauge",
        "Versions of the feed that are listed.",
        feeds,
        stat(|s| s.listed_count),
    );
    family(
        &mut out,
        "yanuget_feed_size_bytes",
        "gauge",
        "Bytes of .nupkg files the feed serves.",
        feeds,
        stat(|s| s.total_size),
    );
    family(
        &mut out,
        "yanuget_feed_package_downloads",
        "gauge",
        "Downloads recorded in the database for the feed's versions.",
        feeds,
        stat(|s| s.total_downloads),
    );

    let counter = |f: fn(&super::FeedCounters) -> u64| {
        move |r: &FeedReport<'_>| Some((feed_label(r), f(&r.state.feed.counters)))
    };
    family(
        &mut out,
        "yanuget_pushes_total",
        "counter",
        "Packages pushed to the feed since startup.",
        feeds,
        counter(|c| c.pushes.load(Ordering::Relaxed)),
    );
    family(
        &mut out,
        "yanuget_downloads_total",
        "counter",
        "Package downloads served by the feed since startup.",
        feeds,
        counter(|c| c.downloads.load(Ordering::Relaxed)),
    );
    family(
        &mut out,
        "yanuget_promotions_total",
        "counter",
        "Versions promoted from the feed into its promotes_to feed since startup.",
        feeds,
        |r| {
            let target = r.state.feed.promotes_to.as_deref()?;
            Some((
                format!("{},target=\"{}\"", feed_label(r), escape_label(target)),
                r.state.feed.counters.promotions.load(Ordering::Relaxed),
            ))
        },
    );

    let mirror = |f: fn(&UpstreamStatus) -> Option<u64>| {
        move |r: &FeedReport<'_>| {
            let (_, m) = r.mirror.as_ref()?;
            Some((feed_label(r), f(m)?))
        }
    };
    family(
        &mut out,
        "yanuget_mirror_upstream_info",
        "gauge",
        "The upstream each mirrored feed reads through to.",
        feeds,
        |r| {
            let (upstream, _) = r.mirror.as_ref()?;
            Some((
                format!("{},upstream=\"{}\"", feed_label(r), escape_label(upstream)),
                1,
            ))
        },
    );
    family(
        &mut out,
        "yanuget_mirror_upstream_up",
        "gauge",
        "Whether the mirror's upstream answered its last request (1) or not (0). \
         Absent until the first request.",
        feeds,
        mirror(|m| m.reachable().map(u64::from)),
    );
    family(
        &mut out,
        "yanuget_mirror_upstream_consecutive_failures",
        "gauge",
        "Failed upstream requests since the last success.",
        feeds,
        mirror(|m| Some(m.consecutive_failures)),
    );
    family(
        &mut out,
        "yanuget_mirror_upstream_last_success_timestamp_seconds",
        "gauge",
        "Unix time of the last successful upstream request.",
        feeds,
        mirror(|m| m.last_success.map(|t| t.timestamp().max(0) as u64)),
    );
    family(
        &mut out,
        "yanuget_mirror_upstream_requests_total",
        "counter",
        "Requests sent to the upstream since startup.",
        feeds,
        mirror(|m| Some(m.requests)),
    );
    family(
        &mut out,
        "yanuget_mirror_upstream_failures_total",
        "counter",
        "Upstream requests that failed (unreachable, 5xx, 401/403/407/429) since startup.",
        feeds,
        mirror(|m| Some(m.failures)),
    );
    family(
        &mut out,
        "yanuget_mirror_packages_total",
        "counter",
        "Versions fetched from the upstream into the feed since startup.",
        feeds,
        mirror(|m| Some(m.mirrored)),
    );
    family(
        &mut out,
        "yanuget_mirror_package_failures_total",
        "counter",
        "Versions that failed to download or index since startup.",
        feeds,
        mirror(|m| Some(m.mirror_failures)),
    );
    out
}

/// Escape a label value for the text format: backslash, quote and newline.
fn escape_label(v: &str) -> String {
    v.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(escape_label("a\nb"), "a\\nb");
    }

    async fn counted(calls: &AtomicUsize) -> usize {
        tokio::time::sleep(Duration::from_millis(20)).await;
        calls.fetch_add(1, Ordering::SeqCst) + 1
    }

    #[tokio::test]
    async fn concurrent_callers_share_one_refresh() {
        let recent = Recent::new();
        let calls = AtomicUsize::new(0);
        let results = futures::future::join_all(
            (0..8).map(|_| recent.get(Duration::from_secs(60), 0, counted(&calls))),
        )
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(results.iter().all(|&r| r == 1));
        // Still fresh: answered from the cache.
        assert_eq!(
            recent
                .get(Duration::from_secs(60), 0, counted(&calls))
                .await,
            1
        );
    }

    #[tokio::test]
    async fn a_write_or_an_expired_value_refreshes() {
        let recent = Recent::new();
        let calls = AtomicUsize::new(0);
        let ttl = Duration::from_secs(60);
        assert_eq!(recent.get(ttl, 0, counted(&calls)).await, 1);
        assert_eq!(recent.get(ttl, 1, counted(&calls)).await, 2);
        assert_eq!(recent.get(ttl, 1, counted(&calls)).await, 2);
        assert_eq!(recent.get(Duration::ZERO, 1, counted(&calls)).await, 3);
    }
}
