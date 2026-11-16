//! Resumable uploads in progress: `uploads`.

use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;
use sqlx::Row;

use crate::database::{canonical_id, UploadSession};
use crate::error::{Error, Result};

use super::{parse_time, SqliteDatabase};

pub(super) async fn create_upload(db: &SqliteDatabase, u: &UploadSession) -> Result<()> {
    sqlx::query(
        "INSERT INTO uploads (id, feed, lower_id, normalized_version, name, length, \
                              received, expected_sha256, created, expires, held_since) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )
    .bind(&u.id)
    .bind(&u.feed)
    .bind(canonical_id(&u.lower_id))
    .bind(&u.normalized_version)
    .bind(&u.name)
    .bind(u.length as i64)
    .bind(u.received as i64)
    .bind(&u.expected_sha256)
    .bind(Utc::now().to_rfc3339())
    .bind(u.expires.to_rfc3339())
    .bind(u.held_since.map(|t| t.to_rfc3339()))
    .execute(&db.pool)
    .await?;
    Ok(())
}

pub(super) async fn get_upload(db: &SqliteDatabase, id: &str) -> Result<Option<UploadSession>> {
    let row = sqlx::query("SELECT * FROM uploads WHERE id = ?1")
        .bind(id)
        .fetch_optional(&db.pool)
        .await?;
    row.as_ref().map(row_to_upload).transpose()
}

pub(super) async fn set_upload_received(
    db: &SqliteDatabase,
    id: &str,
    received: u64,
    held_since: Option<DateTime<Utc>>,
) -> Result<()> {
    sqlx::query("UPDATE uploads SET received = ?2, held_since = ?3 WHERE id = ?1")
        .bind(id)
        .bind(received as i64)
        .bind(held_since.map(|t| t.to_rfc3339()))
        .execute(&db.pool)
        .await?;
    Ok(())
}

pub(super) async fn set_upload_held(
    db: &SqliteDatabase,
    id: &str,
    since: Option<DateTime<Utc>>,
) -> Result<()> {
    sqlx::query("UPDATE uploads SET held_since = ?2 WHERE id = ?1")
        .bind(id)
        .bind(since.map(|t| t.to_rfc3339()))
        .execute(&db.pool)
        .await?;
    Ok(())
}

pub(super) async fn delete_upload(db: &SqliteDatabase, id: &str) -> Result<()> {
    sqlx::query("DELETE FROM uploads WHERE id = ?1")
        .bind(id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

pub(super) async fn outstanding_upload_bytes(
    db: &SqliteDatabase,
    now: DateTime<Utc>,
    held_after: DateTime<Utc>,
) -> Result<u64> {
    // An expired upload is as good as gone: no request may write to it. One
    // that has not held its space since `held_after` must ask for it again,
    // and one with no hold (`NULL`) is being written by a request that holds
    // the space itself.
    let left: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(MAX(length - received, 0)), 0) FROM uploads \
         WHERE expires >= ?1 AND held_since >= ?2",
    )
    .bind(now.to_rfc3339())
    .bind(held_after.to_rfc3339())
    .fetch_one(&db.pool)
    .await?;
    Ok(left.max(0) as u64)
}

pub(super) async fn feeds_upload_bytes(
    db: &SqliteDatabase,
    feeds: &[String],
    now: DateTime<Utc>,
    held_after: DateTime<Utc>,
) -> Result<u64> {
    // The same uploads as above, of some feeds. The list goes in as one JSON
    // array, so that a group of any size is one statement and one query.
    let feeds = serde_json::to_string(feeds)
        .map_err(|e| Error::Other(anyhow::anyhow!("could not encode the feed list: {e}")))?;
    let left: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(MAX(length - received, 0)), 0) FROM uploads \
         WHERE feed IN (SELECT value FROM json_each(?1)) \
           AND expires >= ?2 AND held_since >= ?3",
    )
    .bind(feeds)
    .bind(now.to_rfc3339())
    .bind(held_after.to_rfc3339())
    .fetch_one(&db.pool)
    .await?;
    Ok(left.max(0) as u64)
}

pub(super) async fn open_uploads(
    db: &SqliteDatabase,
    feed: &str,
    now: DateTime<Utc>,
) -> Result<u64> {
    let open: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM uploads WHERE feed = ?1 AND expires >= ?2")
            .bind(feed)
            .bind(now.to_rfc3339())
            .fetch_one(&db.pool)
            .await?;
    Ok(open.max(0) as u64)
}

pub(super) async fn expired_uploads(
    db: &SqliteDatabase,
    now: DateTime<Utc>,
) -> Result<Vec<UploadSession>> {
    // Stored as RFC 3339 in UTC, so the times compare as text.
    let rows = sqlx::query("SELECT * FROM uploads WHERE expires < ?1")
        .bind(now.to_rfc3339())
        .fetch_all(&db.pool)
        .await?;
    rows.iter().map(row_to_upload).collect()
}

