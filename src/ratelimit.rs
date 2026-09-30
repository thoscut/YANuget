//! A small per-IP request rate limiter (brute-force mitigation).
//!
//! A fixed window of `window` allows at most `max` requests per client IP; once
//! exceeded, requests are rejected with `429 Too Many Requests` until the window
//! rolls over. State is held per limiter instance (one per built router) rather
//! than in a global, so independently built apps — including each integration
//! test's server — never share counters.
//!
//! The client is the connection's peer address, unless that peer is a trusted
//! proxy: then `X-Forwarded-For` is walked from the right, past every hop that
//! is itself a trusted proxy, to the first address nobody vouched for (see
//! [`client_ip`]). IPv6 clients are bucketed by their /64, since that is what
//! one subscriber is routinely handed. When no peer is known the request is
//! allowed through, so the limiter never returns false positives for a
//! deployment that has not wired connection info.
//!
//! A second, much smaller budget counts failed authentications: responses of
//! `401` to requests that carried a credential. Guessing a key is the one
//! thing a client does that is both cheap for it and worth bounding tightly,
//! and the general budget has to clear a large restore.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::proxy::TrustedProxies;

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

    /// Whether `ip` has used up its budget, without spending any of it.
    pub fn is_exhausted(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner.buckets.get(&bucket_key(ip)).is_some_and(|b| {
            now.duration_since(b.window_start) < self.window && b.count >= self.max
        })
    }

    fn check_at(&self, ip: IpAddr, now: Instant) -> bool {
        let ip = bucket_key(ip);
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

/// The bucket a client address counts against.
///
/// An IPv4-mapped IPv6 address (what a dual-stack listener reports for an IPv4
/// peer) is the IPv4 client it maps. Any other IPv6 address counts as its /64:
/// a single subscriber is routinely handed a whole /64, so bucketing per /128
/// gave one client 2^64 separate budgets to rotate through.
fn bucket_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let s = v6.segments();
            IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
        }
        v4 => v4,
    }
}

/// Parse one `X-Forwarded-For` entry: an address, optionally with a port.
fn forwarded_addr(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    entry
        .parse::<IpAddr>()
        .or_else(|_| entry.parse::<SocketAddr>().map(|s| s.ip()))
        .ok()
        .map(|ip| ip.to_canonical())
}

/// Resolve the client a request is throttled as.
///
/// Forwarding headers only count when the connection peer is a trusted proxy.
/// Then `X-Forwarded-For` is read **from the right**: each proxy appends the
/// address it received the request from, so the rightmost entries were written
/// by proxies we trust and the leftmost by whoever sent the request first —
/// possibly the client itself, choosing a fresh value per request (nginx's
/// usual `$proxy_add_x_forwarded_for` keeps what the client sent). The client
/// is the first address, walking leftwards, that is not itself a trusted proxy.
/// An entry that does not parse ends the walk at the last hop that did.
///
/// `X-Real-IP` is only consulted when there is no `X-Forwarded-For`, and
/// without a known peer nothing is (such a request has had its forwarding
/// headers stripped anyway).
pub fn client_ip(
    headers: &HeaderMap,
    peer: Option<IpAddr>,
    trusted: &TrustedProxies,
) -> Option<IpAddr> {
    let peer = peer?.to_canonical();
    if !trusted.trusts(peer) {
        return Some(peer);
    }
    // Several header lines are one list, in order.
    let chain: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter(|e| !e.trim().is_empty())
        .collect();
    if !chain.is_empty() {
        let mut client = peer;
        for entry in chain.iter().rev() {
            let Some(ip) = forwarded_addr(entry) else {
                break;
            };
            client = ip;
            if !trusted.trusts(ip) {
                break;
            }
        }
        return Some(client);
    }
    if let Some(ip) = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(forwarded_addr)
    {
        return Some(ip);
    }
    Some(peer)
}

/// Everything [`enforce`] needs: the general budget, the failed-auth budget,
/// and who may speak for a client.
#[derive(Debug, Clone)]
pub struct Throttle {
    /// Requests per client per window.
    pub requests: RateLimiter,
    /// Failed authentications per client per window, when that budget is on.
    pub auth_failures: Option<RateLimiter>,
    /// Peers whose `X-Forwarded-For` is believed.
    pub trusted: std::sync::Arc<TrustedProxies>,
}

