//! Small formatting helpers shared by the pages: counts, sizes, truncation
//! and the key/value rows of the settings and retention pages.

use super::escape::escape_html;

/// `""` or `"s"`, so counts read as "1 package" rather than "1 package(s)".
pub(super) fn plural(n: i64) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Group a non-negative integer into thousands with `,` separators.
pub(super) fn group_digits(n: i64) -> String {
    let s = n.max(0).to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// A key/value row with an escaped text value.
pub(super) fn kv(label: &str, value: &str) -> String {
    kv_html(label, &escape_html(value))
}

/// A key/value row whose value is already trusted HTML.
pub(super) fn kv_html(label: &str, value_html: &str) -> String {
    format!("<div><b>{}</b>{}</div>", escape_html(label), value_html)
}

pub(super) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(max).collect();
        t.push('\u{2026}');
        t
    }
}

pub(super) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_digits_inserts_separators() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(42), "42");
        assert_eq!(group_digits(1234), "1,234");
        assert_eq!(group_digits(1234567), "1,234,567");
    }

    #[test]
    fn human_size_scales() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(25 * 1024 * 1024 * 1024), "25.0 GB");
    }
}
