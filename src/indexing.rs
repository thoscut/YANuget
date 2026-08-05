//! The package indexing pipeline.
//!
//! Given a `.nupkg` already streamed to a temp file (so the body never touches
//! memory), indexing reads the manifest, validates it, stores the payload and
//! its sidecars, and records the metadata — rolling storage back if the
//! database write fails so no orphaned files are left behind.

use std::path::PathBuf;

use chrono::Utc;

use crate::config::{LicensePolicyConfig, OverwriteMode};
use crate::database::{Membership, PackageDatabase};
use crate::error::{Error, Result};
use crate::models::Package;
use crate::nuspec::{self, Nuspec};
use crate::policy;
use crate::storage::{AuxFile, PackageStorage};
use crate::streaming::StreamSummary;
use crate::version::NuGetVersion;
use crate::{nupkg, validation};

/// Caps for embedded sidecar files extracted into auxiliary storage.
const MAX_README_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ICON_BYTES: u64 = 4 * 1024 * 1024;

/// Options influencing how a package is indexed into a feed.
#[derive(Debug, Clone, Default)]
pub struct IndexOptions {
    /// Whether (and for which versions) an existing id/version is replaced
    /// instead of rejected.
    pub overwrite: OverwriteMode,
    /// The new membership starts pending (withheld until an admin approves it).
    pub pending: bool,
    /// The feed's offline license policy, evaluated against the package.
    pub license_policy: LicensePolicyConfig,
    /// The identity the caller asked for, when it knows one up front.
    ///
    /// A push is self-describing — whatever the manifest says *is* the package.
    /// A mirror or migration is not: the caller requested a specific id/version
    /// from an upstream that could answer with something else entirely. Setting
    /// this makes the manifest prove it is what was asked for, so a hostile or
    /// compromised upstream cannot substitute a different package under a name
    /// local clients already trust.
    pub expect: Option<ExpectedIdentity>,
}

/// The id/version a caller requires the indexed manifest to declare.
#[derive(Debug, Clone)]
pub struct ExpectedIdentity {
    pub id: String,
    pub version: NuGetVersion,
}

/// The identity (and outcome) of a successfully indexed package.
#[derive(Debug, Clone)]
pub struct IndexResult {
    pub id: String,
    pub version: NuGetVersion,
    /// Whether the new membership is pending approval.
    pub pending: bool,
    /// A policy-violation reason recorded on the membership, if any.
    pub flag_reason: Option<String>,
}

/// Index a package whose bytes already live at `temp_path` into `feed`, with
/// `summary` describing its size and hash. On success the temp file has been
/// moved into permanent storage (or removed when the payload was already
/// stored by another feed); on failure it is removed.
///
/// The package metadata and payload are stored once and shared across feeds;
/// this only adds (or refreshes) `feed`'s membership.
pub async fn index_package(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    temp_path: PathBuf,
    summary: StreamSummary,
    options: &IndexOptions,
) -> Result<IndexResult> {
    // Any early error must clean up the temp file.
    let result = index_inner(storage, db, feed, &temp_path, summary, options).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }
    result
}

