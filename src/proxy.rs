//! Trust model for reverse-proxy forwarding headers.
//!
//! `X-Forwarded-Host`, `X-Forwarded-Proto`, `X-Forwarded-For` and `X-Real-IP`
//! are *request* headers: any client can send them. Honouring them
//! unconditionally is a real hazard, because two things downstream depend on
//! them:
//!
//! * [`crate::web::AppState::url_builder`] derives the externally visible base
//!   URL — and therefore every absolute `packageContent`/registration URL the
//!   NuGet client is told to fetch — from the forwarded host and scheme. A
//!   caller who can steer those (directly, or by poisoning a shared HTTP cache
//!   in front of the server) can point restoring clients at a host they choose.
//! * [`crate::ratelimit`] derives the client IP from them, so a spoofed
//!   `X-Forwarded-For` defeats the per-IP throttle entirely.
//!
//! Both are closed by only trusting these headers when the *connection peer* is
//! a proxy the operator vouched for. Everything else has its forwarding headers
//! stripped before any handler or middleware sees them.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// The forwarding headers that are only honoured from a trusted peer.
pub const FORWARDED_HEADERS: [&str; 5] = [
    "x-forwarded-for",
    "x-forwarded-host",
    "x-forwarded-proto",
    "x-real-ip",
    "forwarded",
];

/// Loopback, link-local and RFC1918/ULA ranges — where reverse proxies
/// (sidecars, ingress controllers, a local nginx) actually live. This is the
/// default trust set, expanded from the `private` keyword.
const PRIVATE_RANGES: [&str; 8] = [
    "127.0.0.0/8",
    "::1/128",
    "10.0.0.0/8",
    "172.16.0.0/12",
    "192.168.0.0/16",
    "169.254.0.0/16",
    "fc00::/7",
    "fe80::/10",
];

/// A single CIDR block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cidr {
    addr: IpAddr,
    bits: u8,
}

impl Cidr {
    /// Parse `10.0.0.0/8`, `::1/128` or a bare address (an implicit `/32`
    /// or `/128`).
    fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim();
        let (addr_s, bits_s) = match spec.split_once('/') {
            Some((a, b)) => (a, Some(b)),
            None => (spec, None),
        };
        let addr: IpAddr = addr_s.parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let bits = match bits_s {
            None => max,
            Some(b) => {
                let n: u8 = b.trim().parse().ok()?;
                if n > max {
                    return None;
                }
                n
            }
        };
        Some(Self { addr, bits })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => prefix_eq(&net.octets(), &ip.octets(), self.bits),
            (IpAddr::V6(net), IpAddr::V6(ip)) => prefix_eq(&net.octets(), &ip.octets(), self.bits),
            _ => false,
        }
    }
}

/// Compare the first `bits` bits of two equal-length byte arrays.
fn prefix_eq(a: &[u8], b: &[u8], bits: u8) -> bool {
    let full = (bits / 8) as usize;
    if a[..full] != b[..full] {
        return false;
    }
    let rem = bits % 8;
    if rem == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rem);
    (a[full] & mask) == (b[full] & mask)
}

/// The set of peers whose forwarding headers are honoured.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    /// Every peer is trusted (the explicit `*` opt-out).
    any: bool,
    nets: Vec<Cidr>,
}

impl TrustedProxies {
    /// Build from the configured specs. Each entry is `*`/`any` (trust every
    /// peer), `private` (the loopback/link-local/RFC1918/ULA ranges), a bare IP
    /// address, or a CIDR block. Unparseable entries are logged and ignored
    /// rather than failing startup — an unusable entry must never silently widen
    /// trust, and it must not take the server down either.
    pub fn new<'a>(specs: impl IntoIterator<Item = &'a str>) -> Self {
        let mut out = Self::default();
        for spec in specs {
            let spec = spec.trim();
            if spec.is_empty() {
                continue;
            }
            match spec.to_ascii_lowercase().as_str() {
                "*" | "any" | "all" => out.any = true,
                "private" | "local" => {
                    out.nets
                        .extend(PRIVATE_RANGES.iter().filter_map(|r| Cidr::parse(r)));
                }
                _ => match Cidr::parse(spec) {
                    Some(c) => out.nets.push(c),
                    None => {
                        tracing::warn!(entry = %spec, "ignoring unparseable trusted_proxies entry")
                    }
                },
            }
        }
        out
    }

    /// Whether *no* peer is trusted (forwarding headers are always stripped).
    pub fn is_empty(&self) -> bool {
        !self.any && self.nets.is_empty()
    }

    /// Whether `ip` may set forwarding headers. IPv4-mapped IPv6 peers (what a
    /// dual-stack listener reports for an IPv4 connection) are matched as IPv4.
    pub fn trusts(&self, ip: IpAddr) -> bool {
        if self.any {
            return true;
        }
        let ip = unmap(ip);
        self.nets.iter().any(|n| n.contains(ip))
    }
}

