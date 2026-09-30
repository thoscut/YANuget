//! Feed-scoped reads: what a feed holds, joining `feed_packages` to
//! `packages`.

use sqlx::Row;

use crate::database::{canonical_id, DatabaseStats, FeedVersion, VersionFootprint};
use crate::error::Result;
use crate::models::Package;
use crate::version::NuGetVersion;

use super::packages::row_to_feed_package;
use super::{placeholders, SqliteDatabase, ID_CHUNK};

pub(super) async fn find(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<Option<Package>> {
    let row = sqlx::query(concat!(
        feed_select!(),
        " WHERE fp.feed = ?1 AND fp.lower_id = ?2 \
           AND p.normalized_version = ?3 AND fp.enabled = 1 AND fp.pending = 0"
    ))
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .fetch_optional(&db.pool)
    .await?;
    row.as_ref()
        .map(|r| row_to_feed_package(r).map(|fv| fv.package))
        .transpose()
}

pub(super) async fn find_versions(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    include_unlisted: bool,
) -> Result<Vec<Package>> {
    find_versions_filtered(db, feed, &canonical_id(id), true, true, !include_unlisted).await
}

pub(super) async fn find_all_versions(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
) -> Result<Vec<FeedVersion>> {
    let rows = sqlx::query(concat!(
        feed_select!(),
        " WHERE fp.feed = ?1 AND fp.lower_id = ?2"
    ))
    .bind(feed)
    .bind(canonical_id(id))
    .fetch_all(&db.pool)
    .await?;
    let mut versions = rows
        .iter()
        .map(row_to_feed_package)
        .collect::<Result<Vec<_>>>()?;
    versions.sort_by(|a, b| a.package.version.cmp(&b.package.version));
    Ok(versions)
}

pub(super) async fn find_all_versions_of(
    db: &SqliteDatabase,
    feed: &str,
    ids: &[String],
) -> Result<Vec<FeedVersion>> {
    let mut versions = Vec::new();
    for chunk in ids.chunks(ID_CHUNK) {
        // The only interpolation is `?2,?3,…`, generated from a range.
        let sql = format!(
            concat!(
                feed_select!(),
                " WHERE fp.feed = ?1 AND fp.lower_id IN ({placeholders})"
            ),
            placeholders = placeholders(2, chunk.len())
        );
        let mut query = sqlx::query(&sql).bind(feed);
        for id in chunk {
            query = query.bind(canonical_id(id));
        }
        for row in query.fetch_all(&db.pool).await? {
            versions.push(row_to_feed_package(&row)?);
        }
    }
    versions.sort_by(|a, b| {
        a.package
            .lower_id()
            .cmp(&b.package.lower_id())
            .then_with(|| a.package.version.cmp(&b.package.version))
    });
    Ok(versions)
}

pub(super) async fn version_footprints(
    db: &SqliteDatabase,
    ids: &[String],
) -> Result<Vec<VersionFootprint>> {
    let mut out = Vec::new();
    for chunk in ids.chunks(ID_CHUNK) {
        let sql = format!(
            "SELECT fp.lower_id AS lower_id, fp.normalized_version AS normalized_version, \
                    COUNT(*) AS feeds, \
                    COALESCE((SELECT SUM(pf.size) FROM package_files pf \
                              WHERE pf.lower_id = fp.lower_id \
                                AND pf.normalized_version = fp.normalized_version), 0) \
                        AS file_bytes \
             FROM feed_packages fp WHERE fp.lower_id IN ({}) \
             GROUP BY fp.lower_id, fp.normalized_version",
            placeholders(1, chunk.len())
        );
        let mut query = sqlx::query(&sql);
        for id in chunk {
            query = query.bind(canonical_id(id));
        }
        for row in query.fetch_all(&db.pool).await? {
            out.push(VersionFootprint {
                lower_id: row.try_get("lower_id")?,
                normalized_version: row.try_get("normalized_version")?,
                feeds: row.try_get("feeds")?,
                file_bytes: row.try_get::<i64, _>("file_bytes")?.max(0) as u64,
            });
        }
    }
    Ok(out)
}

pub(super) async fn all_package_ids(db: &SqliteDatabase, feed: &str) -> Result<Vec<String>> {
    let rows = sqlx::query(
        r#"SELECT MAX(p.id) AS id FROM packages p JOIN feed_packages fp
               ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version
           WHERE fp.feed = ?1
           GROUP BY p.lower_id ORDER BY p.lower_id ASC"#,
    )
    .bind(feed)
    .fetch_all(&db.pool)
    .await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
}

pub(super) async fn stats(db: &SqliteDatabase, feed: &str) -> Result<DatabaseStats> {
    let row = sqlx::query(
        r#"SELECT
               COUNT(DISTINCT p.lower_id)       AS package_count,
               COUNT(*)                         AS version_count,
               COALESCE(SUM(fp.listed), 0)      AS listed_count,
               COALESCE(SUM(fp.downloads), 0)   AS total_downloads,
               COALESCE(SUM(p.package_size), 0) AS total_size
           FROM packages p JOIN feed_packages fp
               ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version
           WHERE fp.feed = ?1 AND fp.enabled = 1 AND fp.pending = 0"#,
    )
    .bind(feed)
    .fetch_one(&db.pool)
    .await?;
    let symbol_count: i64 = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM symbols s WHERE EXISTS (
               SELECT 1 FROM feed_packages fp
               WHERE fp.feed = ?1 AND fp.lower_id = s.lower_id
                 AND fp.normalized_version = s.normalized_version)"#,
    )
    .bind(feed)
    .fetch_one(&db.pool)
    .await?;
    // The size counts each blob once: identical files share their bytes,
    // so this is what the feed's files take on disk.
    let files = sqlx::query(
        "SELECT COUNT(*) AS n, \
                COALESCE((SELECT SUM(size) FROM (SELECT DISTINCT pf2.sha256, pf2.size \
                    FROM package_files pf2 JOIN feed_packages fp2 \
                      ON fp2.lower_id = pf2.lower_id \
                     AND fp2.normalized_version = pf2.normalized_version \
                    WHERE fp2.feed = ?1 AND fp2.enabled = 1 AND fp2.pending = 0)), 0) AS bytes \
         FROM package_files pf JOIN feed_packages fp \
           ON fp.lower_id = pf.lower_id AND fp.normalized_version = pf.normalized_version \
         WHERE fp.feed = ?1 AND fp.enabled = 1 AND fp.pending = 0",
    )
    .bind(feed)
    .fetch_one(&db.pool)
    .await?;
    Ok(DatabaseStats {
        package_count: row.try_get("package_count")?,
        version_count: row.try_get("version_count")?,
        listed_count: row.try_get("listed_count")?,
        total_downloads: row.try_get("total_downloads")?,
        total_size: row.try_get("total_size")?,
        file_count: files.try_get("n")?,
        file_bytes: files.try_get("bytes")?,
        symbol_count,
    })
}

