//! Which hosts and addresses an upstream fetch must not reach.
//!
//! A package feed, or a script inside a package, names the URLs that get
//! fetched. Pointed at the local network or a cloud metadata service, such a
//! fetch would publish whatever that host answers, so the server's mirror and
//! the internalizer's downloader both refuse loopback, private, link-local
//! and reserved addresses unless the operator allowed them. A host given as a
//! name has to be resolved first; with the `net` feature, [`GuardedResolver`]
//! does that and filters the answers.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

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

/// Collapse an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`) to its IPv4 form.
pub fn unmap(ip: IpAddr) -> IpAddr {
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
    // A fully qualified name may end in a dot (`localhost.`); it names the same
    // host, so it must not slip past the name checks below.
    let lower = bare.to_ascii_lowercase();
    let lower = lower.trim_end_matches('.');
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

/// IPv4 ranges the mirror never fetches from, beyond what `Ipv4Addr`'s own
/// predicates cover: "this network" (`0.0.0.0/8`, which Linux routes to the
/// local host), CGNAT, IETF protocol assignments, the benchmarking range, and
/// multicast plus the reserved `240.0.0.0/4` (which includes broadcast).
const PRIVATE_V4: [([u8; 4], u8); 6] = [
    ([0, 0, 0, 0], 8),
    ([100, 64, 0, 0], 10),
    ([192, 0, 0, 0], 24),
    ([198, 18, 0, 0], 15),
    ([224, 0, 0, 0], 4),
    ([240, 0, 0, 0], 4),
];

/// IPv6 ranges that are private in themselves: unique-local, link-local, the
/// deprecated site-local, multicast, and the local-use NAT64 prefix
/// (`64:ff9b:1::/48`, RFC 8215), whose embedding is chosen by the local
/// network and so cannot be decoded — anything under it is some local
/// translator's idea of an IPv4 address.
const PRIVATE_V6: [([u16; 3], u8); 5] = [
    ([0xfc00, 0, 0], 7),
    ([0xfe80, 0, 0], 10),
    ([0xfec0, 0, 0], 10),
    ([0xff00, 0, 0], 8),
    ([0x0064, 0xff9b, 0x0001], 48),
];

fn in_v4(ip: Ipv4Addr, (net, bits): ([u8; 4], u8)) -> bool {
    prefix_eq(&net, &ip.octets(), bits)
}

fn in_v6(ip: Ipv6Addr, (net, bits): ([u16; 3], u8)) -> bool {
    let net = Ipv6Addr::new(net[0], net[1], net[2], 0, 0, 0, 0, 0);
    prefix_eq(&net.octets(), &ip.octets(), bits)
}

/// The IPv4 address an IPv6 address stands for, when it is one of the forms
/// that carry one: IPv4-mapped (`::ffff:a.b.c.d`), IPv4-compatible
/// (`::a.b.c.d`), NAT64's well-known prefix (`64:ff9b::/96`) and 6to4
/// (`2002::/16`). A connection to any of these can end up at the embedded
/// address, so it is what has to be classified: `[64:ff9b::a9fe:a9fe]` is the
/// metadata service behind a NAT64 gateway.
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let o = v6.octets();
    let s = v6.segments();
    let tail = Ipv4Addr::new(o[12], o[13], o[14], o[15]);
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    // `::` and `::1` are the unspecified and loopback addresses, not
    // IPv4-compatible ones; they are classified as IPv6.
    if s[..6] == [0; 6] && (s[6] != 0 || s[7] > 1) {
        return Some(tail);
    }
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return Some(tail);
    }
    if s[0] == 0x2002 {
        return Some(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    None
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || PRIVATE_V4.iter().any(|&net| in_v4(v4, net))
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = embedded_v4(v6) {
                return is_private_ip(IpAddr::V4(v4));
            }
            v6.is_loopback() || v6.is_unspecified() || PRIVATE_V6.iter().any(|&net| in_v6(v6, net))
        }
    }
}

