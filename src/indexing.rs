//! The package indexing pipeline.
//!
//! Given a `.nupkg` already streamed to a temp file (so the body never touches
//! memory), indexing reads the manifest, validates it, stores the payload and
//! its sidecars, and records the metadata — rolling storage back if the
//! database write fails so no orphaned files are left behind.

use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::config::{LicensePolicyConfig, OverwriteMode, ReservedPrefix};
use crate::database::{Membership, PackageDatabase};
use crate::error::{Error, Result};
use crate::models::Package;
use crate::nuspec::{self, Nuspec};
use crate::policy;
use crate::storage::{AuxFile, PackageContent, PackageStorage};
use crate::streaming::StreamSummary;
use crate::version::NuGetVersion;
use crate::{nupkg, validation};

/// Caps for embedded sidecar files extracted into auxiliary storage.
///
/// These bound decompression work per push *and* the size of what the gallery
/// later renders on every page view — a highly compressible readme is otherwise
/// a cheap way to turn a small upload into a huge response served repeatedly.
/// 1 MiB matches the limit nuget.org enforces on embedded readmes, and is far
/// above any real one.
const MAX_README_BYTES: u64 = 1024 * 1024;
const MAX_ICON_BYTES: u64 = 1024 * 1024;

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
    /// Id prefixes other feeds reserved: an id under one is refused here.
    pub reserved_elsewhere: Vec<ReservedPrefix>,
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
    let manifest = nuspec::parse_nuspec_blocking(archive.nuspec_xml.clone()).await?;

    validation::validate_package_id(&manifest.id)?;
    let version = NuGetVersion::parse(&manifest.version)
        .map_err(|e| Error::InvalidPackage(format!("invalid <version>: {e}")))?;

    let id = manifest.id.clone();
    let normalized = version.normalized();

    // Before anything is stored: the first feed to store an id+version owns
    // it everywhere, so a reserved prefix is only worth anything if no other
    // feed can get there first.
    if let Some(reserved) = options.reserved_elsewhere.iter().find(|r| r.covers(&id)) {
        tracing::warn!(%feed, %id, version = %normalized, owner = %reserved.feed, "refused: id prefix reserved for another feed");
        return Err(Error::Forbidden(format!(
            "package id {id} is under the prefix {:?}, reserved for feed {:?}",
            reserved.prefix, reserved.feed
        )));
    }

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

    let sidecars = Sidecars {
        nuspec: archive.nuspec_xml.as_bytes(),
        readme: readme_bytes.as_deref(),
        icon: icon_bytes.as_deref(),
    };
    let mut existing = db.get_package_data(&id, &version).await?;
    let mut previous = db.get_membership(feed, &id, &version).await?;

    // 4. Whatever a failed purge left behind is finished off first, so this
    //    push starts from a clean slate instead of inheriting it.
    //
    //    Global data that no feed holds any more is an orphan: nothing serves
    //    it, and adopting it would adopt a payload that may already be gone —
    //    every download of the new membership would then 404, and different
    //    bytes would be refused in every feed for the rest of time.
    if existing.is_some() && db.feed_count(&id, &version).await? == 0 {
        tracing::warn!(%feed, %id, version = %normalized, "replacing the remains of an unfinished purge");
        crate::retention::purge_global_data(storage, db, &id, &version).await?;
        existing = None;
    }
    //    The reverse, a membership without data, cannot serve anything either.
    if existing.is_none() && previous.is_some() {
        tracing::warn!(%feed, %id, version = %normalized, "dropping a membership that has no package data");
        db.remove_membership(feed, &id, &version).await?;
        previous = None;
    }

    let flagged = outcome.violation.is_some();
    let result = IndexResult {
        id: id.clone(),
        version: version.clone(),
        pending: options.pending,
        flag_reason: outcome.violation.clone(),
    };

    let Some(existing) = existing else {
        // 5a. A version new to the server: store the payload and sidecars, then
        //     record the data and the membership together, removing the
        //     payload again if that fails.
        store_payload(storage, &id, &normalized, temp_path, &sidecars).await?;
        let membership = Membership {
            pending: options.pending,
            flagged,
            flag_reason: outcome.violation.clone(),
            ..Membership::active(feed, &package)
        };
        if let Err(e) = db.add_version(&package, &membership).await {
            if db.feed_count(&id, &version).await.unwrap_or(0) == 0 {
                let _ = db.delete_package_data(&id, &version).await;
                let _ = storage.delete(&id, &normalized).await;
            }
            return Err(e);
        }
        return Ok(result);
    };

    // The bytes the rows describe are not on disk: a purge or a store failed
    // part-way. Any push of the same content may put them back.
    let payload_present = storage.package_exists(&id, &normalized).await;
    let same_content = existing.package_hash == package.package_hash;

    let Some(previous) = previous else {
        // 5b. Another feed already holds this exact id/version, so its payload
        //     is what every client downloads. Publishing our metadata over it
        //     would advertise a hash and size that do not describe those bytes,
        //     so only the same content may join.
        if !same_content {
            return Err(taken(feed, &id, &version));
        }
        if payload_present {
            let _ = tokio::fs::remove_file(temp_path).await;
        } else {
            tracing::warn!(%feed, %id, version = %normalized, "restoring a missing payload from an identical push");
            store_payload(storage, &id, &normalized, temp_path, &sidecars).await?;
        }
        let membership = Membership {
            pending: options.pending,
            flagged,
            flag_reason: outcome.violation.clone(),
            ..Membership::active(feed, &package)
        };
        db.add_version(&existing, &membership).await?;
        return Ok(result);
    };

    // 5c. The version is already in this feed: honour the overwrite policy.
    if !options.overwrite.allows(version.is_prerelease()) {
        return Err(Error::VersionExists(already_here(
            &id,
            &version,
            Some(&previous),
            options.overwrite,
        )));
    }
    // Another feed holding this version pins its payload: the stored bytes
    // cannot be replaced from here, so only an identical re-push can succeed.
    if !same_content && db.feed_count(&id, &version).await? > 1 {
        return Err(taken(feed, &id, &version));
    }
    // An overwrite replaces the build, not the operator's decisions about the
    // version: a push key must not be able to undo an admin's disable or
    // unlist, and a pin outlives the replacement. New bytes need approval
    // again where the feed gates; the same bytes do not.
    let membership = Membership {
        listed: previous.listed,
        enabled: previous.enabled,
        pending: previous.pending || (options.pending && !same_content),
        flagged,
        flag_reason: outcome.violation.clone(),
        pinned: previous.pinned,
        ..Membership::active(feed, &package)
    };
    let result = IndexResult {
        pending: membership.pending,
        ..result
    };
    if same_content && payload_present {
        // Nothing to store: the bytes on disk are these bytes.
        let _ = tokio::fs::remove_file(temp_path).await;
        db.replace_version(&existing, &membership).await?;
        return Ok(result);
    }
    overwrite(
        storage,
        db,
        temp_path,
        &package,
        &existing,
        &membership,
        &sidecars,
    )
    .await?;
    // Only once the replacement is recorded: the previous build's PDBs have
    // different SSQP keys, so leaving their mappings behind would keep serving
    // them to anyone debugging the new build — the mappings still resolve to an
    // id/version that exists.
    if !same_content {
        crate::retention::purge_symbols(storage, db, &id, &version).await?;
    }
    Ok(result)
}

