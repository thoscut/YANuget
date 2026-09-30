//! API-key authentication for write operations.
//!
//! NuGet clients send the key in the `X-NuGet-ApiKey` header. Comparison is
//! constant-time to avoid leaking the key through timing.

use std::sync::LazyLock;

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};

/// The header NuGet clients use to carry the push API key.
pub const API_KEY_HEADER: &str = "X-NuGet-ApiKey";

/// Authenticator holding the configured API keys. Several keys may be accepted
/// at once (e.g. one per team/developer); a presented key matching any of them
/// is valid.
#[derive(Clone, Default)]
pub struct ApiKeyAuth {
    expected: Vec<String>,
}

impl ApiKeyAuth {
    /// Build from the configured keys. Empty entries are ignored; an empty list
    /// means authentication is disabled and all writes are permitted.
    pub fn new(expected: impl IntoIterator<Item = String>) -> Self {
        Self {
            expected: expected.into_iter().filter(|k| !k.is_empty()).collect(),
        }
    }

    /// Whether a key is required at all.
    pub fn is_enabled(&self) -> bool {
        !self.expected.is_empty()
    }

    /// Check a presented key against the configured ones. Every configured key
    /// is compared (constant-time) so the work — and timing — does not depend on
    /// which key matches.
    pub fn check(&self, presented: Option<&str>) -> bool {
        if self.expected.is_empty() {
            return true; // auth disabled
        }
        let Some(p) = presented else {
            return false;
        };
        let mut ok = false;
        for expected in &self.expected {
            ok |= constant_time_eq(p.as_bytes(), expected.as_bytes());
        }
        ok
    }

    /// Convenience: validate the key carried in request headers.
    pub fn check_headers(&self, headers: &HeaderMap) -> bool {
        let presented = headers
            .get(API_KEY_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim);
        self.check(presented)
    }
}

/// Authenticator for read (download/restore) access to a feed. When no key is
/// configured, reads are open. Otherwise the credential may arrive either as an
/// `X-NuGet-ApiKey` header or as the password of HTTP Basic credentials (what
/// `dotnet`/`nuget` send to an authenticated feed).
#[derive(Clone, Default)]
pub struct ReadAuth {
    expected: Option<String>,
}

impl ReadAuth {
    /// Build from the configured read key. `None`/empty means reads are open.
    /// Surrounding whitespace is dropped, as it is for push keys.
    pub fn new(expected: Option<String>) -> Self {
        Self {
            expected: trimmed(expected),
        }
    }

    /// Whether a credential is required for reads.
    pub fn is_enabled(&self) -> bool {
        self.expected.is_some()
    }

    /// Validate the credentials carried in request headers.
    pub fn check_headers(&self, headers: &HeaderMap) -> bool {
        let Some(expected) = &self.expected else {
            return true; // reads are open
        };
        if let Some(key) = headers
            .get(API_KEY_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
        {
            if constant_time_eq(key.as_bytes(), expected.as_bytes()) {
                return true;
            }
        }
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(basic_password)
            .map(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()))
            .unwrap_or(false)
    }
}

/// Authenticator for the admin area, validated via HTTP Basic auth so a browser
/// can prompt for credentials. The username is ignored; the password must match
/// the configured admin key.
#[derive(Clone, Default)]
pub struct AdminAuth {
    expected: Option<String>,
}

impl AdminAuth {
    /// Build from the configured admin key. `None`/empty disables the admin area.
    /// Surrounding whitespace is dropped, as it is for push keys.
    pub fn new(expected: Option<String>) -> Self {
        Self {
            expected: trimmed(expected),
        }
    }

    /// Whether the admin area is configured at all.
    pub fn is_enabled(&self) -> bool {
        self.expected.is_some()
    }

    /// Validate the `Authorization: Basic …` header against the admin key.
    pub fn check_headers(&self, headers: &HeaderMap) -> bool {
        let Some(expected) = &self.expected else {
            return false;
        };
        let Some(password) = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(basic_password)
        else {
            return false;
        };
        constant_time_eq(password.as_bytes(), expected.as_bytes())
    }

    /// A fresh CSRF token for the admin forms of this feed.
    ///
    /// The admin area authenticates with HTTP Basic, which browsers replay
    /// automatically on *any* request to the origin — including a form POST
    /// submitted by a page on an attacker's site. Without a secret the attacker
    /// cannot know, a signed-in operator merely visiting a hostile page is
    /// enough to delete packages.
    ///
    /// The token is `{issued}.{mac}`: an HMAC-SHA256, under a secret drawn at
    /// random when the process starts, over this feed's admin key and the time
    /// it was issued. It is still stateless (no session store, no cookie), but
    /// unlike a hash of the admin key alone it cannot be computed by anyone
    /// who merely knows that key's derivation, it stops working after
    /// [`CSRF_TOKEN_TTL_SECS`], and a restart revokes every token handed out.
    pub fn csrf_token(&self) -> Option<String> {
        self.csrf_token_at(unix_now())
    }

