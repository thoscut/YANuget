//! Package retention: automatic pruning of old versions.
//!
//! The decision of *which* versions to drop is a pure, side-effect-free
//! function ([`versions_to_prune`]) so it can be exhaustively unit-tested; the
//! I/O that acts on that decision ([`prune_package`], [`prune_all`]) is a thin
//! wrapper over the existing storage/database primitives.
//!
//! A version is pruned when it is **excess by count** (beyond the newest N of
//! its release channel) **or** **too old** (published before the age cutoff).
//! As a safety floor the newest stable version is always kept — or, when a
//! package has no stable version, its newest pre-release — so retention can
//! never make a package disappear entirely.

use chrono::{DateTime, Duration, Utc};

use crate::config::RetentionConfig;
use crate::database::PackageDatabase;
use crate::error::Result;
use crate::models::Package;
use crate::storage::PackageStorage;
use crate::version::NuGetVersion;

/// The evaluated retention rules (a snapshot of [`RetentionConfig`]).
#[derive(Debug, Clone, Default)]
pub struct RetentionPolicy {
    pub keep_latest_stable: Option<usize>,
    pub keep_latest_prerelease: Option<usize>,
    pub max_age_days: Option<u64>,
}

impl RetentionPolicy {
    /// Whether any limit is set; with none, pruning is a no-op.
    pub fn has_limits(&self) -> bool {
        self.keep_latest_stable.is_some()
            || self.keep_latest_prerelease.is_some()
            || self.max_age_days.is_some()
    }
}

impl From<&RetentionConfig> for RetentionPolicy {
    fn from(c: &RetentionConfig) -> Self {
        Self {
            keep_latest_stable: c.keep_latest_stable,
            keep_latest_prerelease: c.keep_latest_prerelease,
            max_age_days: c.max_age_days,
        }
    }
}

/// Decide which versions of a single package id to prune.
///
/// `packages` may be in any order; the returned versions are those that should
/// be removed. `now` is injected for deterministic testing.
pub fn versions_to_prune(
    packages: &[Package],
    policy: &RetentionPolicy,
    now: DateTime<Utc>,
) -> Vec<NuGetVersion> {
    if packages.is_empty() || !policy.has_limits() {
        return Vec::new();
    }

    // Newest-first within each channel so "rank" is an index from the top.
    let mut stable: Vec<&Package> = packages.iter().filter(|p| !p.is_prerelease()).collect();
    let mut prerelease: Vec<&Package> = packages.iter().filter(|p| p.is_prerelease()).collect();
    stable.sort_by(|a, b| b.version.cmp(&a.version));
    prerelease.sort_by(|a, b| b.version.cmp(&a.version));

    // The single version that must survive no matter what.
    let protected: Option<&NuGetVersion> = stable
        .first()
        .or_else(|| prerelease.first())
        .map(|p| &p.version);

    let cutoff = policy.max_age_days.map(|d| now - Duration::days(d as i64));

    let mut prune = Vec::new();
    for (channel, keep) in [
        (&stable, policy.keep_latest_stable),
        (&prerelease, policy.keep_latest_prerelease),
    ] {
        for (rank, pkg) in channel.iter().enumerate() {
            if protected.is_some_and(|v| *v == pkg.version) {
                continue;
            }
            let excess_by_count = keep.is_some_and(|n| rank >= n);
            let too_old = cutoff.is_some_and(|c| pkg.published < c);
            if excess_by_count || too_old {
                prune.push(pkg.version.clone());
            }
        }
    }
    prune
}

/// Hard-delete one package version and everything attached to it: its symbol
/// files and mappings, its stored payload/sidecars, and its database row.
///
/// Used by both the retention sweep and the API's hard-delete path so symbol
/// cleanup is never forgotten.
pub async fn purge_version(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    for sym in db.find_symbols(id, version).await.unwrap_or_default() {
        let _ = storage.delete_symbol(&sym.key, &sym.filename).await;
    }
    let _ = db.delete_symbols(id, version).await;
    let removed = db.delete(id, version).await?;
    let _ = storage.delete(id, &version.normalized()).await;
    Ok(removed)
}

/// Apply the policy to a single package id. Returns the number of versions
/// pruned.
pub async fn prune_package(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    id: &str,
    policy: &RetentionPolicy,
) -> Result<usize> {
    if !policy.has_limits() {
        return Ok(0);
    }
    let packages = db.find_versions(id, true).await?;
    let to_prune = versions_to_prune(&packages, policy, Utc::now());
    let mut pruned = 0;
    for version in &to_prune {
        if purge_version(storage, db, id, version).await? {
            pruned += 1;
            tracing::info!(%id, version = %version.normalized(), "retention pruned version");
        }
    }
    Ok(pruned)
}

