//! API-key authentication for write operations.
//!
//! NuGet clients send the key in the `X-NuGet-ApiKey` header. Comparison is
//! constant-time to avoid leaking the key through timing.

use axum::http::HeaderMap;

/// The header NuGet clients use to carry the push API key.
pub const API_KEY_HEADER: &str = "X-NuGet-ApiKey";

/// Authenticator holding the configured API keys. Several keys may be accepted
/// at once (e.g. one per team/developer); a presented key matching any of them
/// is valid.
#[derive(Debug, Clone, Default)]
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
#[derive(Debug, Clone, Default)]
pub struct ReadAuth {
    expected: Option<String>,
}

impl ReadAuth {
    /// Build from the configured read key. `None`/empty means reads are open.
    pub fn new(expected: Option<String>) -> Self {
        Self {
            expected: expected.filter(|k| !k.is_empty()),
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
#[derive(Debug, Clone, Default)]
pub struct AdminAuth {
    expected: Option<String>,
}

impl AdminAuth {
    /// Build from the configured admin key. `None`/empty disables the admin area.
    pub fn new(expected: Option<String>) -> Self {
        Self {
            expected: expected.filter(|k| !k.is_empty()),
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

    /// The CSRF token embedded in every admin form and required by every admin
    /// state-changing POST.
    ///
    /// The admin area authenticates with HTTP Basic, which browsers replay
    /// automatically on *any* request to the origin — including a form POST
    /// submitted by a page on an attacker's site. Without a secret the attacker
    /// cannot know, a signed-in operator merely visiting a hostile page is
    /// enough to delete packages. The token is derived from the admin key, so
    /// producing it requires already knowing that key; it never appears outside
    /// same-origin admin pages, and it is stateless (no session store, no
    /// cookie, nothing to expire).
    pub fn csrf_token(&self) -> Option<String> {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let expected = self.expected.as_deref()?;
        let mut hasher = Sha256::new();
        hasher.update(b"yanuget-admin-csrf-v1\0");
        hasher.update(expected.as_bytes());
        Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hasher.finalize()))
    }

    /// Whether `presented` is this feed's CSRF token (constant-time).
    pub fn check_csrf(&self, presented: Option<&str>) -> bool {
        let Some(expected) = self.csrf_token() else {
            return false;
        };
        presented.is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()))
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
    creds.split_once(':').map(|(_, pass)| pass.to_string())
}

/// Length-independent, content constant-time byte comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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
