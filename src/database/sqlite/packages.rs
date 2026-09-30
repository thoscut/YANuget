//! The global data of a version, shared by every feed: `packages`, and the
//! `package_tags` index kept in step with it.

use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};

use crate::database::{canonical_id, FeedVersion, Membership};
use crate::error::{Error, Result};
use crate::models::{DependencyGroup, Package, PackageType};
use crate::version::NuGetVersion;

use super::memberships::insert_membership;
use super::{from_json, json, SqliteDatabase};

/// Fill `package_tags` for one version from its JSON tag array (`?3`),
/// lower-cased. Tags are capped when a package is pushed; the `LIMIT`-like
/// `key` and `substr` bounds apply the same caps to rows stored before that.
macro_rules! insert_tags {
    () => {
        "INSERT OR IGNORE INTO package_tags (lower_id, normalized_version, tag) \
         SELECT ?1, ?2, lower(substr(trim(je.value), 1, 64)) FROM json_each(?3) je \
         WHERE trim(je.value) <> '' AND je.key < 64"
    };
}

/// The `packages` insert, completed by an `ON CONFLICT` clause (`$conflict`).
/// Bound by [`bind_package`], in the column order written here.
macro_rules! package_insert {
    ($conflict:literal) => {
        concat!(
            "INSERT INTO packages ( \
                id, lower_id, normalized_version, original_version, \
                version_major, version_minor, version_patch, version_revision, \
                is_prerelease, is_semver2, listed, enabled, \
                authors, description, icon_url, license_url, license_expression, \
                project_url, repository_url, repository_type, min_client_version, \
                release_notes, language, title, summary, tags, \
                has_readme, has_embedded_icon, is_development_dependency, \
                require_license_acceptance, \
                package_size, package_hash, package_hash_algorithm, \
                published, downloads, package_types, dependencies \
            ) VALUES ( \
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, \
                ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, \
                ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, \
                ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37 \
            ) ",
            $conflict
        )
    };
}

pub(super) async fn upsert_package_data(db: &SqliteDatabase, p: &Package) -> Result<bool> {
    // The row and its tag index in one transaction: as two autocommit
    // statements, a failed tag insert left a package the tag filter and
    // the tag cloud never saw, and nothing ever retried it.
    let mut tx = db.write_tx().await?;
    let inserted = insert_package(&mut tx, p).await?;
    tx.commit().await?;
    Ok(inserted)
}

pub(super) async fn add_version(db: &SqliteDatabase, p: &Package, m: &Membership) -> Result<bool> {
    let mut tx = db.write_tx().await?;
    let inserted = insert_package(&mut tx, p).await?;
    insert_membership(&mut tx, m).await?;
    tx.commit().await?;
    Ok(inserted)
}

pub(super) async fn replace_version(
    db: &SqliteDatabase,
    p: &Package,
    m: &Membership,
) -> Result<()> {
    let mut tx = db.write_tx().await?;
    bind_package(
        sqlx::query(package_insert!(
            "ON CONFLICT(lower_id, normalized_version) DO UPDATE SET \
         id = excluded.id, original_version = excluded.original_version, \
         authors = excluded.authors, description = excluded.description, \
         icon_url = excluded.icon_url, license_url = excluded.license_url, \
         license_expression = excluded.license_expression, \
         project_url = excluded.project_url, repository_url = excluded.repository_url, \
         repository_type = excluded.repository_type, \
         min_client_version = excluded.min_client_version, \
         release_notes = excluded.release_notes, language = excluded.language, \
         title = excluded.title, summary = excluded.summary, tags = excluded.tags, \
         has_readme = excluded.has_readme, has_embedded_icon = excluded.has_embedded_icon, \
         is_development_dependency = excluded.is_development_dependency, \
         require_license_acceptance = excluded.require_license_acceptance, \
         package_size = excluded.package_size, package_hash = excluded.package_hash, \
         package_hash_algorithm = excluded.package_hash_algorithm, \
         published = excluded.published, package_types = excluded.package_types, \
         dependencies = excluded.dependencies"
        )),
        p,
    )?
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM package_tags WHERE lower_id = ?1 AND normalized_version = ?2")
        .bind(p.lower_id())
        .bind(p.normalized_version())
        .execute(&mut *tx)
        .await?;
    insert_tags(&mut tx, p).await?;
    // The membership's own state is the caller's to decide; its download
    // counter is history and survives the replacement.
    sqlx::query(
        r#"INSERT INTO feed_packages
               (feed, lower_id, normalized_version, listed, enabled, pending,
                flagged, flag_reason, added, downloads, pinned)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)
           ON CONFLICT(feed, lower_id, normalized_version) DO UPDATE SET
               listed = excluded.listed, enabled = excluded.enabled,
               pending = excluded.pending, flagged = excluded.flagged,
               flag_reason = excluded.flag_reason, pinned = excluded.pinned"#,
    )
    .bind(&m.feed)
    .bind(canonical_id(&m.lower_id))
    .bind(&m.normalized_version)
    .bind(i64::from(m.listed))
    .bind(i64::from(m.enabled))
    .bind(i64::from(m.pending))
    .bind(i64::from(m.flagged))
    .bind(&m.flag_reason)
    .bind(Utc::now().to_rfc3339())
    .bind(i64::from(m.pinned))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn orphaned_versions(db: &SqliteDatabase, limit: i64) -> Result<Vec<Package>> {
    let rows = sqlx::query(
        "SELECT * FROM packages p WHERE NOT EXISTS ( \
             SELECT 1 FROM feed_packages fp \
             WHERE fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version) \
         LIMIT ?1",
    )
    .bind(limit.max(0))
    .fetch_all(&db.pool)
    .await?;
    rows.iter().map(row_to_package).collect()
}

