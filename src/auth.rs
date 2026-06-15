//! API-key authentication for write operations.
//!
//! NuGet clients send the key in the `X-NuGet-ApiKey` header. Comparison is
//! constant-time to avoid leaking the key through timing.

use axum::http::HeaderMap;

/// The header NuGet clients use to carry the push API key.
pub const API_KEY_HEADER: &str = "X-NuGet-ApiKey";

/// Authenticator holding the (optional) configured API key.
#[derive(Debug, Clone, Default)]
pub struct ApiKeyAuth {
    expected: Option<String>,
}

impl ApiKeyAuth {
    /// Build from the configured key. `None`/empty means authentication is
    /// disabled and all writes are permitted.
    pub fn new(expected: Option<String>) -> Self {
        Self {
            expected: expected.filter(|k| !k.is_empty()),
        }
    }

    /// Whether a key is required at all.
    pub fn is_enabled(&self) -> bool {
        self.expected.is_some()
    }

    /// Check a presented key against the configured one.
    pub fn check(&self, presented: Option<&str>) -> bool {
        match &self.expected {
            None => true, // auth disabled
            Some(expected) => presented
                .map(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()))
                .unwrap_or(false),
        }
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
        let auth = ApiKeyAuth::new(None);
        assert!(!auth.is_enabled());
        assert!(auth.check(None));
        assert!(auth.check(Some("anything")));
    }

    #[test]
    fn enabled_auth_requires_exact_key() {
        let auth = ApiKeyAuth::new(Some("s3cret".into()));
        assert!(auth.is_enabled());
        assert!(auth.check(Some("s3cret")));
        assert!(!auth.check(Some("wrong")));
        assert!(!auth.check(Some("s3cre")));
        assert!(!auth.check(None));
    }

    #[test]
    fn reads_header() {
        let auth = ApiKeyAuth::new(Some("key".into()));
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
