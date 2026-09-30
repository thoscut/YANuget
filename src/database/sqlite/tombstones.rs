//! Versions deliberately removed from a feed: `tombstones`.

use chrono::Utc;

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
    sqlx::query(
        "INSERT OR IGNORE INTO tombstones (feed, lower_id, normalized_version, created) \
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .bind(Utc::now().to_rfc3339())
    .execute(&db.pool)
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
}
