//! NuGet package versions.
//!
//! NuGet versions look like SemVer but are deliberately more lenient and have
//! their own normalization and ordering rules:
//!
//! * Up to **four** numeric components are allowed (`Major.Minor.Patch.Revision`).
//!   A missing component is treated as zero, and a trailing zero `Revision` is
//!   dropped when normalizing.
//! * A pre-release label (`-alpha.1`) and build metadata (`+sha.abc`) may follow.
//! * Build metadata is **ignored** for equality and ordering.
//! * Pre-release identifiers are compared the SemVer way, except that the
//!   comparison of alphanumeric identifiers is **case-insensitive** (ordinal),
//!   matching `NuGetVersion`.
//! * A version is considered *SemVer 2.0.0* when it has dotted pre-release
//!   labels or any build metadata; otherwise it is SemVer 1.0.0 compatible.
//!
//! This module implements exactly those rules so that registration/search
//! ordering matches the official client.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A parsed, comparable NuGet version.
#[derive(Debug, Clone)]
pub struct NuGetVersion {
    major: u64,
    minor: u64,
    patch: u64,
    revision: u64,
    /// Pre-release identifiers, already split on `.`. Empty for release versions.
    pre: Vec<String>,
    /// Build metadata (everything after `+`). Ignored for comparison.
    metadata: Option<String>,
    /// The original, un-normalized string the user supplied.
    original: String,
}

impl NuGetVersion {
    /// Parse a version string. Leading/trailing whitespace and an optional
    /// leading `v` are tolerated, matching NuGet client behaviour.
    pub fn parse(input: &str) -> Result<Self, VersionParseError> {
        let original = input.trim().to_string();
        let mut s = original.as_str();
        if let Some(stripped) = s.strip_prefix(['v', 'V']) {
            s = stripped;
        }
        if s.is_empty() {
            return Err(VersionParseError(original.clone()));
        }

        // Split off build metadata first (`+...`).
        let (s, metadata) = match s.split_once('+') {
            Some((v, m)) if !m.is_empty() => (v, Some(m.to_string())),
            Some((_, _)) => return Err(VersionParseError(original.clone())),
            None => (s, None),
        };

        // Then the pre-release label (`-...`).
        let (core, pre_str) = match s.split_once('-') {
            Some((v, p)) => (v, Some(p)),
            None => (s, None),
        };

        let mut parts = core.split('.');
        let major = parse_component(parts.next(), &original)?;
        let minor = parse_component_or_zero(parts.next(), &original)?;
        let patch = parse_component_or_zero(parts.next(), &original)?;
        let revision = parse_component_or_zero(parts.next(), &original)?;
        if parts.next().is_some() {
            // More than four components is not a valid NuGet version.
            return Err(VersionParseError(original.clone()));
        }

        let pre = match pre_str {
            None => Vec::new(),
            Some(p) => {
                if p.is_empty() {
                    return Err(VersionParseError(original.clone()));
                }
                let ids: Vec<String> = p.split('.').map(|x| x.to_string()).collect();
                for id in &ids {
                    if id.is_empty() || !id.bytes().all(is_pre_char) {
                        return Err(VersionParseError(original.clone()));
                    }
                }
                ids
            }
        };

        Ok(NuGetVersion {
            major,
            minor,
            patch,
            revision,
            pre,
            metadata,
            original,
        })
    }

