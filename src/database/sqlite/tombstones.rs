//! Versions deliberately removed from a feed: `tombstones`.

use chrono::Utc;
use sqlx::SqliteConnection;

use crate::database::canonical_id;
use crate::error::Result;
use crate::version::NuGetVersion;

use super::SqliteDatabase;

pub(super) async fn add_tombstone(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<()> {
    let mut conn = db.pool.acquire().await?;
    insert_tombstone(&mut conn, feed, id, version).await
}

/// The tombstone insert, on a connection or inside a transaction.
pub(super) async fn insert_tombstone(
    conn: &mut SqliteConnection,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO tombstones (feed, lower_id, normalized_version, created) \
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .bind(Utc::now().to_rfc3339())
    .execute(conn)
    .await?;
    Ok(())
}

pub(super) async fn is_tombstoned(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let row = sqlx::query(
        "SELECT 1 FROM tombstones \
         WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .fetch_optional(&db.pool)
    .await?;
    Ok(row.is_some())
}

pub(super) async fn tombstoned_versions(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
) -> Result<std::collections::HashSet<String>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT normalized_version FROM tombstones WHERE feed = ?1 AND lower_id = ?2",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .fetch_all(&db.pool)
    .await?;
    Ok(rows.into_iter().collect())
}

pub(super) async fn clear_tombstone(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<()> {
    sqlx::query(
        "DELETE FROM tombstones WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .execute(&db.pool)
    .await?;
    Ok(())
}

pub(super) async fn clear_tombstones(db: &SqliteDatabase, feed: &str) -> Result<u64> {
    let result = sqlx::query("DELETE FROM tombstones WHERE feed = ?1")
        .bind(feed)
        .execute(&db.pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;

    #[tokio::test]
    async fn tombstones_are_per_feed_and_version() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let v1 = NuGetVersion::parse("1.0.0").unwrap();
        let v2 = NuGetVersion::parse("2.0.0").unwrap();
        assert!(!db.is_tombstoned(FEED, "Gone", &v1).await.unwrap());
        db.add_tombstone(FEED, "Gone", &v1).await.unwrap();
        // Idempotent, and matched case-insensitively.
        db.add_tombstone(FEED, "gone", &v1).await.unwrap();
        assert!(db.is_tombstoned(FEED, "GONE", &v1).await.unwrap());
        assert!(!db.is_tombstoned(FEED, "gone", &v2).await.unwrap());
        assert!(!db.is_tombstoned("other", "gone", &v1).await.unwrap());
        db.clear_tombstone(FEED, "Gone", &v1).await.unwrap();
        assert!(!db.is_tombstoned(FEED, "gone", &v1).await.unwrap());
    }

    #[tokio::test]
    async fn the_tombstones_of_a_package_are_read_at_once() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let v = |s: &str| NuGetVersion::parse(s).unwrap();
        for version in ["1.0.0", "2.0.0-rc.1"] {
            db.add_tombstone(FEED, "Gone", &v(version)).await.unwrap();
        }
        db.add_tombstone(FEED, "Other", &v("3.0.0")).await.unwrap();
        db.add_tombstone("other", "Gone", &v("4.0.0"))
            .await
            .unwrap();
        // Exactly the versions `is_tombstoned` says yes to, as it names them.
        let gone = db.tombstoned_versions(FEED, "GONE").await.unwrap();
        let expected = ["1.0.0", "2.0.0-rc.1"].map(String::from);
        assert_eq!(gone, expected.into_iter().collect());
        for version in ["1.0.0", "2.0.0-rc.1", "3.0.0", "4.0.0"] {
            assert_eq!(
                gone.contains(&v(version).normalized()),
                db.is_tombstoned(FEED, "gone", &v(version)).await.unwrap(),
                "{version}"
            );
        }
        assert!(db
            .tombstoned_versions(FEED, "never")
            .await
            .unwrap()
            .is_empty());
    }
}
