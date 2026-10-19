//! Manifest data a `.nuspec` declares, independent of how it is stored.

use serde::{Deserialize, Serialize};

use crate::range::VersionRange;

/// A NuGet package type declaration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageType {
    pub name: String,
    pub version: Option<String>,
}

/// A group of dependencies that applies to a single target framework
/// (or to all frameworks when [`DependencyGroup::target_framework`] is `None`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DependencyGroup {
    pub target_framework: Option<String>,
    pub dependencies: Vec<Dependency>,
}

/// A single package dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub id: String,
    /// The version range string (e.g. `[1.0.0, 2.0.0)`), if specified. Parse
    /// it with [`crate::range::VersionRange::parse`].
    pub version_range: Option<String>,
    /// Comma-separated `include` assets.
    pub include: Option<String>,
    /// Comma-separated `exclude` assets.
    pub exclude: Option<String>,
}

impl Dependency {
    /// Whether either end of [`Self::version_range`] is a SemVer 2.0.0
    /// version (a dotted pre-release label or build metadata). A range that
    /// does not parse counts as not SemVer 2.0.0.
    pub fn is_semver2(&self) -> bool {
        let Some(range) = self.version_range.as_deref() else {
            return false;
        };
        let Ok(range) = VersionRange::parse(range) else {
            return false;
        };
        [range.min, range.max]
            .iter()
            .flatten()
            .any(|bound| bound.version.is_semver2())
    }
}

/// Whether any dependency in `groups` has a SemVer 2.0.0 range.
///
/// NuGet treats such a package as SemVer 2.0.0 even when its own version is
/// not: a SemVer 1.0.0 client cannot resolve the dependency, so the version is
/// hidden from requests without `semVerLevel=2.0.0`, as nuget.org does.
pub fn depends_on_semver2(groups: &[DependencyGroup]) -> bool {
    groups
        .iter()
        .flat_map(|g| &g.dependencies)
        .any(Dependency::is_semver2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(ranges: &[Option<&str>]) -> Vec<DependencyGroup> {
        vec![DependencyGroup {
            target_framework: None,
            dependencies: ranges
                .iter()
                .map(|r| Dependency {
                    id: "Dep".into(),
                    version_range: r.map(str::to_string),
                    include: None,
                    exclude: None,
                })
                .collect(),
        }]
    }

    #[test]
    fn a_semver2_bound_makes_the_dependencies_semver2() {
        assert!(!depends_on_semver2(&[]));
        assert!(!depends_on_semver2(&group(&[None, Some("1.0.0-beta")])));
        assert!(!depends_on_semver2(&group(&[Some("[1.0, 2.0)")])));
        assert!(!depends_on_semver2(&group(&[Some("not a range")])));
        assert!(depends_on_semver2(&group(&[Some("1.0.0-beta.1")])));
        assert!(depends_on_semver2(&group(&[None, Some("[1.0, 2.0+meta]")])));
        assert!(depends_on_semver2(&group(&[Some("(, 2.0.0-rc.1)")])));
    }
}