pub(super) async fn package_data_exists(
    db: &SqliteDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let row = sqlx::query(
        "SELECT 1 FROM packages WHERE lower_id = ?1 AND normalized_version = ?2 LIMIT 1",
    )
    .bind(canonical_id(id))
    .bind(version.normalized())
    .fetch_optional(&db.pool)
    .await?;
    Ok(row.is_some())
}

pub(super) async fn get_package_data(
    db: &SqliteDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<Option<Package>> {
    let row = sqlx::query("SELECT * FROM packages WHERE lower_id = ?1 AND normalized_version = ?2")
        .bind(canonical_id(id))
        .bind(version.normalized())
        .fetch_optional(&db.pool)
        .await?;
    row.as_ref().map(row_to_package).transpose()
}

pub(super) async fn delete_package_data(
    db: &SqliteDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let lower = canonical_id(id);
    let normalized = version.normalized();
    // All or nothing. As separate statements, a failure after the
    // memberships went left a `packages` row that no feed held and nothing
    // would ever revisit; a later push then adopted its missing payload.
    let mut tx = db.write_tx().await?;
    //
    // The file blobs are the caller's to delete (see
    // `retention::purge_global_data`), which it does before this, while
    // these rows still say what they were.
    for statement in [
        "DELETE FROM feed_packages WHERE lower_id = ?1 AND normalized_version = ?2",
        "DELETE FROM package_tags WHERE lower_id = ?1 AND normalized_version = ?2",
        "DELETE FROM package_files WHERE lower_id = ?1 AND normalized_version = ?2",
    ] {
        sqlx::query(statement)
            .bind(&lower)
            .bind(&normalized)
            .execute(&mut *tx)
            .await?;
    }
    let result =
        sqlx::query("DELETE FROM packages WHERE lower_id = ?1 AND normalized_version = ?2")
            .bind(&lower)
            .bind(&normalized)
            .execute(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(result.rows_affected() > 0)
}

pub(super) async fn feed_count(
    db: &SqliteDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM feed_packages WHERE lower_id = ?1 AND normalized_version = ?2",
    )
    .bind(canonical_id(id))
    .bind(version.normalized())
    .fetch_one(&db.pool)
    .await?;
    Ok(n)
}

type SqliteQuery<'q> = sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>;

/// Bind a package to the parameters of [`package_insert!`].
fn bind_package<'q>(query: SqliteQuery<'q>, p: &'q Package) -> Result<SqliteQuery<'q>> {
    let (major, minor, patch, revision) = p.version.core();
    Ok(query
        .bind(&p.id)
        .bind(p.lower_id())
        .bind(p.normalized_version())
        .bind(p.version.original())
        .bind(major as i64)
        .bind(minor as i64)
        .bind(patch as i64)
        .bind(revision as i64)
        .bind(i64::from(p.is_prerelease()))
        .bind(i64::from(p.is_semver2))
        .bind(i64::from(p.listed))
        .bind(i64::from(p.enabled))
        .bind(json(&p.authors)?)
        .bind(&p.description)
        .bind(&p.icon_url)
        .bind(&p.license_url)
        .bind(&p.license_expression)
        .bind(&p.project_url)
        .bind(&p.repository_url)
        .bind(&p.repository_type)
        .bind(&p.min_client_version)
        .bind(&p.release_notes)
        .bind(&p.language)
        .bind(&p.title)
        .bind(&p.summary)
        .bind(json(&p.tags)?)
        .bind(i64::from(p.has_readme))
        .bind(i64::from(p.has_embedded_icon))
        .bind(i64::from(p.is_development_dependency))
        .bind(i64::from(p.require_license_acceptance))
        .bind(p.package_size as i64)
        .bind(&p.package_hash)
        .bind(&p.package_hash_algorithm)
        .bind(p.published.to_rfc3339())
        .bind(p.downloads as i64)
        .bind(json(&p.package_types)?)
        .bind(json(&p.dependencies)?))
}

/// Insert a version's global data and its tag index if absent. Returns
/// whether a row was written.
async fn insert_package(conn: &mut SqliteConnection, p: &Package) -> Result<bool> {
    let result = bind_package(
        sqlx::query(package_insert!(
            "ON CONFLICT(lower_id, normalized_version) DO NOTHING"
        )),
        p,
    )?
    .execute(&mut *conn)
    .await?;
    let inserted = result.rows_affected() > 0;
    if inserted {
        insert_tags(conn, p).await?;
    }
    Ok(inserted)
}

async fn insert_tags(conn: &mut SqliteConnection, p: &Package) -> Result<()> {
    sqlx::query(insert_tags!())
        .bind(p.lower_id())
        .bind(p.normalized_version())
        .bind(json(&p.tags)?)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Build a [`Package`] from a `packages` row, taking the listed/enabled flags
/// and download count from explicit arguments so it works for both global and
/// feed reads (each of which sources those values from a different column).
fn build_package(row: &SqliteRow, listed: bool, enabled: bool, downloads: u64) -> Result<Package> {
    let original_version: String = row.try_get("original_version")?;
    // Stored rows are read with the rules they were written under; see
    // `NuGetVersion::parse_stored`.
    let version = NuGetVersion::parse_stored(&original_version)
        .map_err(|e| Error::InvalidVersion(e.to_string()))?;
    let published: String = row.try_get("published")?;
    let published = DateTime::parse_from_rfc3339(&published)
        .map_err(|e| Error::Other(anyhow::anyhow!("bad published timestamp: {e}")))?
        .with_timezone(&Utc);

    let authors: Vec<String> = from_json(&row.try_get::<String, _>("authors")?)?;
    let tags: Vec<String> = from_json(&row.try_get::<String, _>("tags")?)?;
    let package_types: Vec<PackageType> = from_json(&row.try_get::<String, _>("package_types")?)?;
    let dependencies: Vec<DependencyGroup> = from_json(&row.try_get::<String, _>("dependencies")?)?;

    Ok(Package {
        id: row.try_get("id")?,
        version,
        listed,
        enabled,
        authors,
        description: row.try_get("description")?,
        icon_url: row.try_get("icon_url")?,
        license_url: row.try_get("license_url")?,
        license_expression: row.try_get("license_expression")?,
        project_url: row.try_get("project_url")?,
        repository_url: row.try_get("repository_url")?,
        repository_type: row.try_get("repository_type")?,
        min_client_version: row.try_get("min_client_version")?,
        release_notes: row.try_get("release_notes")?,
        language: row.try_get("language")?,
        title: row.try_get("title")?,
        summary: row.try_get("summary")?,
        tags,
        has_readme: row.try_get::<i64, _>("has_readme")? != 0,
        has_embedded_icon: row.try_get::<i64, _>("has_embedded_icon")? != 0,
        is_development_dependency: row.try_get::<i64, _>("is_development_dependency")? != 0,
        require_license_acceptance: row.try_get::<i64, _>("require_license_acceptance")? != 0,
        is_semver2: row.try_get::<i64, _>("is_semver2")? != 0,
        package_size: row.try_get::<i64, _>("package_size")? as u64,
        package_hash: row.try_get("package_hash")?,
        package_hash_algorithm: row.try_get("package_hash_algorithm")?,
        published,
        downloads,
        package_types,
        dependencies,
    })
}

/// A global `packages` row: listed/enabled/downloads come from its own columns.
fn row_to_package(row: &SqliteRow) -> Result<Package> {
    let listed = row.try_get::<i64, _>("listed")? != 0;
    let enabled = row.try_get::<i64, _>("enabled")? != 0;
    let downloads = row.try_get::<i64, _>("downloads")? as u64;
    build_package(row, listed, enabled, downloads)
}

pub(super) fn row_to_feed_package(row: &SqliteRow) -> Result<FeedVersion> {
    let listed = row.try_get::<i64, _>("m_listed")? != 0;
    let enabled = row.try_get::<i64, _>("m_enabled")? != 0;
    let downloads = row.try_get::<i64, _>("m_downloads")? as u64;
    let package = build_package(row, listed, enabled, downloads)?;
    Ok(FeedVersion {
        package,
        pending: row.try_get::<i64, _>("m_pending")? != 0,
        flagged: row.try_get::<i64, _>("m_flagged")? != 0,
        flag_reason: row.try_get("m_flag_reason")?,
        pinned: row.try_get::<i64, _>("m_pinned")? != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;

    #[tokio::test]
    async fn add_find_and_duplicate() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let p = sample("Contoso.Utils", "1.0.0");
        db.add_to_feed(FEED, &p).await.unwrap();

        let found = db
            .find(FEED, "contoso.utils", &p.version)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.id, "Contoso.Utils");
        assert_eq!(found.package_size, 25_000_000_000);
        assert_eq!(found.license_expression.as_deref(), Some("MIT"));

        // Duplicate membership in the same feed is rejected.
        let err = db.add_to_feed(FEED, &p).await.unwrap_err();
        assert!(matches!(err, Error::PackageAlreadyExists));
    }

    #[tokio::test]
    async fn ids_fold_like_storage_and_locks_do() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let p = sample("Kit.Pkg", "1.0.0");
        db.add_to_feed(FEED, &p).await.unwrap();
        // The Kelvin sign folds to `k` under Unicode rules only. Storage paths
        // and the version lock fold ASCII, so the database must not match it
        // either, or a delete removes rows the lock and the directory miss.
        let kelvin = "\u{212A}it.Pkg";
        assert!(!db.exists(FEED, kelvin, &p.version).await.unwrap());
        assert!(!db.delete_package_data(kelvin, &p.version).await.unwrap());
        assert!(db.exists(FEED, "KIT.pkg", &p.version).await.unwrap());
    }

    #[tokio::test]
    async fn version_in_multiple_feeds_is_not_duplicated() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let p = sample("Shared.Pkg", "1.0.0");
        db.add_to_feed("dev", &p).await.unwrap();
        // Same version into a second feed: a new membership, no new package row.
        db.add_to_feed("stable", &p).await.unwrap();

        assert!(db.exists("dev", "shared.pkg", &p.version).await.unwrap());
        assert!(db.exists("stable", "shared.pkg", &p.version).await.unwrap());
        assert!(!db.exists("other", "shared.pkg", &p.version).await.unwrap());
        assert_eq!(db.feed_count("shared.pkg", &p.version).await.unwrap(), 2);

        // Removing one membership leaves the other (and the global data) intact.
        assert!(db
            .remove_membership("dev", "shared.pkg", &p.version)
            .await
            .unwrap());
        assert_eq!(db.feed_count("shared.pkg", &p.version).await.unwrap(), 1);
        assert!(db
            .package_data_exists("shared.pkg", &p.version)
            .await
            .unwrap());
        assert!(db
            .find("stable", "shared.pkg", &p.version)
            .await
            .unwrap()
            .is_some());
        assert!(db
            .find("dev", "shared.pkg", &p.version)
            .await
            .unwrap()
            .is_none());
    }

    /// A version an older release accepted but today's parser refuses must
    /// not make its package unreadable: every listing, registration and
    /// restore of the package reads all its rows.
    #[tokio::test]
    async fn rows_stored_under_older_version_rules_stay_readable() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Legacy", "1.0.0"))
            .await
            .unwrap();
        for legacy in [
            "v1.1.0-01",
            "1.2.0+has space",
            &format!("1.3.0-{}", "a".repeat(80)),
        ] {
            assert!(
                NuGetVersion::parse(legacy).is_err(),
                "{legacy} parses strictly"
            );
            let mut p = sample("Legacy", "1.0.0");
            p.version = NuGetVersion::parse_stored(legacy).unwrap();
            db.add_to_feed(FEED, &p).await.unwrap();
        }

        let versions: Vec<String> = db
            .find_versions(FEED, "legacy", true)
            .await
            .unwrap()
            .iter()
            .map(|p| p.normalized_version())
            .collect();
        assert_eq!(versions.len(), 4, "{versions:?}");
        assert_eq!(versions[0], "1.0.0");
        assert_eq!(versions[1], "1.1.0-01");
        assert_eq!(versions[2], "1.2.0");
        // The same key it was stored under still finds it.
        let stored = NuGetVersion::parse_stored("1.1.0-01").unwrap();
        assert!(db.find(FEED, "legacy", &stored).await.unwrap().is_some());
    }
}
