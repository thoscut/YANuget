//! NuGet version ranges, as a `.nuspec` dependency writes them.
//!
//! The notation is NuGet's interval notation:
//!
//! | Range         | Means                 |
//! | ------------- | --------------------- |
//! | `1.0`         | `1.0 ≤ x`             |
//! | `[1.0]`       | `x = 1.0`             |
//! | `(1.0,)`      | `1.0 < x`             |
//! | `[1.0,)`      | `1.0 ≤ x`             |
//! | `(,1.0]`      | `x ≤ 1.0`             |
//! | `(,1.0)`      | `x < 1.0`             |
//! | `[1.0,2.0]`   | `1.0 ≤ x ≤ 2.0`       |
//! | `(1.0,2.0)`   | `1.0 < x < 2.0`       |
//! | `[1.0,2.0)`   | `1.0 ≤ x < 2.0`       |
//!
//! An empty string, or a missing range, is any version. Floating versions
//! (`1.*`) belong to project files, not to package manifests, and are refused,
//! as are ranges that contain no version at all (`(1.0)`, `[2.0,1.0]`,
//! `(1.0,1.0)`).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::version::NuGetVersion;

/// The longest range string accepted: two versions and the punctuation
/// around them. Bounds the work done on untrusted input.
const MAX_RANGE_CHARS: usize = 2 * 256 + 8;

/// One end of a range.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Bound {
    pub version: NuGetVersion,
    pub inclusive: bool,
}

/// A parsed NuGet version range. Either end may be open.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct VersionRange {
    pub min: Option<Bound>,
    pub max: Option<Bound>,
}

