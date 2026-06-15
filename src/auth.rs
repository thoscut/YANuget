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
}