/// Whether a request presents a credential of any kind.
///
/// Only those can be a guess. A `401` to a request that carried none is the
/// challenge a NuGet client waits for before it sends its key — a restore
/// against a read-gated feed produces one per package — and must not count.
fn carries_credential(headers: &HeaderMap) -> bool {
    headers.contains_key(header::AUTHORIZATION) || headers.contains_key(crate::auth::API_KEY_HEADER)
}

fn too_many(window_secs: u64) -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(header::RETRY_AFTER, window_secs.to_string())],
        Json(serde_json::json!({ "error": "rate limit exceeded" })),
    )
        .into_response()
}

/// Axum middleware enforcing a [`Throttle`]. Wire it with
/// `from_fn_with_state(throttle, enforce)`.
pub async fn enforce(State(throttle): State<Throttle>, req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip());
    let Some(ip) = client_ip(req.headers(), peer, &throttle.trusted) else {
        return next.run(req).await;
    };
    if !throttle.requests.check(ip) {
        return too_many(throttle.requests.window_secs());
    }
    let failures = throttle
        .auth_failures
        .as_ref()
        .filter(|_| carries_credential(req.headers()));
    if let Some(failures) = failures {
        // Out of guesses: refuse before the credential is even compared.
        if failures.is_exhausted(ip) {
            return too_many(failures.window_secs());
        }
    }
    let response = next.run(req).await;
    if let Some(failures) = failures {
        if response.status() == StatusCode::UNAUTHORIZED {
            failures.check(ip);
        }
    }
    response
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

    fn trusting(specs: &[&str]) -> TrustedProxies {
        TrustedProxies::new(specs.iter().copied())
    }

    fn xff(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append("x-forwarded-for", v.parse().unwrap());
        }
        h
    }

    fn addr(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_client_is_the_first_untrusted_hop_from_the_right() {
        let proxies = trusting(&["10.0.0.0/8"]);
        let peer = Some(addr("10.0.0.1"));
        // nginx appends the address it saw to whatever the client sent; the
        // client's own entry must not choose the bucket.
        let h = xff(&["6.6.6.6, 203.0.113.9"]);
        assert_eq!(client_ip(&h, peer, &proxies), Some(addr("203.0.113.9")));
        // Trusted hops in the chain are skipped.
        let h = xff(&["6.6.6.6, 203.0.113.9, 10.0.0.2"]);
        assert_eq!(client_ip(&h, peer, &proxies), Some(addr("203.0.113.9")));
        // Several header lines are one list.
        let h = xff(&["6.6.6.6", "203.0.113.9"]);
        assert_eq!(client_ip(&h, peer, &proxies), Some(addr("203.0.113.9")));
        // Garbage stops the walk at the last hop that parsed.
        let h = xff(&["203.0.113.9, junk, 10.0.0.2"]);
        assert_eq!(client_ip(&h, peer, &proxies), Some(addr("10.0.0.2")));
        // Ports and mapped addresses are normalised.
        let h = xff(&["[::ffff:198.51.100.7]:4711"]);
        assert_eq!(client_ip(&h, peer, &proxies), Some(addr("198.51.100.7")));
    }

    #[test]
    fn an_untrusted_peer_is_the_client_whatever_it_claims() {
        let proxies = trusting(&["10.0.0.0/8"]);
        let h = xff(&["203.0.113.9"]);
        assert_eq!(
            client_ip(&h, Some(addr("198.51.100.1")), &proxies),
            Some(addr("198.51.100.1"))
        );
        // No peer, no identity.
        assert_eq!(client_ip(&h, None, &proxies), None);
    }

    #[test]
    fn ipv6_clients_share_their_64() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60));
        let now = Instant::now();
        assert!(limiter.check_at(addr("2001:db8:1:2::1"), now));
        assert!(limiter.check_at(addr("2001:db8:1:2:ffff::9"), now));
        assert!(!limiter.check_at(addr("2001:db8:1:2::abcd"), now));
        // The next /64 over is someone else.
        assert!(limiter.check_at(addr("2001:db8:1:3::1"), now));
        // A mapped IPv4 peer is that IPv4 client.
        assert!(limiter.check_at(addr("192.0.2.1"), now));
        assert!(limiter.check_at(addr("::ffff:192.0.2.1"), now));
        assert!(!limiter.check_at(addr("192.0.2.1"), now));
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
