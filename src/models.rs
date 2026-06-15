//! Core domain models shared across storage, database and the HTTP layer.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::version::NuGetVersion;

/// A fully indexed package version, as stored in the database.
///
/// Sizes are kept as [`i64`]/[`u64`] so packages larger than 4 GiB (and indeed
/// the 25 GiB+ packages YANuget is designed to handle) are represented exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    /// The package id, preserving the author's original casing.
    pub id: String,
    /// The parsed version.
    pub version: NuGetVersion,
    /// Whether the version is listed (visible in search/registration).
    pub listed: bool,
    /// Authors, joined with `, ` as in the nuspec.
    pub authors: Vec<String>,
    pub description: String,
    pub icon_url: Option<String>,
    pub license_url: Option<String>,
    pub license_expression: Option<String>,
    pub project_url: Option<String>,
    pub repository_url: Option<String>,
    pub repository_type: Option<String>,
    pub min_client_version: Option<String>,
    pub release_notes: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub tags: Vec<String>,
    /// Whether the package embeds a readme.
    pub has_readme: bool,
    /// Whether the package embeds an icon.
    pub has_embedded_icon: bool,
    /// Whether this version requires development dependency semantics.
    pub is_development_dependency: bool,
    /// `true` when the version requires `SemVerLevel=2.0.0` to be visible.
    pub is_semver2: bool,
    /// Total uncompressed-irrelevant on-disk size of the `.nupkg`, in bytes.
    pub package_size: u64,
    /// Base64-encoded SHA-512 of the `.nupkg`.
    pub package_hash: String,
    /// Algorithm used for [`Package::package_hash`] (always `SHA512`).
    pub package_hash_algorithm: String,
    /// When the package was published to this server.
    pub published: DateTime<Utc>,
    /// Cumulative download count.
    pub downloads: u64,
    /// Declared package types (e.g. `Dependency`, `DotnetTool`).
    pub package_types: Vec<PackageType>,
    /// Dependency groups keyed by target framework.
    pub dependencies: Vec<DependencyGroup>,
}

impl Package {
    /// The lower-cased package id used for case-insensitive lookups and URLs.
    pub fn lower_id(&self) -> String {
        self.id.to_lowercase()
    }

    /// The normalized version string used in URLs and storage paths.
    pub fn normalized_version(&self) -> String {
        self.version.normalized()
    }

    /// Whether the package is a pre-release.
    pub fn is_prerelease(&self) -> bool {
        self.version.is_prerelease()
    }
}

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
    /// The version range string (e.g. `[1.0.0, 2.0.0)`), if specified.
    pub version_range: Option<String>,
    /// Comma-separated `include` assets.
    pub include: Option<String>,
    /// Comma-separated `exclude` assets.
    pub exclude: Option<String>,
}