    /// Whether this is a pre-release version.
    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }

    /// Whether this version requires `SemVerLevel=2.0.0` to be visible.
    ///
    /// A version is SemVer2 if it carries build metadata or has more than one
    /// dot-separated pre-release identifier.
    pub fn is_semver2(&self) -> bool {
        self.metadata.is_some() || self.pre.len() > 1
    }

    /// The original (un-normalized) string.
    pub fn original(&self) -> &str {
        &self.original
    }

    /// The normalized string used as the canonical identifier in URLs, storage
    /// paths and the database. Build metadata is dropped, a trailing zero
    /// revision is omitted, and the pre-release label is lower-cased.
    ///
    /// The lower-casing is what makes this string an *identity*. Two versions
    /// whose pre-release labels differ only in case are the same version — that
    /// is what [`compare_identifier`] implements and what NuGet clients assume —
    /// and every other place that identifies a version already agrees: storage
    /// paths are lower-cased, and so are the URLs clients request. Leaving the
    /// case here made the database the one component that disagreed, so
    /// `1.0.0-Beta` and `1.0.0-beta` became two rows sharing a single file: the
    /// second push was accepted despite `allow_overwrite = false`, it replaced
    /// the first one's bytes, and the first row went on advertising a hash that
    /// no longer matched what was served. Use [`Self::original`] for display.
    pub fn normalized(&self) -> String {
        let mut out = if self.revision > 0 {
            format!(
                "{}.{}.{}.{}",
                self.major, self.minor, self.patch, self.revision
            )
        } else {
            format!("{}.{}.{}", self.major, self.minor, self.patch)
        };
        if !self.pre.is_empty() {
            out.push('-');
            out.push_str(&self.pre.join(".").to_ascii_lowercase());
        }
        out
    }

    /// `(major, minor, patch, revision)` numeric core.
    pub fn core(&self) -> (u64, u64, u64, u64) {
        (self.major, self.minor, self.patch, self.revision)
    }

    fn cmp_core(&self, other: &Self) -> Ordering {
        self.core().cmp(&other.core())
    }

    fn cmp_pre(&self, other: &Self) -> Ordering {
        match (self.pre.is_empty(), other.pre.is_empty()) {
            (true, true) => Ordering::Equal,
            // A release version is greater than any pre-release of the same core.
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => compare_pre_identifiers(&self.pre, &other.pre),
        }
    }
}

fn is_pre_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

fn parse_component(part: Option<&str>, original: &str) -> Result<u64, VersionParseError> {
    let part = part.ok_or_else(|| VersionParseError(original.to_string()))?;
    part.parse::<u64>()
        .map_err(|_| VersionParseError(original.to_string()))
}

fn parse_component_or_zero(part: Option<&str>, original: &str) -> Result<u64, VersionParseError> {
    match part {
        None => Ok(0),
        Some(p) => p
            .parse::<u64>()
            .map_err(|_| VersionParseError(original.to_string())),
    }
}

/// Compare two pre-release identifier lists per SemVer, but case-insensitively
/// for alphanumeric identifiers (NuGet semantics).
fn compare_pre_identifiers(a: &[String], b: &[String]) -> Ordering {
    for (x, y) in a.iter().zip(b.iter()) {
        let ord = compare_identifier(x, y);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.len().cmp(&b.len())
}

fn compare_identifier(a: &str, b: &str) -> Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        // Both numeric: compare numerically.
        (Ok(na), Ok(nb)) => na.cmp(&nb),
        // Numeric identifiers always have lower precedence than alphanumeric.
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        // Both alphanumeric: case-insensitive ordinal comparison.
        (Err(_), Err(_)) => a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()),
    }
}

impl PartialEq for NuGetVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for NuGetVersion {}

impl PartialOrd for NuGetVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NuGetVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.cmp_core(other).then_with(|| self.cmp_pre(other))
    }
}

impl std::hash::Hash for NuGetVersion {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Must agree with `Eq`: hash the normalized lowercase form.
        self.core().hash(state);
        for id in &self.pre {
            id.to_ascii_lowercase().hash(state);
        }
    }
}

impl fmt::Display for NuGetVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.normalized())
    }
}

impl FromStr for NuGetVersion {
    type Err = VersionParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        NuGetVersion::parse(s)
    }
}

impl Serialize for NuGetVersion {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.normalized())
    }
}

impl<'de> Deserialize<'de> for NuGetVersion {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        NuGetVersion::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Error returned when a version string cannot be parsed.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid NuGet version: {0}")]
pub struct VersionParseError(pub String);

#[cfg(test)]
mod tests {
    #[test]
    fn a_pre_release_label_normalizes_to_one_canonical_case() {
        // `normalized()` is the database key, the storage path and the URL
        // segment. Two versions that compare equal must produce the same one,
        // or they become two rows sharing a single file — with the first row
        // still advertising a hash the served bytes no longer match.
        let upper = NuGetVersion::parse("1.0.0-Beta").unwrap();
        let lower = NuGetVersion::parse("1.0.0-beta").unwrap();
        assert_eq!(
            upper, lower,
            "NuGet compares pre-release labels case-insensitively"
        );
        assert_eq!(upper.normalized(), lower.normalized());
        assert_eq!(upper.normalized(), "1.0.0-beta");

        // Multi-identifier labels and build metadata take the same path.
        let mixed = NuGetVersion::parse("2.1.0-RC.2+SHA.abcDEF").unwrap();
        assert_eq!(mixed.normalized(), "2.1.0-rc.2");

        // The string the publisher wrote is still available for display.
        assert_eq!(upper.original(), "1.0.0-Beta");

        // Nothing about the numeric core changes.
        assert_eq!(
            NuGetVersion::parse("1.2.3.0").unwrap().normalized(),
            "1.2.3"
        );
        assert_eq!(
            NuGetVersion::parse("1.2.3.4").unwrap().normalized(),
            "1.2.3.4"
        );
    }

