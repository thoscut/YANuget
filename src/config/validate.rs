//! Checks on a loaded configuration and on feed names and prefixes.

use crate::error::{Error, Result};

use super::{Config, FeedConfig};

impl Config {
    /// Reject combinations that parse but cannot mean what they say.
    ///
    /// Runs after both layers, so a TOML value and an environment value are
    /// held to the same rules.
    pub fn validate(&self) -> Result<()> {
        if self.rate_limit.enabled && self.rate_limit.window_secs == 0 {
            // A zero-length window resets on every request, so no client ever
            // reaches the limit: the throttle is on in name only.
            return Err(Error::BadRequest(
                "rate_limit.window_secs must be at least 1 (set rate_limit.enabled = false \
                 to turn the limiter off)"
                    .into(),
            ));
        }
        if self.tls_enabled && self.tls_cert_path.is_some() != self.tls_key_path.is_some() {
            // Half a pair used to fall back to the self-signed certificate,
            // silently serving something other than what was configured.
            return Err(Error::BadRequest(
                "tls_cert_path and tls_key_path must be set together".into(),
            ));
        }
        if self.monitoring.token.is_some() && self.monitoring.token().is_none() {
            // What a template writes for an empty variable: as for
            // `YANUGET_MONITORING_TOKEN`, a blank token would quietly serve
            // /metrics to anyone instead of requiring one.
            return Err(Error::BadRequest(
                "monitoring.token is set but blank; give it a token, or remove it to \
                 serve /metrics and /health/status without one"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// A feed's `reserved_id_prefixes`, checked: made of package-id characters
/// and starting with a letter, digit or `_`, so each can match real ids.
pub(super) fn reserved_prefixes(feed: &FeedConfig) -> Result<Vec<String>> {
    let mut prefixes = Vec::with_capacity(feed.reserved_id_prefixes.len());
    for raw in &feed.reserved_id_prefixes {
        let prefix = raw.trim();
        let valid = prefix
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_')
            && prefix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if !valid || prefix.len() > crate::validation::MAX_ID_LENGTH {
            return Err(Error::BadRequest(format!(
                "feed {:?}: invalid reserved_id_prefixes entry {raw:?}",
                feed.name
            )));
        }
        prefixes.push(prefix.to_string());
    }
    Ok(prefixes)
}

/// Route path segments a feed may not shadow. A feed is mounted at `/{name}`,
/// so a feed called `health` or `v3` would collide with (or mask) a real route.
/// `_assets` is where the gallery's font is served, at the root.
const RESERVED_FEED_NAMES: [&str; 13] = [
    "health", "admin", "docs", "packages", "stats", "settings", "v3", "api", "download", "metrics",
    "_assets", "tags", "files",
];

/// Validate a feed name: non-empty, made only of URL-path-safe characters (so it
/// can be a path segment and a storage/database key), not a relative-path token,
/// and not the name of an existing route.
pub(super) fn validate_feed_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::BadRequest(format!(
            "invalid feed name {name:?}: use only letters, digits, '-', '_' or '.'"
        )));
    }
    // `.` and `..` pass the character check but are path traversal tokens, and
    // the name becomes a URL prefix (`/..`).
    if name.chars().all(|c| c == '.') {
        return Err(Error::BadRequest(format!(
            "invalid feed name {name:?}: a name of only dots is a relative path"
        )));
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED_FEED_NAMES.contains(&lower.as_str()) {
        return Err(Error::BadRequest(format!(
            "feed name {name:?} is reserved: it would shadow the /{lower} route"
        )));
    }
    Ok(())
}