/// Collapse an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) to its IPv4 form.
fn unmap(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        },
        v4 => v4,
    }
}

/// Whether `host` (a URL host, possibly with a port) names a loopback,
/// link-local or private-range address. Used to keep an upstream feed from
/// steering the mirror at the local network or a cloud metadata service.
pub fn is_private_host(host: &str) -> bool {
    let host = host.trim();
    // Strip an IPv6 literal's brackets and any `:port` suffix.
    let bare = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        host.rsplit_once(':')
            .map(|(h, _)| h)
            .filter(|h| !h.contains(':'))
            .unwrap_or(host)
    };
    let lower = bare.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") || lower.ends_with(".local") {
        return true;
    }
    match lower.parse::<IpAddr>() {
        Ok(ip) => is_private_ip(unmap(ip)),
        // A DNS name we cannot classify without resolving; treat as public.
        Err(_) => false,
    }
}

/// Whether an already-resolved address is one the mirror must not fetch from.
///
/// [`is_private_host`] can only classify a host it can read as an address; a
/// DNS name has to be resolved first, and this is what the resolved addresses
/// are then checked against.
pub fn is_private_ip_addr(ip: IpAddr) -> bool {
    is_private_ip(unmap(ip))
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // 100.64.0.0/10 (CGNAT) and 192.0.0.0/24 (IETF protocol assignments).
                || Cidr::parse("100.64.0.0/10").is_some_and(|c| c.contains(IpAddr::V4(v4)))
                || Cidr::parse("192.0.0.0/24").is_some_and(|c| c.contains(IpAddr::V4(v4)))
                || v4 == Ipv4Addr::new(169, 254, 169, 254)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || Cidr::parse("fc00::/7").is_some_and(|c| c.contains(IpAddr::V6(v6)))
                || Cidr::parse("fe80::/10").is_some_and(|c| c.contains(IpAddr::V6(v6)))
                || v6 == Ipv6Addr::UNSPECIFIED
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn default_is_trust_nothing() {
        let t = TrustedProxies::default();
        assert!(t.is_empty());
        assert!(!t.trusts(ip("127.0.0.1")));
        assert!(!t.trusts(ip("10.1.2.3")));
    }

    #[test]
    fn private_keyword_covers_the_usual_proxy_ranges() {
        let t = TrustedProxies::new(["private"]);
        for good in ["127.0.0.1", "10.1.2.3", "172.16.0.9", "192.168.1.1", "::1"] {
            assert!(t.trusts(ip(good)), "{good} should be trusted");
        }
        for bad in ["8.8.8.8", "203.0.113.7", "2001:4860:4860::8888"] {
            assert!(!t.trusts(ip(bad)), "{bad} should not be trusted");
        }
    }

    #[test]
    fn explicit_cidrs_and_addresses() {
        let t = TrustedProxies::new(["203.0.113.0/24", "2001:db8::1"]);
        assert!(t.trusts(ip("203.0.113.7")));
        assert!(!t.trusts(ip("203.0.114.7")));
        assert!(t.trusts(ip("2001:db8::1")));
        assert!(!t.trusts(ip("2001:db8::2")));
    }

    #[test]
    fn wildcard_trusts_everyone() {
        let t = TrustedProxies::new(["*"]);
        assert!(!t.is_empty());
        assert!(t.trusts(ip("8.8.8.8")));
    }

    #[test]
    fn ipv4_mapped_peers_match_ipv4_rules() {
        let t = TrustedProxies::new(["127.0.0.0/8"]);
        assert!(t.trusts(ip("::ffff:127.0.0.1")));
        assert!(!t.trusts(ip("::ffff:8.8.8.8")));
    }

    #[test]
    fn unparseable_entries_are_ignored_not_widened() {
        let t = TrustedProxies::new(["not-an-ip", "10.0.0.0/99", ""]);
        assert!(t.is_empty());
        assert!(!t.trusts(ip("10.0.0.1")));
    }

    #[test]
    fn private_hosts_are_recognised() {
        for private in [
            "localhost",
            "LOCALHOST:8080",
            "127.0.0.1",
            "127.0.0.1:5000",
            "10.0.0.5",
            "169.254.169.254",
            "[::1]:443",
            "[fd00::1]",
            "box.local",
        ] {
            assert!(is_private_host(private), "{private} should be private");
        }
        for public in [
            "api.nuget.org",
            "example.com:443",
            "8.8.8.8",
            "[2001:db8::1]",
        ] {
            assert!(!is_private_host(public), "{public} should be public");
        }
    }
}