/// The small files stored next to a payload.
struct Sidecars<'a> {
    nuspec: &'a [u8],
    readme: Option<&'a [u8]>,
    icon: Option<&'a [u8]>,
}

/// Move the payload into storage and write its sidecars.
async fn store_payload(
    storage: &dyn PackageStorage,
    id: &str,
    normalized: &str,
    temp_path: &Path,
    sidecars: &Sidecars<'_>,
) -> Result<()> {
    storage
        .store_package(id, normalized, temp_path.to_path_buf())
        .await?;
    store_sidecars(storage, id, normalized, sidecars).await
}

async fn store_sidecars(
    storage: &dyn PackageStorage,
    id: &str,
    normalized: &str,
    sidecars: &Sidecars<'_>,
) -> Result<()> {
    storage
        .store_aux(id, normalized, AuxFile::Nuspec, sidecars.nuspec)
        .await?;
    if let Some(bytes) = sidecars.readme {
        storage
            .store_aux(id, normalized, AuxFile::Readme, bytes)
            .await?;
    }
    if let Some(bytes) = sidecars.icon {
        storage
            .store_aux(id, normalized, AuxFile::Icon, bytes)
            .await?;
    }
    Ok(())
}

/// Replace a stored build with a new one, keeping the version in the feed
/// throughout.
///
/// The new payload is stored first and the rows are swapped after it in one
/// transaction, so until the swap every row still describes the bytes on disk.
/// The previous payload is hard-linked aside (and the sidecars read) first, so
/// when anything after the store fails the previous build is put back rather
/// than leaving rows that describe one build over the bytes of another. On a
/// filesystem without hard links the previous bytes cannot be kept aside
/// without copying them; the store is still an atomic rename, so only a
/// failure after it can leave the two out of step, and that is logged.
async fn overwrite(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    temp_path: &Path,
    package: &Package,
    existing: &Package,
    membership: &Membership,
    sidecars: &Sidecars<'_>,
) -> Result<()> {
    let id = &package.id;
    let normalized = package.normalized_version();
    let backup = keep_aside(storage, id, &normalized, temp_path).await;
    let old_nuspec = storage.get_aux(id, &normalized, AuxFile::Nuspec).await.ok();
    let old_readme = match existing.has_readme {
        true => storage.get_aux(id, &normalized, AuxFile::Readme).await.ok(),
        false => None,
    };
    let old_icon = match existing.has_embedded_icon {
        true => storage.get_aux(id, &normalized, AuxFile::Icon).await.ok(),
        false => None,
    };

    // The store is an atomic rename: when it fails, the previous build is
    // still in place and nothing else has been touched.
    if let Err(e) = storage
        .store_package(id, &normalized, temp_path.to_path_buf())
        .await
    {
        if let Some(backup) = backup {
            let _ = tokio::fs::remove_file(backup).await;
        }
        return Err(e);
    }
    let swapped = async {
        store_sidecars(storage, id, &normalized, sidecars).await?;
        db.replace_version(package, membership).await
    }
    .await;
    let Err(e) = swapped else {
        if let Some(backup) = backup {
            let _ = tokio::fs::remove_file(backup).await;
        }
        return Ok(());
    };

    // Put the previous build back, so the rows (which the failed swap left
    // as they were) describe the bytes on disk again.
    let restored = match &backup {
        Some(backup) => storage
            .store_package(id, &normalized, backup.clone())
            .await
            .map_err(|e| e.to_string()),
        None => Err("it could not be kept aside".to_string()),
    };
    if let Err(why) = restored {
        if let Some(backup) = backup {
            let _ = tokio::fs::remove_file(backup).await;
        }
        tracing::error!(%id, version = %normalized, error = %why, "an overwrite failed after replacing the payload, and the previous payload could not be restored");
    }
    let previous = Sidecars {
        nuspec: old_nuspec.as_deref().unwrap_or(sidecars.nuspec),
        readme: old_readme.as_deref(),
        icon: old_icon.as_deref(),
    };
    if old_nuspec.is_some() {
        let _ = store_sidecars(storage, id, &normalized, &previous).await;
    }
    Err(e)
}

