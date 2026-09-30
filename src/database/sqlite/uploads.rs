//! Resumable uploads in progress: `uploads`.

use chrono::{DateTime, Utc};
use sqlx::sqlite::SqliteRow;
use sqlx::Row;

use crate::database::{canonical_id, UploadSession};
use crate::error::Result;

use super::{parse_time, SqliteDatabase};

pub(super) async fn create_upload(db: &SqliteDatabase, u: &UploadSession) -> Result<()> {
    sqlx::query(
        "INSERT INTO uploads (id, feed, lower_id, normalized_version, name, length, \
                              received, expected_sha256, created, expires) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
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
) -> Result<()> {
    sqlx::query("UPDATE uploads SET received = ?2 WHERE id = ?1")
        .bind(id)
        .bind(received as i64)
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
    })
}