/// Error returned when a range string cannot be parsed.
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid NuGet version range {0:?}: {1}")]
pub struct RangeParseError(pub String, pub &'static str);

impl VersionRange {
    /// Every version.
    pub fn any() -> Self {
        Self::default()
    }

    /// Exactly `version` (`[1.2.3]`).
    pub fn exact(version: NuGetVersion) -> Self {
        let bound = Bound {
            version,
            inclusive: true,
        };
        Self {
            min: Some(bound.clone()),
            max: Some(bound),
        }
    }

    /// `version` or anything above it (`1.2.3`, `[1.2.3, )`).
    pub fn at_least(version: NuGetVersion) -> Self {
        Self {
            min: Some(Bound {
                version,
                inclusive: true,
            }),
            max: None,
        }
    }

    /// Parse NuGet's interval notation (see the module docs).
    pub fn parse(input: &str) -> Result<Self, RangeParseError> {
        let s = input.trim();
        let err = |why| RangeParseError(input.to_string(), why);
        if s.len() > MAX_RANGE_CHARS {
            return Err(err("too long"));
        }
        if s.is_empty() {
            return Ok(Self::any());
        }
        let version = |v: &str| {
            NuGetVersion::parse(v.trim()).map_err(|_| err("not a valid version inside it"))
        };

        let (open, close) = (s.as_bytes()[0], s.as_bytes()[s.len() - 1]);
        if !matches!(open, b'[' | b'(') {
            // A bare version is a minimum, inclusive.
            if s.contains(['*', ',', '[', ']', '(', ')']) {
                return Err(err("floating versions and stray brackets are not ranges"));
            }
            return Ok(Self::at_least(version(s)?));
        }
        if s.len() < 2 || !matches!(close, b']' | b')') {
            return Err(err("an opening bracket needs a closing one"));
        }
        let (min_inclusive, max_inclusive) = (open == b'[', close == b']');
        let inner = &s[1..s.len() - 1];
        if inner.contains(['[', ']', '(', ')', '*']) {
            return Err(err("brackets or a floating version inside the range"));
        }

        let range = match inner.split_once(',') {
            None => {
                // `[1.0]` is exact; `(1.0)`, `[1.0)` and `(1.0]` hold nothing.
                if !(min_inclusive && max_inclusive) {
                    return Err(err("a single version must be written [x]"));
                }
                Self::exact(version(inner)?)
            }
            Some((lo, hi)) => {
                if hi.contains(',') {
                    return Err(err("more than two versions"));
                }
                let (lo, hi) = (lo.trim(), hi.trim());
                if lo.is_empty() && hi.is_empty() {
                    return Err(err("no version on either side"));
                }
                let min = if lo.is_empty() {
                    None
                } else {
                    Some(Bound {
                        version: version(lo)?,
                        inclusive: min_inclusive,
                    })
                };
                let max = if hi.is_empty() {
                    None
                } else {
                    Some(Bound {
                        version: version(hi)?,
                        inclusive: max_inclusive,
                    })
                };
                // An open end cannot be inclusive: `[,1.0]` and `[1.0,]` are
                // NuGet errors.
                if (min.is_none() && min_inclusive) || (max.is_none() && max_inclusive) {
                    return Err(err("an open end must use a parenthesis"));
                }
                Self { min, max }
            }
        };
        if range.is_empty() {
            return Err(err("no version satisfies it"));
        }
        Ok(range)
    }

    /// Whether no version can satisfy the range (`max < min`, or equal
    /// bounds with either end exclusive). [`Self::parse`] refuses these.
    pub fn is_empty(&self) -> bool {
        match (&self.min, &self.max) {
            (Some(lo), Some(hi)) => match lo.version.cmp(&hi.version) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => !(lo.inclusive && hi.inclusive),
                std::cmp::Ordering::Less => false,
            },
            _ => false,
        }
    }

    /// Whether `version` is inside the range.
    pub fn contains(&self, version: &NuGetVersion) -> bool {
        let above_min = match &self.min {
            None => true,
            Some(b) if b.inclusive => version >= &b.version,
            Some(b) => version > &b.version,
        };
        let below_max = match &self.max {
            None => true,
            Some(b) if b.inclusive => version <= &b.version,
            Some(b) => version < &b.version,
        };
        above_min && below_max
    }

    /// Whether either bound is itself a pre-release, which lets pre-release
    /// candidates in under [`Self::best_match`].
    pub fn has_prerelease_bound(&self) -> bool {
        [&self.min, &self.max]
            .into_iter()
            .flatten()
            .any(|b| b.version.is_prerelease())
    }

    /// The version a dependency on this range resolves to among `available`:
    /// the lowest one inside the range when it has a lower bound (NuGet's
    /// "lowest applicable version"), the highest one otherwise.
    ///
    /// Pre-release versions are candidates only when `allow_prerelease` is
    /// set or a bound of the range is itself a pre-release.
    pub fn best_match<'a, I>(
        &self,
        available: I,
        allow_prerelease: bool,
    ) -> Option<&'a NuGetVersion>
    where
        I: IntoIterator<Item = &'a NuGetVersion>,
    {
        let prerelease = allow_prerelease || self.has_prerelease_bound();
        let candidates = available
            .into_iter()
            .filter(|v| self.contains(v) && (prerelease || !v.is_prerelease()));
        if self.min.is_some() {
            candidates.min()
        } else {
            candidates.max()
        }
    }

    /// NuGet's normalized form: `[1.0.0, 2.0.0)`, `[1.0.0, )`, `(, 2.0.0]`,
    /// `[1.0.0]`, and the empty string for any version. Versions are
    /// normalized ([`NuGetVersion::normalized`]).
    pub fn normalized(&self) -> String {
        match (&self.min, &self.max) {
            (None, None) => String::new(),
            (Some(lo), Some(hi)) if lo.inclusive && hi.inclusive && lo.version == hi.version => {
                format!("[{}]", lo.version.normalized())
            }
            (lo, hi) => format!(
                "{}{}, {}{}",
                if lo.as_ref().is_some_and(|b| b.inclusive) {
                    '['
                } else {
                    '('
                },
                lo.as_ref()
                    .map(|b| b.version.normalized())
                    .unwrap_or_default(),
                hi.as_ref()
                    .map(|b| b.version.normalized())
                    .unwrap_or_default(),
                if hi.as_ref().is_some_and(|b| b.inclusive) {
                    ']'
                } else {
                    ')'
                },
            ),
        }
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.normalized())
    }
}