    fn csrf_token_at(&self, issued: u64) -> Option<String> {
        use base64::Engine;
        let expected = self.expected.as_deref()?;
        let mac = csrf_mac(expected, issued);
        Some(format!(
            "{issued}.{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac)
        ))
    }

    /// Whether `presented` is a current CSRF token for this feed.
    pub fn check_csrf(&self, presented: Option<&str>) -> bool {
        self.check_csrf_at(presented, unix_now())
    }

    fn check_csrf_at(&self, presented: Option<&str>, now: u64) -> bool {
        use base64::Engine;
        let (Some(expected), Some(presented)) = (self.expected.as_deref(), presented) else {
            return false;
        };
        let Some((issued, mac)) = presented.trim().split_once('.') else {
            return false;
        };
        if issued.is_empty() || !issued.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
        let Ok(issued) = issued.parse::<u64>() else {
            return false;
        };
        // A little slack for a clock step; nothing from the far future.
        let fresh =
            issued <= now.saturating_add(60) && now.saturating_sub(issued) <= CSRF_TOKEN_TTL_SECS;
        let Ok(mac) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(mac) else {
            return false;
        };
        // Both sides are 32-byte digests, so the comparison below never
        // depends on a length the caller chose.
        let Ok(mac) = <[u8; 32]>::try_from(mac.as_slice()) else {
            return false;
        };
        let matches = digests_eq(&mac, &csrf_mac(expected, issued));
        fresh && matches
    }
}

/// How long an admin page's CSRF token stays valid: a working day, so a page
/// left open over lunch still submits, and one found in a browser cache or a
/// screenshot the next morning does not.
pub const CSRF_TOKEN_TTL_SECS: u64 = 12 * 60 * 60;

/// The per-process secret CSRF tokens are keyed with.
///
/// Drawn from the OS generator (through two v4 UUIDs, 244 random bits, which
/// avoids a dependency for one call). Held only in memory: a restart revokes
/// every token issued before it, which costs an operator one page reload.
static CSRF_SECRET: LazyLock<[u8; 32]> = LazyLock::new(|| {
    let mut secret = [0u8; 32];
    secret[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    secret[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    secret
});

fn csrf_mac(admin_key: &str, issued: u64) -> [u8; 32] {
    hmac_sha256(
        CSRF_SECRET.as_slice(),
        &[
            b"yanuget-admin-csrf-v2\0",
            admin_key.as_bytes(),
            b"\0",
            issued.to_string().as_bytes(),
        ],
    )
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// HMAC-SHA256 (RFC 2104) over the concatenation of `parts`.
///
/// Written out rather than pulled in: it is a dozen lines over the `sha2`
/// crate the server already uses, and checked against RFC 4231 below.
fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    for part in parts {
        inner.update(part);
    }
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

fn trimmed(key: Option<String>) -> Option<String> {
    key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty())
}

// Written out rather than derived: these hold the keys themselves, and a
// derived `Debug` would print them into any log line or panic that formats a
// feed (`FeedMeta` and `FeedContext` carry them).
impl std::fmt::Debug for ApiKeyAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyAuth")
            .field("keys", &self.expected.len())
            .finish()
    }
}

impl std::fmt::Debug for ReadAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadAuth")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

impl std::fmt::Debug for AdminAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminAuth")
            .field("enabled", &self.is_enabled())
            .finish()
    }
}

/// Extract the password from a `Basic base64(user:pass)` header value.
fn basic_password(value: &str) -> Option<String> {
    use base64::Engine;
    let b64 = value
        .strip_prefix("Basic ")
        .or_else(|| value.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    let creds = String::from_utf8(decoded).ok()?;
    // `user:pass` — the password is everything after the first colon.
    creds
        .split_once(':')
        .map(|(_, pass)| pass.trim().to_string())
}

/// Compare a presented secret with the expected one in time that depends on
/// neither's content nor length.
///
/// Both are hashed first and the fixed-size digests compared. A plain
/// byte-by-byte loop has to return early on a length mismatch, which tells a
/// caller probing with keys of different lengths how long the real one is.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    digests_eq(&Sha256::digest(a).into(), &Sha256::digest(b).into())
}