async fn index_inner(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    temp_path: &PathBuf,
    summary: StreamSummary,
    options: &IndexOptions,
) -> Result<IndexResult> {
    // 1. Read and parse the manifest (seek-based; never reads the payload).
    let archive = nupkg::read_archive(temp_path).await?;
    let manifest = nuspec::parse_nuspec(&archive.nuspec_xml)?;

    validation::validate_package_id(&manifest.id)?;
    let version = NuGetVersion::parse(&manifest.version)
        .map_err(|e| Error::InvalidPackage(format!("invalid <version>: {e}")))?;

    let id = manifest.id.clone();
    let normalized = version.normalized();

    // The caller pinned an identity (mirror/migrate): the fetched payload must
    // be the package that was requested, not merely a valid package.
    if let Some(want) = &options.expect {
        if !want.id.eq_ignore_ascii_case(&id) || want.version != version {
            return Err(Error::InvalidPackage(format!(
                "manifest declares {}/{} but {}/{} was requested",
                id,
                normalized,
                want.id,
                want.version.normalized(),
            )));
        }
    }

    // 2. Resolve sidecar presence and pull readme/icon out while we still have
    //    the file (before it is moved into storage).
    let readme_bytes = match &manifest.readme {
        Some(path) if archive.contains(path) => {
            nupkg::extract_file(temp_path, path, MAX_README_BYTES).await?
        }
        _ => None,
    };
    let icon_bytes = match &manifest.icon {
        Some(path) if archive.contains(path) => {
            nupkg::extract_file(temp_path, path, MAX_ICON_BYTES).await?
        }
        _ => None,
    };

    let package = build_package(
        &manifest,
        version.clone(),
        &summary,
        &readme_bytes,
        &icon_bytes,
    );

    // 3. Evaluate the feed's license policy. Under "block" this rejects the
    //    push; under "warn" it records a flag on the new membership.
    let outcome = policy::evaluate_license(&options.license_policy, &package);
    if !outcome.allowed {
        return Err(Error::PolicyViolation(
            outcome.violation.unwrap_or_else(|| "license policy".into()),
        ));
    }

    // Serialize the store/dedup/membership sequence against any concurrent push
    // or purge of the *same* version (the payload and metadata are shared across
    // feeds), so a racing purge can never delete a payload we just adopted.
    let _guard = crate::locks::lock_version(&id, &normalized).await;

    // 4. Honour immutability / overwrite policy *within this feed*.
    if db.exists(feed, &id, &version).await? {
        if options.overwrite.allows(version.is_prerelease()) {
            db.remove_membership(feed, &id, &version).await?;
            // If no other feed references the version, drop the orphaned global
            // data and payload so the re-push stores fresh content.
            if db.feed_count(&id, &version).await? == 0 {
                let _ = db.delete_package_data(&id, &version).await;
                let _ = storage.delete(&id, &normalized).await;
            }
        } else {
            return Err(Error::PackageAlreadyExists);
        }
    }

    // 5. Store the payload + sidecars once. If another feed already holds this
    //    version, the bytes are present — drop our temp copy instead.
    let stored_now = !db.package_data_exists(&id, &version).await?;
    if stored_now {
        storage
            .store_package(&id, &normalized, temp_path.clone())
            .await?;
        storage
            .store_aux(
                &id,
                &normalized,
                AuxFile::Nuspec,
                archive.nuspec_xml.as_bytes(),
            )
            .await?;
        if let Some(bytes) = &readme_bytes {
            storage
                .store_aux(&id, &normalized, AuxFile::Readme, bytes)
                .await?;
        }
        if let Some(bytes) = &icon_bytes {
            storage
                .store_aux(&id, &normalized, AuxFile::Icon, bytes)
                .await?;
        }
    } else {
        // Another feed already holds this exact id/version, so its payload — not
        // ours — is what every client will download. Publishing our metadata
        // over it would advertise a hash and size that do not describe those
        // bytes, and a NuGet client verifying `packageHash` would reject the
        // restore. Only adopt the existing payload when it really is the same
        // content; otherwise this push is a different package wearing a taken
        // name, and it is refused.
        if let Some(existing) = db.get_package_data(&id, &version).await? {
            if existing.package_hash != package.package_hash {
                return Err(Error::PackageAlreadyExists);
            }
        }
        let _ = tokio::fs::remove_file(temp_path).await;
    }

    // 6. Record global metadata (idempotent) and this feed's membership; roll
    //    freshly stored payload back on failure. The global per-version lock
    //    above plus the `feed_count == 0` guard mean a concurrent push that won
    //    the same-feed race (or another feed) keeps the shared payload alive —
    //    deleting it on a duplicate would yank the version directory out from
    //    under the winner.
    db.upsert_package_data(&package).await?;
    let membership = Membership {
        feed: feed.to_string(),
        lower_id: package.lower_id(),
        normalized_version: normalized.clone(),
        listed: true,
        enabled: true,
        pending: options.pending,
        flagged: outcome.violation.is_some(),
        flag_reason: outcome.violation.clone(),
    };
    if let Err(e) = db.add_membership(&membership).await {
        if stored_now && db.feed_count(&id, &version).await.unwrap_or(0) == 0 {
            let _ = db.delete_package_data(&id, &version).await;
            let _ = storage.delete(&id, &normalized).await;
        }
        return Err(e);
    }

    Ok(IndexResult {
        id,
        version,
        pending: options.pending,
        flag_reason: outcome.violation,
    })
}