fn row_to_upload(row: &SqliteRow) -> Result<UploadSession> {
    Ok(UploadSession {
        id: row.try_get("id")?,
        feed: row.try_get("feed")?,
        lower_id: row.try_get("lower_id")?,
        normalized_version: row.try_get("normalized_version")?,
        name: row.try_get("name")?,
        length: row.try_get::<i64, _>("length")?.max(0) as u64,
        received: row.try_get::<i64, _>("received")?.max(0) as u64,
        expected_sha256: row.try_get("expected_sha256")?,
        expires: parse_time(&row.try_get::<String, _>("expires")?, "expires")?,
        held_since: row
            .try_get::<Option<String>, _>("held_since")?
            .map(|t| parse_time(&t, "held_since"))
            .transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use chrono::Duration;

    fn upload(id: &str, feed: &str, length: u64, expires: chrono::DateTime<Utc>) -> UploadSession {
        UploadSession {
            id: id.into(),
            feed: feed.into(),
            lower_id: "res.pkg".into(),
            normalized_version: "1.0.0".into(),
            name: format!("{id}.wim"),
            length,
            received: 0,
            expected_sha256: None,
            expires,
            held_since: Some(Utc::now()),
        }
    }

    /// The hold cutoff: an upload held since then is not idle.
    fn recently() -> chrono::DateTime<Utc> {
        Utc::now() - Duration::hours(1)
    }

    #[tokio::test]
    async fn an_expired_upload_gives_back_its_space_and_its_slot() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let now = Utc::now();
        db.create_upload(&upload("live", FEED, 100, now + Duration::hours(1)))
            .await
            .unwrap();
        db.create_upload(&upload("gone", FEED, 1000, now - Duration::seconds(1)))
            .await
            .unwrap();
        assert_eq!(
            db.outstanding_upload_bytes(now, recently()).await.unwrap(),
            100
        );
        assert_eq!(db.open_uploads(FEED, now).await.unwrap(), 1);
        // Seen from after the first one expires too, nothing is left.
        let later = now + Duration::hours(2);
        assert_eq!(
            db.outstanding_upload_bytes(later, recently())
                .await
                .unwrap(),
            0
        );
        assert_eq!(db.open_uploads(FEED, later).await.unwrap(), 0);
        let expired: Vec<String> = db
            .expired_uploads(now)
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.id)
            .collect();
        assert_eq!(expired, ["gone"]);
    }

    #[tokio::test]
    async fn what_has_arrived_is_no_longer_promised() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let now = Utc::now();
        db.create_upload(&upload("half", FEED, 1000, now + Duration::hours(1)))
            .await
            .unwrap();
        db.set_upload_received("half", 400, Some(now))
            .await
            .unwrap();
        assert_eq!(
            db.outstanding_upload_bytes(now, recently()).await.unwrap(),
            600
        );
        // A row that somehow received more than its length promises nothing,
        // rather than taking off what others promised.
        db.set_upload_received("half", 1200, Some(now))
            .await
            .unwrap();
        assert_eq!(
            db.outstanding_upload_bytes(now, recently()).await.unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn slots_are_counted_per_feed_and_space_over_every_feed() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let expires = Utc::now() + Duration::hours(1);
        db.create_upload(&upload("a", "team", 100, expires))
            .await
            .unwrap();
        db.create_upload(&upload("b", "team", 200, expires))
            .await
            .unwrap();
        db.create_upload(&upload("c", "prod", 300, expires))
            .await
            .unwrap();
        let now = Utc::now();
        assert_eq!(db.open_uploads("team", now).await.unwrap(), 2);
        assert_eq!(db.open_uploads("prod", now).await.unwrap(), 1);
        assert_eq!(db.open_uploads("other", now).await.unwrap(), 0);
        assert_eq!(
            db.outstanding_upload_bytes(now, recently()).await.unwrap(),
            600
        );
        // What each feed holds of that, for its share of the volume.
        let db = &db;
        let held = |feeds: &'static [&'static str]| {
            let feeds: Vec<String> = feeds.iter().map(|f| f.to_string()).collect();
            async move { db.feeds_upload_bytes(&feeds, now, recently()).await }
        };
        assert_eq!(held(&["team"]).await.unwrap(), 300);
        assert_eq!(held(&["prod"]).await.unwrap(), 300);
        assert_eq!(held(&["other"]).await.unwrap(), 0);
        assert_eq!(held(&["team", "prod", "other"]).await.unwrap(), 600);
        assert_eq!(held(&[]).await.unwrap(), 0);
        // An upload being written, or idle, holds none of its feed's share.
        db.set_upload_held("a", None).await.unwrap();
        db.set_upload_held("b", Some(now - Duration::hours(2)))
            .await
            .unwrap();
        assert_eq!(held(&["team"]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn an_idle_upload_holds_no_space_until_its_next_request() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let now = Utc::now();
        let expires = now + Duration::hours(72);
        let mut idle = upload("idle", FEED, 1000, expires);
        idle.held_since = Some(now - Duration::hours(2));
        db.create_upload(&idle).await.unwrap();
        db.create_upload(&upload("busy", FEED, 10, expires))
            .await
            .unwrap();
        // Still open, and still taking a slot, but holding nothing.
        assert_eq!(db.open_uploads(FEED, now).await.unwrap(), 2);
        assert_eq!(
            db.outstanding_upload_bytes(now, recently()).await.unwrap(),
            10
        );

        // A request that writes it gives up the row's hold; its end renews it.
        db.set_upload_held("busy", None).await.unwrap();
        assert_eq!(
            db.outstanding_upload_bytes(now, recently()).await.unwrap(),
            0
        );
        assert_eq!(
            db.get_upload("busy").await.unwrap().unwrap().held_since,
            None
        );
        db.set_upload_received("idle", 100, Some(Utc::now()))
            .await
            .unwrap();
        assert_eq!(
            db.outstanding_upload_bytes(now, recently()).await.unwrap(),
            900
        );
        let held = db.get_upload("idle").await.unwrap().unwrap().held_since;
        assert!(held.is_some_and(|t| t > recently()), "{held:?}");

        // Recording what arrived does not renew a hold by itself: the caller
        // says since when the upload holds, and an old time stays old.
        db.set_upload_received("idle", 100, idle.held_since)
            .await
            .unwrap();
        assert_eq!(
            db.outstanding_upload_bytes(now, recently()).await.unwrap(),
            0
        );
    }
}