/// Hard-link a version's stored payload next to `temp_path`, returning the
/// link, or `None` when it cannot be (no payload, not a local file, or a
/// filesystem without links).
async fn keep_aside(
    storage: &dyn PackageStorage,
    id: &str,
    normalized: &str,
    temp_path: &Path,
) -> Option<PathBuf> {
    let PackageContent::LocalPath(current) = storage.get_package(id, normalized).await.ok()?;
    let backup = temp_path.with_file_name(format!("previous-{}.tmp", uuid::Uuid::new_v4()));
    match tokio::fs::hard_link(&current, &backup).await {
        Ok(()) => Some(backup),
        Err(e) => {
            tracing::debug!(%id, version = %normalized, error = %e, "cannot keep the previous payload aside");
            None
        }
    }
}

/// The refusal for different bytes under an id/version the server already
/// stores. An id and version are one namespace across every feed, so this is
/// logged as the failure it is rather than mistaken for a benign race (a
/// mirror fetch that loses to a concurrent one gets `PackageAlreadyExists`).
fn taken(feed: &str, id: &str, version: &NuGetVersion) -> Error {
    let normalized = version.normalized();
    tracing::warn!(
        %feed, %id, version = %normalized,
        "refused: different content is already stored under this id and version"
    );
    Error::Conflict(format!(
        "{id} {normalized} is already stored with different content; an id and \
         version name one package across every feed. {}",
        new_version_advice(version)
    ))
}

