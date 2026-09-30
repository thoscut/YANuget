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
    /// Parse a version string. Surrounding whitespace is tolerated; anything
    /// NuGet's own `NuGetVersion.Parse` refuses is refused here too.
    ///
    /// This is the identity of every stored version, so it errs strict: a
    /// string this server accepts but the NuGet client does not is a version
    /// nobody can restore, and one that normalizes differently here than there
    /// is two names for one package. In particular:
    ///
    /// * A leading `v` is rejected. It used to be stripped, which NuGet never
    ///   does, so `v1.0.0` in a manifest was published as `1.0.0` while the
    ///   client refuses that manifest outright. No client sends it in a URL
    ///   either — the protocol always uses the normalized form — so there is no
    ///   leniency worth keeping for the router.
    /// * Numeric components are plain ASCII digits no larger than `i32::MAX`,
    ///   which is what NuGet's `System.Version` core holds.
    /// * A numeric pre-release identifier may not have a leading zero (SemVer
    ///   §9, and NuGet's `IsValidPart`). `1.0.0-01` compared equal to
    ///   `1.0.0-1` but normalized differently, which allowed two rows — and two
    ///   files — for what the client treats as one version.
    /// * Build metadata follows the pre-release character rules
    ///   (`[0-9A-Za-z-]`, dot-separated, no empty identifier), though leading
    ///   zeros are allowed there, as in SemVer. It is persisted and displayed,
    ///   so it must not carry spaces or arbitrary Unicode.
    /// * The full normalized string, metadata included, is at most
    ///   [`MAX_VERSION_CHARS`] characters, as on nuget.org. Without a cap a long
    ///   label surfaced late, as an `ENAMETOOLONG` from the store.
    pub fn parse(input: &str) -> Result<Self, VersionParseError> {
        let original = input.trim().to_string();
        let err = || VersionParseError(original.clone());
        // A cheap bound before any work. Only the leading zeros the core drops
        // can make the input longer than its normalized form.
        if original.is_empty() || original.len() > MAX_INPUT_CHARS {
            return Err(err());
        }
        let s = original.as_str();

        // Split off build metadata first (`+...`).
        let (s, metadata) = match s.split_once('+') {
            Some((v, m)) => {
                if !valid_identifiers(m, true) {
                    return Err(err());
                }
                (v, Some(m.to_string()))
            }
            None => (s, None),
        };

        // Then the pre-release label (`-...`).
        let (core, pre_str) = match s.split_once('-') {
            Some((v, p)) => (v, Some(p)),
            None => (s, None),
        };

        let mut parts = core.split('.');
        let major = parse_component(parts.next()).ok_or_else(err)?;
        let mut rest = [0u64; 3];
        for slot in &mut rest {
            if let Some(part) = parts.next() {
                *slot = parse_component(Some(part)).ok_or_else(err)?;
            }
        }
        let [minor, patch, revision] = rest;
        if parts.next().is_some() {
            // More than four components is not a valid NuGet version.
            return Err(err());
        }

        let pre = match pre_str {
            None => Vec::new(),
            Some(p) => {
                if !valid_identifiers(p, false) {
                    return Err(err());
                }
                p.split('.').map(str::to_string).collect()
            }
        };

        let version = NuGetVersion {
            major,
            minor,
            patch,
            revision,
            pre,
            metadata,
            original: original.clone(),
        };
        if version.to_full_string().len() > MAX_VERSION_CHARS {
            return Err(err());
        }
        Ok(version)
    }

    /// Parse a version this server stored, under the rules that were in force
    /// when it was stored: what [`Self::parse`] accepted before it matched
    /// NuGet's strictness (a leading `v`, leading-zero pre-release
    /// identifiers, any non-empty build metadata, `u64` components, no length
    /// cap).
    ///
    /// Only for reading back what the database or storage already holds. A
    /// row written by an older release must stay readable: one odd version
    /// failing to parse would fail every listing, registration and restore of
    /// its whole package. Normalization is the same function as for new input,
    /// so a stored version keeps the key it was stored under. Anything new —
    /// a manifest, an upstream's version list — goes through [`Self::parse`].
    pub fn parse_stored(input: &str) -> Result<Self, VersionParseError> {
        if let Ok(version) = Self::parse(input) {
            return Ok(version);
        }
        let original = input.trim().to_string();
        let err = || VersionParseError(original.clone());
        let s = original
            .strip_prefix(['v', 'V'])
            .unwrap_or(original.as_str());
        if s.is_empty() {
            return Err(err());
        }
        let (s, metadata) = match s.split_once('+') {
            Some((_, "")) => return Err(err()),
            Some((v, m)) => (v, Some(m.to_string())),
            None => (s, None),
        };
        let (core, pre_str) = match s.split_once('-') {
            Some((v, p)) => (v, Some(p)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let major = parts
            .next()
            .and_then(|p| p.parse::<u64>().ok())
            .ok_or_else(err)?;
        let mut rest = [0u64; 3];
        for slot in &mut rest {
            if let Some(part) = parts.next() {
                *slot = part.parse::<u64>().map_err(|_| err())?;
            }
        }
        let [minor, patch, revision] = rest;
        if parts.next().is_some() {
            return Err(err());
        }
        let pre = match pre_str {
            None => Vec::new(),
            Some(p) => {
                let ids: Vec<String> = p.split('.').map(str::to_string).collect();
                if ids
                    .iter()
                    .any(|id| id.is_empty() || !id.bytes().all(is_pre_char))
                {
                    return Err(err());
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
        let mut out = self.core_string();
        if !self.pre.is_empty() {
            out.push('-');
            out.push_str(&self.pre.join(".").to_ascii_lowercase());
        }
        out
    }

    /// The version as a client should display it: the normalized numeric core,
    /// the pre-release label in the publisher's casing, and the build metadata
    /// (NuGet's `ToFullString`). This is what nuget.org reports in
    /// registration and search; [`Self::normalized`] stays the identity used in
    /// URLs, paths and the database.
    pub fn to_full_string(&self) -> String {
        let mut out = self.core_string();
        if !self.pre.is_empty() {
            out.push('-');
            out.push_str(&self.pre.join("."));
        }
        if let Some(metadata) = &self.metadata {
            out.push('+');
            out.push_str(metadata);
        }
        out
    }

    fn core_string(&self) -> String {
        if self.revision > 0 {
            format!(
                "{}.{}.{}.{}",
                self.major, self.minor, self.patch, self.revision
            )
        } else {
            format!("{}.{}.{}", self.major, self.minor, self.patch)
        }
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

/// The longest version accepted, counted on the normalized form with its build
/// metadata ([`NuGetVersion::to_full_string`]). nuget.org enforces the same
/// limit.
pub const MAX_VERSION_CHARS: usize = 64;

/// The longest raw input looked at. Generous next to [`MAX_VERSION_CHARS`] —
/// only leading zeros in the core and surrounding whitespace can make up the
/// difference — but it bounds the work spent on an absurd string.
const MAX_INPUT_CHARS: usize = 256;

fn is_pre_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-'
}

/// Validate a dot-separated pre-release label or build-metadata string: every
/// identifier non-empty and made of `[0-9A-Za-z-]`. A numeric identifier may
/// have a leading zero only where `allow_leading_zeros` says so (metadata).
fn valid_identifiers(s: &str, allow_leading_zeros: bool) -> bool {
    s.split('.').all(|id| {
        let numeric = id.bytes().all(|b| b.is_ascii_digit());
        !id.is_empty()
            && id.bytes().all(is_pre_char)
            && (allow_leading_zeros || !numeric || id == "0" || !id.starts_with('0'))
    })
}

/// Parse one numeric core component: ASCII digits only (Rust's own `parse`
/// also takes a leading `+`), at most `i32::MAX`.
fn parse_component(part: Option<&str>) -> Option<u64> {
    let part = part?;
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Leading zeros are legal in the core (`1.02.3` is `1.2.3`, as in
    // `System.Version`), so they must not count toward an overflow.
    let digits = part.trim_start_matches('0');
    if digits.is_empty() {
        return Some(0);
    }
    let value: u64 = digits.parse().ok()?;
    (value <= i32::MAX as u64).then_some(value)
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
        // Must agree with `Eq`, which is `compare_identifier` per identifier:
        // numeric identifiers by value, the rest case-insensitively. `parse`
        // already refuses the leading zeros that made `01` and `1` equal but
        // distinct strings; hashing the value keeps the contract structural
        // rather than dependent on that.
        self.core().hash(state);
        self.pre.len().hash(state);
        for id in &self.pre {
            match id.parse::<u64>() {
                Ok(n) => (0u8, n).hash(state),
                Err(_) => (1u8, id.to_ascii_lowercase()).hash(state),
            }
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
    fn tolerates_whitespace_but_not_a_leading_v() {
        assert_eq!(v("  1.2.3 ").core(), (1, 2, 3, 0));
        // NuGet refuses `v1.2.3`; stripping it here published a manifest the
        // client itself would not read.
        assert!(NuGetVersion::parse("v1.2.3").is_err());
        assert!(NuGetVersion::parse("V1.2.3").is_err());
    }

    /// `1.0.0-01` and `1.0.0-1` compared equal but normalized — and hashed —
    /// differently, so they could become two rows for one NuGet version.
    #[test]
    fn numeric_prerelease_identifiers_may_not_have_leading_zeros() {
        assert!(NuGetVersion::parse("1.0.0-01").is_err());
        assert!(NuGetVersion::parse("1.0.0-alpha.007").is_err());
        // A lone zero, and zeros inside alphanumeric identifiers, are fine.
        assert_eq!(v("1.0.0-0").normalized(), "1.0.0-0");
        assert_eq!(v("1.0.0-0a.rc-01").normalized(), "1.0.0-0a.rc-01");
        // Metadata may have leading zeros, as in SemVer.
        assert_eq!(v("1.0.0+001").to_full_string(), "1.0.0+001");
    }

    #[test]
    fn numeric_components_are_digits_within_int32() {
        assert_eq!(v("2147483647.0.0").core().0, 2_147_483_647);
        assert!(NuGetVersion::parse("2147483648.0.0").is_err());
        assert!(NuGetVersion::parse("1.99999999999999999999.0").is_err());
        // Rust's integer parser takes a sign; NuGet does not.
        assert!(NuGetVersion::parse("1.+2.3").is_err());
        // Leading zeros in the core are still dropped, not counted as size.
        assert_eq!(v("0000000001.0.0").normalized(), "1.0.0");
    }

    #[test]
    fn build_metadata_is_validated_like_a_label() {
        assert!(NuGetVersion::parse("1.0.0+").is_err());
        assert!(NuGetVersion::parse("1.0.0+a..b").is_err());
        assert!(NuGetVersion::parse("1.0.0+has space").is_err());
        assert!(NuGetVersion::parse("1.0.0+café").is_err());
        assert!(NuGetVersion::parse("1.0.0+a+b").is_err());
        assert_eq!(v("1.0.0+sha.ABC-1").to_full_string(), "1.0.0+sha.ABC-1");
    }

    #[test]
    fn the_full_string_is_capped() {
        let label = "a".repeat(MAX_VERSION_CHARS - "1.0.0-".len());
        assert!(NuGetVersion::parse(&format!("1.0.0-{label}")).is_ok());
        assert!(NuGetVersion::parse(&format!("1.0.0-{label}a")).is_err());
        // Metadata counts too.
        assert!(NuGetVersion::parse(&format!("1.0.0-{label}+m")).is_err());
        assert!(NuGetVersion::parse(&"1".repeat(10_000)).is_err());
    }

    /// Versions stored before the strict rules read back as they were stored,
    /// under the same normalized key.
    #[test]
    fn stored_versions_parse_under_the_old_rules() {
        let legacy = NuGetVersion::parse_stored(" v1.0.0-01+has space ").unwrap();
        assert_eq!(legacy.normalized(), "1.0.0-01");
        assert_eq!(legacy.original(), "v1.0.0-01+has space");
        let long = format!("1.0.0-{}", "x".repeat(200));
        assert_eq!(
            NuGetVersion::parse_stored(&long).unwrap().normalized(),
            long
        );
        assert_eq!(
            NuGetVersion::parse_stored("99999999999.0")
                .unwrap()
                .core()
                .0,
            99_999_999_999
        );
        // What parses strictly parses the same way.
        assert_eq!(
            NuGetVersion::parse_stored("1.0.0-Beta+m")
                .unwrap()
                .to_full_string(),
            "1.0.0-Beta+m"
        );
        // The old parser's refusals still stand.
        for bad in ["", "v", "1.2.3.4.5", "1.0.0-", "1.0.0+", "1.0.0-a_b", "x"] {
            assert!(NuGetVersion::parse_stored(bad).is_err(), "{bad}");
        }
        // Eq and Hash still agree across the two spellings of one number.
        use std::collections::HashSet;
        let set: HashSet<NuGetVersion> = ["1.0.0-01", "1.0.0-1"]
            .into_iter()
            .map(|s| NuGetVersion::parse_stored(s).unwrap())
            .collect();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn hash_agrees_with_eq() {
        use std::collections::HashSet;
        let set: HashSet<NuGetVersion> = ["1.0.0-Beta.1", "1.0.0-beta.1+meta", "1.0.0.0-BETA.1"]
            .into_iter()
            .map(v)
            .collect();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn full_string_keeps_case_and_metadata() {
        let x = v("1.02.3.0-Beta.2+Build.7");
        assert_eq!(x.to_full_string(), "1.2.3-Beta.2+Build.7");
        assert_eq!(x.normalized(), "1.2.3-beta.2");
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