/// Apply the policy to every package id. Returns the total versions pruned.
pub async fn prune_all(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    policy: &RetentionPolicy,
) -> Result<usize> {
    if !policy.has_limits() {
        return Ok(0);
    }
    let ids = db.all_package_ids().await?;
    let mut total = 0;
    for id in ids {
        match prune_package(storage, db, &id, policy).await {
            Ok(n) => total += n,
            Err(e) => tracing::error!(%id, error = %e, "retention sweep failed for package"),
        }
    }
    if total > 0 {
        tracing::info!(pruned = total, "retention sweep complete");
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn pkg(version: &str, days_old: i64) -> Package {
        let published = Utc::now() - Duration::days(days_old);
        Package {
            id: "Pkg".into(),
            version: NuGetVersion::parse(version).unwrap(),
            listed: true,
            authors: vec![],
            description: String::new(),
            icon_url: None,
            license_url: None,
            license_expression: None,
            project_url: None,
            repository_url: None,
            repository_type: None,
            min_client_version: None,
            release_notes: None,
            language: None,
            title: None,
            summary: None,
            tags: vec![],
            has_readme: false,
            has_embedded_icon: false,
            is_development_dependency: false,
            is_semver2: false,
            package_size: 1,
            package_hash: "h".into(),
            package_hash_algorithm: "SHA512".into(),
            published,
            downloads: 0,
            package_types: vec![],
            dependencies: vec![],
        }
    }

    fn names(mut v: Vec<NuGetVersion>) -> Vec<String> {
        v.sort();
        v.iter().map(|x| x.normalized()).collect()
    }

    #[test]
    fn no_limits_prunes_nothing() {
        let packages = vec![pkg("1.0.0", 0), pkg("2.0.0", 0)];
        let out = versions_to_prune(&packages, &RetentionPolicy::default(), Utc::now());
        assert!(out.is_empty());
    }

    #[test]
    fn keeps_newest_n_stable() {
        let packages = vec![
            pkg("1.0.0", 0),
            pkg("1.1.0", 0),
            pkg("1.2.0", 0),
            pkg("2.0.0", 0),
        ];
        let policy = RetentionPolicy {
            keep_latest_stable: Some(2),
            ..Default::default()
        };
        // Keep 2.0.0 and 1.2.0; prune 1.1.0 and 1.0.0.
        assert_eq!(
            names(versions_to_prune(&packages, &policy, Utc::now())),
            vec!["1.0.0", "1.1.0"]
        );
    }

    #[test]
    fn stable_and_prerelease_channels_are_independent() {
        let packages = vec![
            pkg("1.0.0", 0),
            pkg("2.0.0", 0),
            pkg("3.0.0-rc.1", 0),
            pkg("3.0.0-rc.2", 0),
        ];
        let policy = RetentionPolicy {
            keep_latest_stable: Some(1),
            keep_latest_prerelease: Some(1),
            ..Default::default()
        };
        // Keep newest stable (2.0.0) and newest prerelease (3.0.0-rc.2).
        assert_eq!(
            names(versions_to_prune(&packages, &policy, Utc::now())),
            vec!["1.0.0", "3.0.0-rc.1"]
        );
    }

    #[test]
    fn prunes_by_age_but_protects_newest() {
        let packages = vec![pkg("1.0.0", 100), pkg("2.0.0", 100), pkg("3.0.0", 1)];
        let policy = RetentionPolicy {
            max_age_days: Some(30),
            ..Default::default()
        };
        // 1.0.0 is old → pruned. 2.0.0 is old but... not protected (3.0.0 is
        // the newest stable). So both old ones go.
        assert_eq!(
            names(versions_to_prune(&packages, &policy, Utc::now())),
            vec!["1.0.0", "2.0.0"]
        );
    }

    #[test]
    fn never_prunes_the_only_version() {
        let packages = vec![pkg("1.0.0", 9999)];
        let policy = RetentionPolicy {
            keep_latest_stable: Some(0),
            max_age_days: Some(1),
            ..Default::default()
        };
        assert!(versions_to_prune(&packages, &policy, Utc::now()).is_empty());
    }

    #[test]
    fn protects_newest_prerelease_when_no_stable() {
        let packages = vec![pkg("1.0.0-a", 9999), pkg("1.0.0-b", 9999)];
        let policy = RetentionPolicy {
            keep_latest_prerelease: Some(0),
            ..Default::default()
        };
        // Both exceed the count cap of 0, but the newest (1.0.0-b) is protected.
        assert_eq!(
            names(versions_to_prune(&packages, &policy, Utc::now())),
            vec!["1.0.0-a"]
        );
    }

    #[test]
    fn age_uses_injected_now() {
        let base = Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
        let mut old = pkg("1.0.0", 0);
        old.published = base;
        let mut new = pkg("2.0.0", 0);
        new.published = base + Duration::days(40);
        let policy = RetentionPolicy {
            max_age_days: Some(30),
            ..Default::default()
        };
        let now = base + Duration::days(50);
        // 1.0.0 is 50 days old (> 30) and not protected (2.0.0 is newest).
        assert_eq!(
            names(versions_to_prune(&[old, new], &policy, now)),
            vec!["1.0.0"]
        );
    }
}