pub(super) async fn recent_packages(
    db: &SqliteDatabase,
    feed: &str,
    limit: i64,
) -> Result<Vec<Package>> {
    let rows = sqlx::query(concat!(
        feed_select!(),
        " WHERE fp.feed = ?1 AND fp.enabled = 1 AND fp.pending = 0 \
         ORDER BY p.published DESC LIMIT ?2"
    ))
    .bind(feed)
    .bind(limit.max(0))
    .fetch_all(&db.pool)
    .await?;
    rows.iter()
        .map(|r| row_to_feed_package(r).map(|fv| fv.package))
        .collect()
}

async fn find_versions_filtered(
    db: &SqliteDatabase,
    feed: &str,
    lower_id: &str,
    include_prerelease: bool,
    include_semver2: bool,
    listed_only: bool,
) -> Result<Vec<Package>> {
    // Public listings never include disabled or pending memberships.
    let rows = sqlx::query(concat!(
        feed_select!(),
        " WHERE fp.feed = ?1 AND fp.lower_id = ?2 \
           AND fp.enabled = 1 AND fp.pending = 0 \
           AND (?3 = 1 OR fp.listed = 1) \
           AND (?4 = 1 OR p.is_prerelease = 0) \
           AND (?5 = 1 OR p.is_semver2 = 0)"
    ))
    .bind(feed)
    .bind(lower_id)
    .bind(i64::from(!listed_only))
    .bind(i64::from(include_prerelease))
    .bind(i64::from(include_semver2))
    .fetch_all(&db.pool)
    .await?;

    let mut packages = rows
        .iter()
        .map(|r| row_to_feed_package(r).map(|fv| fv.package))
        .collect::<Result<Vec<_>>>()?;
    packages.sort_by(|a, b| a.version.cmp(&b.version));
    Ok(packages)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;

    #[tokio::test]
    async fn feeds_are_isolated_in_listings() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed("a", &sample("Only.A", "1.0.0"))
            .await
            .unwrap();
        db.add_to_feed("b", &sample("Only.B", "1.0.0"))
            .await
            .unwrap();

        assert_eq!(db.all_package_ids("a").await.unwrap(), vec!["Only.A"]);
        assert_eq!(db.all_package_ids("b").await.unwrap(), vec!["Only.B"]);
        let page_a = db.search("a", &SearchRequest::default()).await.unwrap();
        assert_eq!(page_a.total_hits, 1);
        assert_eq!(page_a.groups[0].latest().id, "Only.A");
    }

    #[tokio::test]
    async fn versions_are_sorted_and_filtered() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        for v in ["1.0.0", "1.0.1", "2.0.0-rc.1", "0.9.0"] {
            db.add_to_feed(FEED, &sample("Pkg", v)).await.unwrap();
        }
        let all = db.find_versions(FEED, "pkg", false).await.unwrap();
        let versions: Vec<String> = all.iter().map(|p| p.normalized_version()).collect();
        assert_eq!(versions, vec!["0.9.0", "1.0.0", "1.0.1", "2.0.0-rc.1"]);

        // Hide an unlisted version.
        let v = NuGetVersion::parse("1.0.1").unwrap();
        assert!(db.set_listed(FEED, "pkg", &v, false).await.unwrap());
        let listed = db.find_versions(FEED, "pkg", false).await.unwrap();
        assert_eq!(listed.len(), 3);
        let with_unlisted = db.find_versions(FEED, "pkg", true).await.unwrap();
        assert_eq!(with_unlisted.len(), 4);
    }

    #[tokio::test]
    async fn all_package_ids_and_stats() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Alpha", "1.0.0"))
            .await
            .unwrap();
        db.add_to_feed(FEED, &sample("Alpha", "1.1.0"))
            .await
            .unwrap();
        db.add_to_feed(FEED, &sample("Beta", "2.0.0"))
            .await
            .unwrap();
        let v = NuGetVersion::parse("2.0.0").unwrap();
        db.increment_downloads(FEED, "beta", &v).await.unwrap();

        let ids = db.all_package_ids(FEED).await.unwrap();
        assert_eq!(ids, vec!["Alpha".to_string(), "Beta".to_string()]);

        let stats = db.stats(FEED).await.unwrap();
        assert_eq!(stats.package_count, 2);
        assert_eq!(stats.version_count, 3);
        assert_eq!(stats.listed_count, 3);
        assert_eq!(stats.total_downloads, 1);
        assert!(stats.total_size > 0);
        assert_eq!(stats.symbol_count, 0);
    }

    #[tokio::test]
    async fn recent_packages_orders_by_publish_time() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let mut older = sample("Old", "1.0.0");
        older.published = Utc::now() - chrono::Duration::days(5);
        let newer = sample("New", "1.0.0");
        db.add_to_feed(FEED, &older).await.unwrap();
        db.add_to_feed(FEED, &newer).await.unwrap();
        let recent = db.recent_packages(FEED, 10).await.unwrap();
        assert_eq!(recent[0].id, "New");
        assert_eq!(recent[1].id, "Old");
        assert_eq!(db.recent_packages(FEED, 1).await.unwrap().len(), 1);
    }
}
