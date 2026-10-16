//! Manifest data a `.nuspec` declares, independent of how it is stored.

use serde::{Deserialize, Serialize};

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