/// Why a push of an id/version this feed already holds is refused, in the
/// words a pusher needs: what state the existing one is in, which setting
/// refuses the overwrite, and what to do instead.
///
/// NuGet and Chocolatey show this as the reason of the `409`. The case that
/// prompted it: a version "deleted" with `nuget delete` on a feed without
/// `hard_delete_enabled` is only unlisted — still there, still downloadable —
/// so pushing a corrected build under the same version was refused with
/// nothing but "409 (Conflict)".
fn already_here(
    id: &str,
    version: &NuGetVersion,
    state: Option<&Membership>,
    overwrite: OverwriteMode,
) -> String {
    let normalized = version.normalized();
    let condition = match state {
        Some(m) if m.pending => " (waiting for approval)",
        Some(m) if !m.enabled => " (disabled by an admin)",
        Some(m) if !m.listed => {
            " (unlisted - which is all a delete does while hard_delete_enabled is off - \
             and still downloadable)"
        }
        _ => "",
    };
    let rule = match overwrite {
        OverwriteMode::PrereleaseOnly => {
            "only pre-release versions may be overwritten (allow_overwrite = \"prerelease-only\")"
        }
        _ => "overwriting is off (allow_overwrite = false)",
    };
    format!(
        "{id} {normalized} already exists in this feed{condition}, and {rule}. {} Or \
         delete it for good first.",
        new_version_advice(version)
    )
}