fn build_package(
    n: &Nuspec,
    version: NuGetVersion,
    summary: &StreamSummary,
    readme_bytes: &Option<Vec<u8>>,
    icon_bytes: &Option<Vec<u8>>,
) -> Package {
    Package {
        id: n.id.clone(),
        is_semver2: version.is_semver2(),
        version,
        listed: true,
        enabled: true,
        authors: n.author_list(),
        description: n.description.clone().unwrap_or_default(),
        icon_url: n.icon_url.clone(),
        license_url: n.license_url.clone(),
        license_expression: n.license_expression.clone(),
        project_url: n.project_url.clone(),
        repository_url: n.repository_url.clone(),
        repository_type: n.repository_type.clone(),
        min_client_version: n.min_client_version.clone(),
        release_notes: n.release_notes.clone(),
        language: n.language.clone(),
        title: n.title.clone(),
        summary: n.summary.clone(),
        tags: n.tag_list(),
        has_readme: readme_bytes.is_some(),
        has_embedded_icon: icon_bytes.is_some(),
        is_development_dependency: n.development_dependency,
        require_license_acceptance: n.require_license_acceptance,
        package_size: summary.size,
        package_hash: summary.sha512_base64.clone(),
        package_hash_algorithm: "SHA512".to_string(),
        published: Utc::now(),
        downloads: 0,
        package_types: n.package_types.clone(),
        dependencies: n.dependency_groups.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::SqliteDatabase;
    use crate::storage::FilesystemStorage;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    /// Construct a `.nupkg` temp file with the given nuspec and optional readme.
    async fn make_package(nuspec: &str, with_readme: bool) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pkg.nupkg");
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        zip.start_file("pkg.nuspec", opts).unwrap();
        zip.write_all(nuspec.as_bytes()).unwrap();
        if with_readme {
            zip.start_file("docs/README.md", opts).unwrap();
            zip.write_all(b"# Hello").unwrap();
        }
        zip.finish().unwrap();
        (dir, path)
    }

    async fn summary_for(path: &PathBuf) -> StreamSummary {
        use tokio_util::io::ReaderStream;
        let file = tokio::fs::File::open(path).await.unwrap();
        let stream = ReaderStream::new(file);
        let mut sink = tokio::io::sink();
        crate::streaming::stream_to_writer(stream, &mut sink)
            .await
            .unwrap()
    }

    const NUSPEC: &str = r#"<?xml version="1.0"?>
        <package><metadata>
            <id>Contoso.Utils</id>
            <version>1.2.3</version>
            <authors>Alice</authors>
            <description>Helpers</description>
            <readme>docs/README.md</readme>
            <tags>util helper</tags>
        </metadata></package>"#;

    const FEED: &str = "default";

    #[tokio::test]
    async fn indexes_a_package_end_to_end() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();

        let (_d, temp) = make_package(NUSPEC, true).await;
        let summary = summary_for(&temp).await;

        let result = index_package(&storage, &db, FEED, temp, summary, &IndexOptions::default())
            .await
            .unwrap();
        assert_eq!(result.id, "Contoso.Utils");
        assert_eq!(result.version.normalized(), "1.2.3");

        // Metadata landed in the DB.
        let pkg = db
            .find(FEED, "contoso.utils", &result.version)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pkg.authors, vec!["Alice"]);
        assert!(pkg.has_readme);
        assert_eq!(pkg.package_hash_algorithm, "SHA512");

        // Payload and sidecars landed in storage.
        assert!(storage.package_exists("contoso.utils", "1.2.3").await);
        let nuspec = storage
            .get_aux("contoso.utils", "1.2.3", AuxFile::Nuspec)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&nuspec).contains("Contoso.Utils"));
        let readme = storage
            .get_aux("contoso.utils", "1.2.3", AuxFile::Readme)
            .await
            .unwrap();
        assert_eq!(readme, b"# Hello");
    }

    #[tokio::test]
    async fn rejects_duplicate_without_overwrite() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();

        let (_d1, temp1) = make_package(NUSPEC, false).await;
        let s1 = summary_for(&temp1).await;
        index_package(&storage, &db, FEED, temp1, s1, &IndexOptions::default())
            .await
            .unwrap();

        let (_d2, temp2) = make_package(NUSPEC, false).await;
        let s2 = summary_for(&temp2).await;
        let err = index_package(
            &storage,
            &db,
            FEED,
            temp2.clone(),
            s2,
            &IndexOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::PackageAlreadyExists));
        // The rejected temp file was cleaned up.
        assert!(!temp2.exists());
    }

    #[tokio::test]
    async fn overwrite_replaces_existing() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        let opts = IndexOptions {
            overwrite: OverwriteMode::Enabled,
            ..Default::default()
        };

        let (_d1, temp1) = make_package(NUSPEC, false).await;
        let s1 = summary_for(&temp1).await;
        index_package(&storage, &db, FEED, temp1, s1, &opts)
            .await
            .unwrap();

        let (_d2, temp2) = make_package(NUSPEC, true).await;
        let s2 = summary_for(&temp2).await;
        let result = index_package(&storage, &db, FEED, temp2, s2, &opts)
            .await
            .unwrap();
        let pkg = db
            .find(FEED, "contoso.utils", &result.version)
            .await
            .unwrap()
            .unwrap();
        assert!(pkg.has_readme); // the second push had a readme
    }

    #[tokio::test]
    async fn rejects_invalid_manifest() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();

        let bad =
            r#"<package><metadata><id>Bad Id!</id><version>1.0.0</version></metadata></package>"#;
        let (_d, temp) = make_package(bad, false).await;
        let s = summary_for(&temp).await;
        let err = index_package(&storage, &db, FEED, temp, s, &IndexOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::InvalidPackage(_)));
    }

    #[tokio::test]
    async fn shared_payload_is_not_duplicated_across_feeds() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();

        let (_d1, temp1) = make_package(NUSPEC, false).await;
        let s1 = summary_for(&temp1).await;
        index_package(&storage, &db, "dev", temp1, s1, &IndexOptions::default())
            .await
            .unwrap();

        // Pushing the same version into a second feed succeeds and adds only a
        // membership — the payload is already present.
        let (_d2, temp2) = make_package(NUSPEC, false).await;
        let s2 = summary_for(&temp2).await;
        let res = index_package(
            &storage,
            &db,
            "stable",
            temp2.clone(),
            s2,
            &IndexOptions::default(),
        )
        .await
        .unwrap();
        assert!(!temp2.exists(), "second temp should be dropped, not stored");
        let v = res.version.clone();
        assert_eq!(db.feed_count("contoso.utils", &v).await.unwrap(), 2);
        assert!(db.find("dev", "contoso.utils", &v).await.unwrap().is_some());
        assert!(db
            .find("stable", "contoso.utils", &v)
            .await
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn blocking_license_policy_rejects_push() {
        use crate::config::{LicensePolicyConfig, PolicyAction};
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();

        // NUSPEC declares no license; block unlicensed packages.
        let opts = IndexOptions {
            license_policy: LicensePolicyConfig {
                enabled: true,
                allow_unlicensed: false,
                action: PolicyAction::Block,
                ..Default::default()
            },
            ..Default::default()
        };
        let (_d, temp) = make_package(NUSPEC, false).await;
        let s = summary_for(&temp).await;
        let err = index_package(&storage, &db, FEED, temp, s, &opts)
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PolicyViolation(_)));
        // Nothing was stored.
        assert!(!storage.package_exists("contoso.utils", "1.2.3").await);
    }

    #[tokio::test]
    async fn warning_license_policy_flags_membership() {
        use crate::config::{LicensePolicyConfig, PolicyAction};
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();

        let opts = IndexOptions {
            license_policy: LicensePolicyConfig {
                enabled: true,
                allow_unlicensed: false,
                action: PolicyAction::Warn,
                ..Default::default()
            },
            ..Default::default()
        };
        let (_d, temp) = make_package(NUSPEC, false).await;
        let s = summary_for(&temp).await;
        let res = index_package(&storage, &db, FEED, temp, s, &opts)
            .await
            .unwrap();
        assert!(res.flag_reason.is_some());
        let all = db.find_all_versions(FEED, "contoso.utils").await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].flagged);
    }
}