    use super::*;

    fn v(s: &str) -> NuGetVersion {
        NuGetVersion::parse(s).expect("valid version")
    }

    #[test]
    fn parses_basic_versions() {
        assert_eq!(v("1.2.3").core(), (1, 2, 3, 0));
        assert_eq!(v("1.2.3.4").core(), (1, 2, 3, 4));
        assert_eq!(v("1.0").core(), (1, 0, 0, 0));
        assert_eq!(v("2").core(), (2, 0, 0, 0));
    }

    #[test]
    fn tolerates_leading_v_and_whitespace() {
        assert_eq!(v("  v1.2.3 ").core(), (1, 2, 3, 0));
    }

    #[test]
    fn normalizes_trailing_zero_revision() {
        assert_eq!(v("1.2.3.0").normalized(), "1.2.3");
        assert_eq!(v("1.2.3.4").normalized(), "1.2.3.4");
        assert_eq!(v("1.0").normalized(), "1.0.0");
        assert_eq!(v("1.02.3").normalized(), "1.2.3"); // leading zeros dropped
    }

    #[test]
    fn normalized_keeps_prerelease_drops_metadata() {
        assert_eq!(v("1.2.3-alpha.1+build.5").normalized(), "1.2.3-alpha.1");
        assert!(v("1.2.3-alpha.1+build.5").is_prerelease());
    }

    #[test]
    fn release_greater_than_prerelease() {
        assert!(v("1.0.0") > v("1.0.0-alpha"));
        assert!(v("1.0.0-beta") > v("1.0.0-alpha"));
    }

    #[test]
    fn prerelease_numeric_vs_alpha_ordering() {
        // Numeric identifiers have lower precedence than alphanumeric.
        assert!(v("1.0.0-1") < v("1.0.0-alpha"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0-alpha.beta"));
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-2") < v("1.0.0-11")); // numeric, not lexical
    }

    #[test]
    fn comparison_is_case_insensitive_but_metadata_ignored() {
        assert_eq!(v("1.0.0-Alpha"), v("1.0.0-alpha"));
        assert_eq!(v("1.0.0+a"), v("1.0.0+b"));
        assert_eq!(v("1.2.3.0"), v("1.2.3"));
    }

    #[test]
    fn semver2_detection() {
        assert!(!v("1.0.0").is_semver2());
        assert!(!v("1.0.0-alpha").is_semver2());
        assert!(v("1.0.0-alpha.1").is_semver2());
        assert!(v("1.0.0+meta").is_semver2());
    }

    #[test]
    fn rejects_garbage() {
        assert!(NuGetVersion::parse("").is_err());
        assert!(NuGetVersion::parse("abc").is_err());
        assert!(NuGetVersion::parse("1.2.3.4.5").is_err());
        assert!(NuGetVersion::parse("1.2.3-").is_err());
        assert!(NuGetVersion::parse("1.2.3-bad_label").is_err());
        assert!(NuGetVersion::parse("1.-1.0").is_err());
    }

    #[test]
    fn sorting_is_correct() {
        let mut versions = [
            v("1.0.0"),
            v("1.0.0-alpha"),
            v("1.0.0-alpha.1"),
            v("1.0.0-beta"),
            v("0.9.0"),
            v("1.0.1"),
        ];
        versions.sort();
        let normalized: Vec<String> = versions.iter().map(|x| x.normalized()).collect();
        assert_eq!(
            normalized,
            vec![
                "0.9.0",
                "1.0.0-alpha",
                "1.0.0-alpha.1",
                "1.0.0-beta",
                "1.0.0",
                "1.0.1",
            ]
        );
    }
}