/// "Push it as a new version", with an example of Chocolatey's package fix
/// version (the software's version plus the date) when that applies.
fn new_version_advice(version: &NuGetVersion) -> String {
    let (major, minor, patch, revision) = version.core();
    if !version.is_prerelease() && revision == 0 {
        format!(
            "Push it as a new version, such as {major}.{minor}.{patch}.{}.",
            Utc::now().format("%Y%m%d")
        )
    } else {
        "Push it as a new version.".to_string()
    }
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
        // The refusal says which setting refuses it and what to do instead.
        let Error::VersionExists(why) = &err else {
            panic!("{err:?}");
        };
        assert!(why.contains("already exists in this feed"), "{why}");
        assert!(why.contains("allow_overwrite = false"), "{why}");
        assert!(why.contains("Push it as a new version"), "{why}");
        // The rejected temp file was cleaned up.
        assert!(!temp2.exists());
    }

    #[test]
    fn a_refused_push_explains_the_state_and_the_way_out() {
        let v = NuGetVersion::parse("11.3.0").unwrap();
        let unlisted = Membership {
            feed: FEED.into(),
            lower_id: "octave.install".into(),
            normalized_version: "11.3.0".into(),
            listed: false,
            enabled: true,
            pending: false,
            flagged: false,
            flag_reason: None,
            pinned: false,
        };
        let why = already_here(
            "octave.install",
            &v,
            Some(&unlisted),
            OverwriteMode::Disabled,
        );
        assert!(
            why.starts_with("octave.install 11.3.0 already exists in this feed (unlisted"),
            "{why}"
        );
        assert!(why.contains("hard_delete_enabled is off"), "{why}");
        // Chocolatey's package fix version: the software's version and a date.
        let today = Utc::now().format("%Y%m%d").to_string();
        assert!(why.contains(&format!("such as 11.3.0.{today}")), "{why}");
        // Pre-releases and four-part versions get no fix-version example.
        for other in ["2.0.0-rc.1", "1.2.3.4"] {
            let v = NuGetVersion::parse(other).unwrap();
            assert_eq!(new_version_advice(&v), "Push it as a new version.");
        }
        let pre_only = already_here("p", &v, None, OverwriteMode::PrereleaseOnly);
        assert!(
            pre_only.contains("only pre-release versions may be overwritten"),
            "{pre_only}"
        );
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
    // --- overwrites, orphans and the shared namespace ---

    use crate::database::PackageFile;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn overwriting() -> IndexOptions {
        IndexOptions {
            overwrite: OverwriteMode::Enabled,
            ..Default::default()
        }
    }

    /// Index a fresh build of [`NUSPEC`]; the readme makes the bytes differ.
    async fn push(
        storage: &dyn PackageStorage,
        db: &dyn PackageDatabase,
        feed: &str,
        with_readme: bool,
        opts: &IndexOptions,
    ) -> Result<IndexResult> {
        let (_d, temp) = make_package(NUSPEC, with_readme).await;
        let s = summary_for(&temp).await;
        index_package(storage, db, feed, temp, s, opts).await
    }

    fn v123() -> NuGetVersion {
        NuGetVersion::parse("1.2.3").unwrap()
    }

    async fn stored_bytes(storage: &FilesystemStorage) -> Vec<u8> {
        let PackageContent::LocalPath(path) =
            storage.get_package("contoso.utils", "1.2.3").await.unwrap();
        tokio::fs::read(path).await.unwrap()
    }

    #[tokio::test]
    async fn an_overwrite_keeps_admin_state_and_attached_files() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        push(&storage, &db, FEED, false, &overwriting())
            .await
            .unwrap();
        let v = v123();
        db.set_enabled(FEED, "contoso.utils", &v, false)
            .await
            .unwrap();
        db.set_listed(FEED, "contoso.utils", &v, false)
            .await
            .unwrap();
        db.set_pinned(FEED, "contoso.utils", &v, true)
            .await
            .unwrap();
        db.increment_downloads(FEED, "contoso.utils", &v)
            .await
            .unwrap();
        db.add_file(&PackageFile {
            lower_id: "contoso.utils".into(),
            normalized_version: "1.2.3".into(),
            name: "disk.iso".into(),
            sha256: "ab".repeat(32),
            size: 3,
            uploaded: Utc::now(),
            downloads: 0,
        })
        .await
        .unwrap();

        push(&storage, &db, FEED, true, &overwriting())
            .await
            .unwrap();

        let all = db.find_all_versions(FEED, "contoso.utils").await.unwrap();
        assert_eq!(all.len(), 1);
        let fv = &all[0];
        assert!(
            fv.package.has_readme,
            "the new build's metadata is recorded"
        );
        assert!(
            !fv.package.enabled,
            "a push must not undo an admin's disable"
        );
        assert!(!fv.package.listed);
        assert!(fv.pinned);
        assert_eq!(fv.package.downloads, 1);
        assert_eq!(db.files_for("contoso.utils", &v).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn new_bytes_wait_for_approval_again_and_the_same_bytes_do_not() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        let gated = IndexOptions {
            pending: true,
            ..overwriting()
        };
        push(&storage, &db, FEED, false, &gated).await.unwrap();
        let v = v123();
        db.approve_membership(FEED, "contoso.utils", &v)
            .await
            .unwrap();

        let same = push(&storage, &db, FEED, false, &gated).await.unwrap();
        assert!(!same.pending);
        assert!(db.is_servable(FEED, "contoso.utils", &v).await.unwrap());

        let rebuilt = push(&storage, &db, FEED, true, &gated).await.unwrap();
        assert!(rebuilt.pending);
        assert!(!db.is_servable(FEED, "contoso.utils", &v).await.unwrap());
    }

    /// Storage whose sidecar writes fail on demand, to fail an overwrite after
    /// its payload has already been replaced.
    struct FailingSidecars {
        inner: FilesystemStorage,
        fail: AtomicBool,
    }

    #[async_trait::async_trait]
    impl PackageStorage for FailingSidecars {
        async fn store_package(&self, id: &str, version: &str, temp: PathBuf) -> Result<u64> {
            self.inner.store_package(id, version, temp).await
        }
        async fn get_package(&self, id: &str, version: &str) -> Result<PackageContent> {
            self.inner.get_package(id, version).await
        }
        async fn package_exists(&self, id: &str, version: &str) -> bool {
            self.inner.package_exists(id, version).await
        }
        async fn store_symbol_package(&self, id: &str, v: &str, temp: PathBuf) -> Result<u64> {
            self.inner.store_symbol_package(id, v, temp).await
        }
        async fn store_symbol(&self, key: &str, filename: &str, bytes: &[u8]) -> Result<()> {
            self.inner.store_symbol(key, filename, bytes).await
        }
        async fn store_symbol_file(&self, key: &str, name: &str, temp: PathBuf) -> Result<()> {
            self.inner.store_symbol_file(key, name, temp).await
        }
        async fn get_symbol(&self, key: &str, filename: &str) -> Result<PackageContent> {
            self.inner.get_symbol(key, filename).await
        }
        async fn delete_symbol(&self, key: &str, filename: &str) -> Result<()> {
            self.inner.delete_symbol(key, filename).await
        }
        async fn store_aux(&self, id: &str, v: &str, kind: AuxFile, bytes: &[u8]) -> Result<()> {
            if self.fail.load(Ordering::SeqCst) {
                return Err(Error::Storage("disk full".into()));
            }
            self.inner.store_aux(id, v, kind, bytes).await
        }
        async fn get_aux(&self, id: &str, version: &str, kind: AuxFile) -> Result<Vec<u8>> {
            self.inner.get_aux(id, version, kind).await
        }
        async fn aux_content(
            &self,
            id: &str,
            version: &str,
            kind: AuxFile,
        ) -> Result<PackageContent> {
            self.inner.aux_content(id, version, kind).await
        }
        async fn delete(&self, id: &str, version: &str) -> Result<()> {
            self.inner.delete(id, version).await
        }
        async fn store_blob(&self, sha256_hex: &str, temp: PathBuf) -> Result<u64> {
            self.inner.store_blob(sha256_hex, temp).await
        }
        async fn get_blob(&self, sha256_hex: &str) -> Result<PackageContent> {
            self.inner.get_blob(sha256_hex).await
        }
        async fn delete_blob(&self, sha256_hex: &str) -> Result<()> {
            self.inner.delete_blob(sha256_hex).await
        }
    }

    #[tokio::test]
    async fn a_failed_overwrite_puts_the_previous_build_back() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FailingSidecars {
            inner: FilesystemStorage::new(store_dir.path()).await.unwrap(),
            fail: AtomicBool::new(false),
        };
        let db = SqliteDatabase::in_memory().await.unwrap();
        push(&storage, &db, FEED, false, &overwriting())
            .await
            .unwrap();
        let before = stored_bytes(&storage.inner).await;
        let v = v123();
        let hash = db
            .get_package_data("contoso.utils", &v)
            .await
            .unwrap()
            .unwrap()
            .package_hash;

        storage.fail.store(true, Ordering::SeqCst);
        push(&storage, &db, FEED, true, &overwriting())
            .await
            .unwrap_err();

        // Still in the feed, and the rows still describe the bytes served.
        let found = db.find(FEED, "contoso.utils", &v).await.unwrap().unwrap();
        assert_eq!(found.package_hash, hash);
        assert!(!found.has_readme);
        assert_eq!(stored_bytes(&storage.inner).await, before);
    }

    #[tokio::test]
    async fn the_remains_of_a_failed_purge_are_replaced_not_adopted() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        push(&storage, &db, FEED, false, &IndexOptions::default())
            .await
            .unwrap();
        // A purge that removed the membership and the payload, then failed.
        let v = v123();
        db.remove_membership(FEED, "contoso.utils", &v)
            .await
            .unwrap();
        storage.delete("contoso.utils", "1.2.3").await.unwrap();

        // Different bytes are not refused in every feed for ever...
        push(&storage, &db, "other", true, &IndexOptions::default())
            .await
            .unwrap();
        // ...and what is advertised is what is stored.
        let found = db
            .find("other", "contoso.utils", &v)
            .await
            .unwrap()
            .unwrap();
        assert!(found.has_readme);
        assert!(storage.package_exists("contoso.utils", "1.2.3").await);
    }

    #[tokio::test]
    async fn an_identical_push_restores_a_missing_payload() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        push(&storage, &db, "dev", false, &IndexOptions::default())
            .await
            .unwrap();
        storage.delete("contoso.utils", "1.2.3").await.unwrap();

        push(&storage, &db, "stable", false, &IndexOptions::default())
            .await
            .unwrap();
        assert!(storage.package_exists("contoso.utils", "1.2.3").await);
        assert_eq!(db.feed_count("contoso.utils", &v123()).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn different_bytes_under_a_stored_version_are_a_conflict() {
        let store_dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(store_dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        push(&storage, &db, "dev", false, &IndexOptions::default())
            .await
            .unwrap();
        // Not the benign `PackageAlreadyExists` a lost race gets, which a
        // mirror or a migration would skip without a word.
        let err = push(&storage, &db, "stable", true, &IndexOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Conflict(_)), "{err:?}");
        assert_eq!(err.status(), axum::http::StatusCode::CONFLICT);
    }
}
