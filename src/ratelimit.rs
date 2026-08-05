//! A small per-IP request rate limiter (brute-force mitigation).
//!
//! A fixed window of `window` allows at most `max` requests per client IP; once
//! exceeded, requests are rejected with `429 Too Many Requests` until the window
//! rolls over. State is held per limiter instance (one per built router) rather
//! than in a global, so independently built apps — including each integration
//! test's server — never share counters.
//!
//! The client IP comes from `X-Forwarded-For`/`X-Real-IP` when present (proxied
//! deployments) and otherwise the connection's peer address. When neither is
//! available the request is allowed through, so the limiter never returns false
//! positives for a deployment that has not wired connection info.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

#[derive(Debug)]
struct Bucket {
    window_start: Instant,
    count: u32,
}

#[derive(Debug, Default)]
struct Inner {
    buckets: HashMap<IpAddr, Bucket>,
    /// Sweep expired buckets once the map grows to at least this size.
    sweep_at: usize,
}

/// A cloneable, per-IP fixed-window rate limiter.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    inner: Arc<Mutex<Inner>>,
    max: u32,
    window: Duration,
}

impl RateLimiter {
    /// Build a limiter allowing `max_requests` (clamped to at least 1) per
    /// `window`.
    pub fn new(max_requests: u32, window: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            max: max_requests.max(1),
            window,
        }
    }

    /// Window length in whole seconds, for the `Retry-After` header.
    pub fn window_secs(&self) -> u64 {
        self.window.as_secs().max(1)
    }

    /// Record a request from `ip`; returns `true` if it is within the limit.
    pub fn check(&self, ip: IpAddr) -> bool {
        self.check_at(ip, Instant::now())
    }

    fn check_at(&self, ip: IpAddr, now: Instant) -> bool {
        // Never `expect`: this runs on every request when the limiter is on,
        // and a poisoned mutex is permanent. One panic anywhere under this
        // guard would turn every subsequent request into a panic in
        // middleware, with the process still answering `/health/live` while
        // serving nothing. The guarded data is a plain map — a panic mid-update
        // leaves it merely stale, which is a far better outcome than a server
        // that is up and refuses everything.
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Amortised cleanup of expired buckets keeps the map bounded.
        if inner.buckets.len() >= inner.sweep_at {
            let window = self.window;
            inner
                .buckets
                .retain(|_, b| now.duration_since(b.window_start) < window);
            inner.sweep_at = inner.buckets.len() * 2 + 64;
        }
        let bucket = inner.buckets.entry(ip).or_insert(Bucket {
            window_start: now,
            count: 0,
        });
        if now.duration_since(bucket.window_start) >= self.window {
            bucket.window_start = now;
            bucket.count = 0;
        }
        if bucket.count >= self.max {
            return false;
        }
        bucket.count += 1;
        true
    }
}

/// Resolve the client IP, preferring proxy headers over the peer address.
fn client_ip(headers: &HeaderMap, connect: Option<&ConnectInfo<SocketAddr>>) -> Option<IpAddr> {
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(ip) = xff.split(',').next().and_then(|s| s.trim().parse().ok()) {
            return Some(ip);
        }
    }
    if let Some(xr) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        if let Ok(ip) = xr.trim().parse() {
            return Some(ip);
        }
    }
    connect.map(|c| c.0.ip())
}

/// Axum middleware enforcing [`RateLimiter`]. Wire it with
/// `from_fn_with_state(limiter, enforce)`.
pub async fn enforce(State(limiter): State<RateLimiter>, req: Request, next: Next) -> Response {
    let ip = client_ip(
        req.headers(),
        req.extensions().get::<ConnectInfo<SocketAddr>>(),
    );
    if let Some(ip) = ip {
        if !limiter.check(ip) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, limiter.window_secs().to_string())],
                Json(serde_json::json!({ "error": "rate limit exceeded" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, n])
    }

    #[test]
    fn blocks_after_limit_and_resets_after_window() {
        let limiter = RateLimiter::new(3, Duration::from_secs(60));
        let start = Instant::now();
        // First three from one IP pass, the fourth is blocked.
        assert!(limiter.check_at(ip(1), start));
        assert!(limiter.check_at(ip(1), start));
        assert!(limiter.check_at(ip(1), start));
        assert!(!limiter.check_at(ip(1), start));
        // A different IP has its own budget.
        assert!(limiter.check_at(ip(2), start));
        // After the window elapses, the first IP is allowed again.
        let later = start + Duration::from_secs(61);
        assert!(limiter.check_at(ip(1), later));
    }

    #[test]
    fn max_zero_is_clamped_to_one() {
        let limiter = RateLimiter::new(0, Duration::from_secs(60));
        let now = Instant::now();
        assert!(limiter.check_at(ip(1), now));
        assert!(!limiter.check_at(ip(1), now));
    }

    #[test]
    fn expired_buckets_are_swept() {
        let limiter = RateLimiter::new(5, Duration::from_secs(1));
        let mut now = Instant::now();
        // Touch many distinct IPs, each well past the window before the next.
        for n in 0..=200u32 {
            now += Duration::from_secs(2);
            let octets = n.to_be_bytes();
            limiter.check_at(IpAddr::from(octets), now);
        }
        let inner = limiter.inner.lock().unwrap();
        assert!(inner.buckets.len() <= inner.sweep_at);
    }
}