impl FromStr for VersionRange {
    type Err = RangeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for VersionRange {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.normalized())
    }
}

impl<'de> Deserialize<'de> for VersionRange {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn v(s: &str) -> NuGetVersion {
        NuGetVersion::parse(s).unwrap()
    }

    fn r(s: &str) -> VersionRange {
        VersionRange::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn the_documented_notation_parses_and_normalizes() {
        for (input, normalized) in [
            ("", ""),
            ("  ", ""),
            ("1.0", "[1.0.0, )"),
            ("[1.0]", "[1.0.0]"),
            ("(1.0,)", "(1.0.0, )"),
            ("[1.0,)", "[1.0.0, )"),
            ("(,1.0]", "(, 1.0.0]"),
            ("(,1.0)", "(, 1.0.0)"),
            ("[1.0,2.0]", "[1.0.0, 2.0.0]"),
            ("(1.0,2.0)", "(1.0.0, 2.0.0)"),
            ("[1.0,2.0)", "[1.0.0, 2.0.0)"),
            (" [ 1.0 , 2.0 ) ", "[1.0.0, 2.0.0)"),
            ("[1.0.0-beta.1, 2.0.0)", "[1.0.0-beta.1, 2.0.0)"),
            ("[1.0,1.0]", "[1.0.0]"),
            ("[1.2.3.4]", "[1.2.3.4]"),
        ] {
            assert_eq!(r(input).normalized(), normalized, "{input:?}");
        }
    }

    #[test]
    fn what_is_not_a_range_is_refused() {
        for bad in [
            "1.*",
            "[1.*, 2.0)",
            "(1.0)",
            "[1.0)",
            "(1.0]",
            "[2.0,1.0]",
            "(1.0,1.0)",
            "[1.0,1.0)",
            "[,1.0]",
            "[1.0,]",
            "(,)",
            "[,]",
            "[1.0,2.0,3.0]",
            "[1.0",
            "1.0]",
            "[]",
            "[",
            "(",
            "not-a-version",
            "[a,b]",
            "1.0,2.0",
            "[[1.0]]",
        ] {
            assert!(VersionRange::parse(bad).is_err(), "{bad:?} accepted");
        }
        assert!(VersionRange::parse(&"9".repeat(MAX_RANGE_CHARS + 1)).is_err());
    }

    #[test]
    fn contains_honours_each_end() {
        let range = r("(1.0,2.0]");
        assert!(!range.contains(&v("1.0")));
        assert!(range.contains(&v("1.0.1")));
        assert!(range.contains(&v("2.0")));
        assert!(!range.contains(&v("2.0.1")));
        // A pre-release of 2.0.0 sorts below it.
        assert!(range.contains(&v("2.0.0-rc.1")));
        assert!(r("").contains(&v("0.0.1-alpha")));
        assert!(r("[1.2.3]").contains(&v("1.2.3+build")));
    }

    #[test]
    fn best_match_is_the_lowest_applicable_version() {
        let available: Vec<NuGetVersion> = ["0.9", "1.0", "1.1-beta", "1.1", "2.0", "3.0-rc"]
            .iter()
            .map(|s| v(s))
            .collect();
        let best = |range: &str, pre: bool| {
            r(range)
                .best_match(&available, pre)
                .map(NuGetVersion::normalized)
        };
        assert_eq!(best("1.0", false).as_deref(), Some("1.0.0"));
        assert_eq!(best("(1.0,)", false).as_deref(), Some("1.1.0"));
        assert_eq!(best("(1.0,)", true).as_deref(), Some("1.1.0-beta"));
        assert_eq!(best("[1.1.0-alpha,)", false).as_deref(), Some("1.1.0-beta"));
        assert_eq!(best("(,2.0)", false).as_deref(), Some("1.1.0"));
        assert_eq!(best("", false).as_deref(), Some("2.0.0"));
        assert_eq!(best("", true).as_deref(), Some("3.0.0-rc"));
        assert_eq!(best("[4.0,)", true), None);
    }

    #[test]
    fn serde_round_trips_through_the_normalized_form() {
        let range = r("[1.0,2.0)");
        let json = serde_json::to_string(&range).unwrap();
        assert_eq!(json, "\"[1.0.0, 2.0.0)\"");
        let back: VersionRange = serde_json::from_str(&json).unwrap();
        assert_eq!(back, range);
        assert!(serde_json::from_str::<VersionRange>("\"(1.0)\"").is_err());
    }

    fn version() -> impl Strategy<Value = NuGetVersion> {
        (0u64..5, 0u64..5, 0u64..5, prop::option::of(0u64..3)).prop_map(
            |(major, minor, patch, pre)| {
                let s = match pre {
                    Some(n) => format!("{major}.{minor}.{patch}-rc.{n}"),
                    None => format!("{major}.{minor}.{patch}"),
                };
                NuGetVersion::parse(&s).unwrap()
            },
        )
    }

    fn bound() -> impl Strategy<Value = Option<Bound>> {
        prop::option::of(
            (version(), any::<bool>())
                .prop_map(|(version, inclusive)| Bound { version, inclusive }),
        )
    }

    fn range() -> impl Strategy<Value = VersionRange> {
        (bound(), bound())
            .prop_map(|(min, max)| VersionRange { min, max })
            .prop_filter("a parseable range holds a version", |r| !r.is_empty())
    }

    proptest! {
        /// Normalizing and parsing again gives the same range.
        #[test]
        fn normalized_form_round_trips(range in range()) {
            let text = range.normalized();
            let back = VersionRange::parse(&text).unwrap();
            prop_assert_eq!(&back, &range, "{}", text);
            prop_assert_eq!(back.normalized(), text);
        }

        /// `contains` agrees with the bounds taken one at a time.
        #[test]
        fn contains_matches_the_bounds(range in range(), x in version()) {
            let above = range.min.as_ref().is_none_or(|b| if b.inclusive { x >= b.version } else { x > b.version });
            let below = range.max.as_ref().is_none_or(|b| if b.inclusive { x <= b.version } else { x < b.version });
            prop_assert_eq!(range.contains(&x), above && below);
        }

        /// An exact range holds its version and nothing else.
        #[test]
        fn an_exact_range_holds_one_version(a in version(), b in version()) {
            let exact = VersionRange::parse(&format!("[{}]", a.normalized())).unwrap();
            prop_assert!(exact.contains(&a));
            prop_assert_eq!(exact.contains(&b), a == b);
        }

        /// The best match is inside the range, and no allowed candidate is
        /// a better (lower, or with no lower bound higher) choice.
        #[test]
        fn best_match_is_inside_and_optimal(
            range in range(),
            available in prop::collection::vec(version(), 0..12),
            pre in any::<bool>(),
        ) {
            let allowed = |x: &NuGetVersion| range.contains(x) && (pre || range.has_prerelease_bound() || !x.is_prerelease());
            match range.best_match(&available, pre) {
                None => prop_assert!(!available.iter().any(allowed)),
                Some(best) => {
                    prop_assert!(allowed(best));
                    for x in available.iter().filter(|x| allowed(x)) {
                        if range.min.is_some() {
                            prop_assert!(best <= x);
                        } else {
                            prop_assert!(best >= x);
                        }
                    }
                }
            }
        }

        /// Arbitrary input never panics, and whatever parses re-parses from
        /// its normalized form.
        #[test]
        fn any_input_parses_or_is_refused(input in "[\\[\\]() ,.0-9a-z*+-]{0,24}") {
            if let Ok(range) = VersionRange::parse(&input) {
                prop_assert_eq!(VersionRange::parse(&range.normalized()).unwrap(), range);
            }
        }
    }
}
