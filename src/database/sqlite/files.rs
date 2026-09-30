//! Files attached to versions: `package_files`.

use sqlx::sqlite::SqliteRow;
use sqlx::Row;

use crate::database::{canonical_id, PackageFile};
use crate::error::{Error, Result};
use crate::version::NuGetVersion;

use super::{is_unique_violation, parse_time, SqliteDatabase};

pub(super) async fn add_file(db: &SqliteDatabase, f: &PackageFile) -> Result<()> {
    let result = sqlx::query(
        "INSERT INTO package_files \
             (lower_id, normalized_version, name, lower_name, sha256, size, uploaded, downloads) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)",
    )
    .bind(canonical_id(&f.lower_id))
    .bind(&f.normalized_version)
    .bind(&f.name)
    .bind(f.name.to_lowercase())
    .bind(&f.sha256)
    .bind(f.size as i64)
    .bind(f.uploaded.to_rfc3339())
    .execute(&db.pool)
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(e) if is_unique_violation(&e) => Err(Error::PackageAlreadyExists),
        Err(e) => Err(Error::Database(e)),
    }
}

pub(super) async fn files_for(
    db: &SqliteDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<Vec<PackageFile>> {
    let rows = sqlx::query(
        "SELECT * FROM package_files WHERE lower_id = ?1 AND normalized_version = ?2 \
         ORDER BY lower_name",
    )
    .bind(canonical_id(id))
    .bind(version.normalized())
    .fetch_all(&db.pool)
    .await?;
    rows.iter().map(row_to_file).collect()
}

pub(super) async fn files_for_id(db: &SqliteDatabase, id: &str) -> Result<Vec<PackageFile>> {
    let rows = sqlx::query(
        "SELECT * FROM package_files WHERE lower_id = ?1 \
         ORDER BY normalized_version, lower_name",
    )
    .bind(canonical_id(id))
    .fetch_all(&db.pool)
    .await?;
    rows.iter().map(row_to_file).collect()
}

pub(super) async fn get_file(
    db: &SqliteDatabase,
    id: &str,
    version: &NuGetVersion,
    name: &str,
) -> Result<Option<PackageFile>> {
    let row = sqlx::query(
        "SELECT * FROM package_files \
         WHERE lower_id = ?1 AND normalized_version = ?2 AND lower_name = ?3",
    )
    .bind(canonical_id(id))
    .bind(version.normalized())
    .bind(name.to_lowercase())
    .fetch_optional(&db.pool)
    .await?;
    row.as_ref().map(row_to_file).transpose()
}

pub(super) async fn delete_file(
    db: &SqliteDatabase,
    id: &str,
    version: &NuGetVersion,
    name: &str,
) -> Result<Option<PackageFile>> {
    let Some(file) = get_file(db, id, version, name).await? else {
        return Ok(None);
    };
    sqlx::query(
        "DELETE FROM package_files \
         WHERE lower_id = ?1 AND normalized_version = ?2 AND lower_name = ?3",
    )
    .bind(canonical_id(id))
    .bind(version.normalized())
    .bind(name.to_lowercase())
    .execute(&db.pool)
    .await?;
    Ok(Some(file))
}

pub(super) async fn blob_references(db: &SqliteDatabase, sha256: &str) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM package_files WHERE sha256 = ?1")
            .bind(sha256)
            .fetch_one(&db.pool)
            .await?,
    )
}

pub(super) async fn increment_file_downloads(
    db: &SqliteDatabase,
    id: &str,
    version: &NuGetVersion,
    name: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE package_files SET downloads = downloads + 1 \
         WHERE lower_id = ?1 AND normalized_version = ?2 AND lower_name = ?3",
    )
    .bind(canonical_id(id))
    .bind(version.normalized())
    .bind(name.to_lowercase())
    .execute(&db.pool)
    .await?;
    Ok(())
}

fn row_to_file(row: &SqliteRow) -> Result<PackageFile> {
    Ok(PackageFile {
        lower_id: row.try_get("lower_id")?,
        normalized_version: row.try_get("normalized_version")?,
        name: row.try_get("name")?,
        sha256: row.try_get("sha256")?,
        size: row.try_get::<i64, _>("size")?.max(0) as u64,
        uploaded: parse_time(&row.try_get::<String, _>("uploaded")?, "uploaded")?,
        downloads: row.try_get::<i64, _>("downloads")?.max(0) as u64,
    })
}