/// Resolves upstream host names, dropping every private, loopback or
/// link-local address unless the operator allowed a private upstream.
///
/// Checking a URL before requesting it cannot stop a *name* that resolves to
/// such an address (`metadata.evil.example` with an `A` record of
/// `169.254.169.254`, or `localhost.` with its trailing dot), and a check that
/// resolves separately from the connection is undone by a DNS answer that
/// changes in between. The resolver is where the connection's own addresses
/// come from, so filtering here covers every URL the mirror fetches, every
/// redirect hop and every later reconnect at once.
///
/// Literal IP hosts never reach a resolver; classify those with
/// [`is_private_host`] before the request and on every redirect.
#[cfg(feature = "net")]
#[derive(Debug, Default)]
pub struct GuardedResolver {
    /// A configured outbound proxy's host, resolved without the filter: the
    /// operator named it, and it is routinely on the private network.
    pub proxy_host: Option<String>,
    /// Names answered without a DNS lookup, and filtered like any other.
    /// Empty outside tests, which need a name that resolves to loopback.
    pub fixed: Vec<(String, std::net::IpAddr)>,
    /// How the operator can allow such an address, appended to the error.
    pub hint: Option<String>,
}

#[cfg(feature = "net")]
impl reqwest::dns::Resolve for GuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        let exempt = self
            .proxy_host
            .as_deref()
            .is_some_and(|p| p.eq_ignore_ascii_case(&host));
        let fixed = self
            .fixed
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&host))
            .map(|(_, ip)| std::net::SocketAddr::new(*ip, 0));
        let hint = self.hint.clone();
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> = match fixed {
                Some(addr) => vec![addr],
                None => tokio::net::lookup_host((host.as_str(), 0)).await?.collect(),
            };
            if exempt {
                return Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs);
            }
            let public: Vec<std::net::SocketAddr> = addrs
                .iter()
                .copied()
                .filter(|a| !is_private_ip_addr(a.ip()))
                .collect();
            if public.is_empty() {
                if let Some(first) = addrs.first() {
                    let mut why = format!(
                        "upstream host {host} resolves to the private address {}",
                        first.ip()
                    );
                    if let Some(hint) = hint {
                        why = format!("{why}; {hint}");
                    }
                    return Err(why.into());
                }
            }
            Ok(Box::new(public.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
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

    #[test]
    fn a_trailing_dot_does_not_hide_localhost() {
        for name in [
            "localhost.",
            "LOCALHOST.:8080",
            "api.localhost.",
            "box.local.",
        ] {
            assert!(is_private_host(name), "{name} should be private");
        }
        assert!(!is_private_host("api.nuget.org."));
    }

    #[test]
    fn reserved_ipv4_ranges_are_private() {
        for private in [
            "0.0.0.0",
            "0.1.2.3",         // "this network", routed to the local host
            "100.64.0.1",      // CGNAT
            "100.127.255.254", // ...to the end of the /10
            "192.0.0.8",       // IETF protocol assignments
            "198.18.0.1",      // benchmarking
            "198.19.255.254",
            "224.0.0.1", // multicast
            "239.255.255.250",
            "240.0.0.1", // reserved
            "255.255.255.255",
            "169.254.169.254",
        ] {
            assert!(
                is_private_ip_addr(ip(private)),
                "{private} should be private"
            );
        }
        for public in ["1.1.1.1", "100.128.0.1", "198.20.0.1", "223.255.255.254"] {
            assert!(!is_private_ip_addr(ip(public)), "{public} should be public");
        }
    }

    #[test]
    fn ipv6_forms_that_embed_a_private_ipv4_address_are_private() {
        for private in [
            "::ffff:127.0.0.1",        // IPv4-mapped
            "::127.0.0.1",             // IPv4-compatible
            "::a9fe:a9fe",             // ...the metadata service
            "64:ff9b::7f00:1",         // NAT64 well-known prefix
            "64:ff9b::a9fe:a9fe",      // ...to 169.254.169.254
            "64:ff9b:1::1",            // local-use NAT64: never decodable
            "64:ff9b:1:ffff::8.8.8.8", // ...even around a public address
            "2002:7f00:1::",           // 6to4 of 127.0.0.1
            "2002:0a00:0001::1",       // 6to4 of 10.0.0.1
            "ff02::1",                 // multicast
            "fec0::1",                 // deprecated site-local
            "::",
            "::1",
        ] {
            assert!(
                is_private_ip_addr(ip(private)),
                "{private} should be private"
            );
            assert!(
                is_private_host(&format!("[{private}]")),
                "[{private}] should be private"
            );
        }
        for public in [
            "64:ff9b::808:808",  // NAT64 of 8.8.8.8
            "2002:0808:0808::1", // 6to4 of 8.8.8.8
            "::8.8.8.8",         // IPv4-compatible 8.8.8.8
            "2606:4700:4700::1111",
        ] {
            assert!(!is_private_ip_addr(ip(public)), "{public} should be public");
        }
    }
}
