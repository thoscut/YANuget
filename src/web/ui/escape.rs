//! Escaping: the only way package data may reach the HTML.
//!
//! Every page in [`super`] is assembled with `format!`, so nothing escapes a
//! value unless the code says so. The rule:
//!
//! * Text and attribute values go through [`escape_html`], once, at the
//!   point they are put into the markup — never a string that already
//!   holds markup.
//! * A URL from package metadata placed in `href` or `src` goes through
//!   [`safe_href`] first (which drops `javascript:`, `data:` and the like),
//!   and then through [`escape_html`] like any other attribute value.
//! * A package id, version, tag or search placed in a path or query of one
//!   of this server's own URLs goes through [`enc_path`]. Its output is
//!   plain ASCII with nothing HTML-significant in it; the URL it ends up in
//!   is still escaped as a whole where it holds anything else.
//!
//! Keep these helpers here, and add any new one here, so a reviewer checking a
//! sink has one place to look. The page CSP (see `layout`) is the backstop
//! for a missed one, not a substitute.

/// Escape the five HTML-significant characters.
pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Return `url` only if it carries a safe, expected scheme (`http`, `https` or
/// `mailto`). Package metadata (project/repository/license/icon URLs) is
/// attacker-controlled, so an unfiltered value like `javascript:alert(1)` or a
/// `data:` URI placed into an `href`/`src` attribute would be a stored-XSS hole
/// that HTML-escaping alone does not close (the scheme contains no escapable
/// characters). The scheme is compared case-insensitively with ASCII whitespace
/// and control characters stripped, because browsers ignore those when
/// resolving it. The caller must still HTML-escape the returned value.
pub fn safe_href(url: &str) -> Option<&str> {
    let trimmed = url.trim();
    let scheme: String = trimmed
        .split(':')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_ascii_whitespace() && !c.is_ascii_control())
        .flat_map(char::to_lowercase)
        .collect();
    match scheme.as_str() {
        "http" | "https" | "mailto" => Some(trimmed),
        _ => None,
    }
}

/// Percent-encode a path segment for use in our own `/packages/...` URLs.
pub(super) fn enc_path(segment: &str) -> String {
    use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
    const KEEP: &percent_encoding::AsciiSet = &NON_ALPHANUMERIC
        .remove(b'.')
        .remove(b'-')
        .remove(b'_')
        .remove(b'~');
    utf8_percent_encode(segment, KEEP).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_dangerous_characters() {
        assert_eq!(
            escape_html("<script>\"&'"),
            "&lt;script&gt;&quot;&amp;&#39;"
        );
    }

    #[test]
    fn safe_href_allows_only_expected_schemes() {
        assert_eq!(
            safe_href("https://example.com/x"),
            Some("https://example.com/x")
        );
        assert_eq!(
            safe_href("  http://example.com  "),
            Some("http://example.com")
        );
        assert_eq!(
            safe_href("HTTPS://Example.com"),
            Some("HTTPS://Example.com")
        );
        assert_eq!(
            safe_href("mailto:dev@example.com"),
            Some("mailto:dev@example.com")
        );
        assert_eq!(safe_href("javascript:alert(1)"), None);
        // Browsers strip control characters before resolving the scheme; so do we.
        assert_eq!(safe_href("java\tscript:alert(1)"), None);
        assert_eq!(safe_href("data:text/html,<script>alert(1)</script>"), None);
        assert_eq!(safe_href("//evil.example.com"), None);
        assert_eq!(safe_href("not a url"), None);
    }
}