/// Compare two 32-byte digests without an early exit.
fn digests_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    // Keep the optimiser from turning the fold into a short-circuit.
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn disabled_auth_allows_everything() {
        let auth = ApiKeyAuth::new(Vec::new());
        assert!(!auth.is_enabled());
        assert!(auth.check(None));
        assert!(auth.check(Some("anything")));
    }

    #[test]
    fn enabled_auth_requires_exact_key() {
        let auth = ApiKeyAuth::new(["s3cret".to_string()]);
        assert!(auth.is_enabled());
        assert!(auth.check(Some("s3cret")));
        assert!(!auth.check(Some("wrong")));
        assert!(!auth.check(Some("s3cre")));
        assert!(!auth.check(None));
    }

    #[test]
    fn accepts_any_of_several_keys() {
        let auth = ApiKeyAuth::new(["alice".to_string(), "bob".to_string()]);
        assert!(auth.check(Some("alice")));
        assert!(auth.check(Some("bob")));
        assert!(!auth.check(Some("carol")));
        // Empty entries are ignored, never enabling a blank key.
        let mixed = ApiKeyAuth::new(["".to_string(), "real".to_string()]);
        assert!(mixed.is_enabled());
        assert!(!mixed.check(Some("")));
        assert!(mixed.check(Some("real")));
    }

    #[test]
    fn reads_header() {
        let auth = ApiKeyAuth::new(["key".to_string()]);
        let mut headers = HeaderMap::new();
        headers.insert(API_KEY_HEADER, HeaderValue::from_static("key"));
        assert!(auth.check_headers(&headers));

        let empty = HeaderMap::new();
        assert!(!auth.check_headers(&empty));
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2: a short key.
        assert_eq!(
            hex::encode(hmac_sha256(
                b"Jefe",
                &[b"what do ya want ", b"for nothing?"]
            )),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block is hashed first.
        assert_eq!(
            hex::encode(hmac_sha256(
                &[0xaa; 131],
                &[b"Test Using Larger Than Block-Size Key - Hash Key First"]
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn csrf_tokens_are_bound_to_the_key_and_expire() {
        let auth = AdminAuth::new(Some("s3cret".into()));
        let now = 1_800_000_000;
        let token = auth.csrf_token_at(now).unwrap();
        assert!(auth.check_csrf_at(Some(&token), now));
        assert!(auth.check_csrf_at(Some(&token), now + CSRF_TOKEN_TTL_SECS));
        // Expired, or from the future.
        assert!(!auth.check_csrf_at(Some(&token), now + CSRF_TOKEN_TTL_SECS + 1));
        assert!(!auth.check_csrf_at(Some(&token), now - 3600));
        // Another feed's admin key does not validate it.
        let other = AdminAuth::new(Some("other".into()));
        assert!(!other.check_csrf_at(Some(&token), now));
        // A token whose time was edited is rejected: the MAC covers it.
        let (_, mac) = token.split_once('.').unwrap();
        assert!(!auth.check_csrf_at(Some(&format!("{}.{mac}", now + 10)), now));
        // Garbage in every position.
        for bad in [
            "",
            ".",
            "x.y",
            "123",
            "123.",
            "+1.abc",
            &format!("{now}.AAAA"),
        ] {
            assert!(!auth.check_csrf_at(Some(bad), now), "{bad:?}");
        }
        assert!(!auth.check_csrf_at(None, now));
        // No admin key, no token.
        assert!(AdminAuth::new(None).csrf_token().is_none());
    }

    #[test]
    fn read_and_admin_keys_are_trimmed_like_push_keys() {
        use base64::Engine;
        let basic = |pass: &str| {
            let mut h = HeaderMap::new();
            let v = base64::engine::general_purpose::STANDARD.encode(format!("u:{pass}"));
            h.insert(
                axum::http::header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Basic {v}")).unwrap(),
            );
            h
        };
        let read = ReadAuth::new(Some(" reader\n".into()));
        assert!(read.check_headers(&basic("reader")));
        let admin = AdminAuth::new(Some("admin ".into()));
        assert!(admin.check_headers(&basic("admin")));
        assert!(admin.check_headers(&basic(" admin ")));
        // Whitespace alone is no key.
        assert!(!AdminAuth::new(Some("  ".into())).is_enabled());
    }

    #[test]
    fn debug_output_never_shows_a_key() {
        let dump = format!(
            "{:?} {:?} {:?}",
            ApiKeyAuth::new(["push-secret".to_string()]),
            ReadAuth::new(Some("read-secret".into())),
            AdminAuth::new(Some("admin-secret".into())),
        );
        assert!(!dump.contains("secret"), "{dump}");
    }

    #[test]
    fn keys_of_any_length_compare_without_error() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn admin_basic_auth() {
        use base64::Engine;
        let auth = AdminAuth::new(Some("s3cret".into()));
        assert!(auth.is_enabled());

        let mut headers = HeaderMap::new();
        // Any username, correct password.
        let good = base64::engine::general_purpose::STANDARD.encode("admin:s3cret");
        headers.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {good}")).unwrap(),
        );
        assert!(auth.check_headers(&headers));

        // Wrong password.
        let bad = base64::engine::general_purpose::STANDARD.encode("admin:nope");
        let mut h2 = HeaderMap::new();
        h2.insert(
            axum::http::header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {bad}")).unwrap(),
        );
        assert!(!auth.check_headers(&h2));

        // No header, and disabled instance.
        assert!(!auth.check_headers(&HeaderMap::new()));
        assert!(!AdminAuth::new(None).check_headers(&headers));
    }
}
