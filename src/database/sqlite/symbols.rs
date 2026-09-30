//! Symbol-file mappings, keyed by SSQP signature: `symbols`.

use sqlx::Row;

use crate::database::{canonical_id, SymbolKey, SymbolRef};
use crate::error::Result;
use crate::version::NuGetVersion;

use super::SqliteDatabase;

pub(super) async fn add_symbol(
    db: &SqliteDatabase,
    key: &str,
    filename: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let result = sqlx::query(
        // A claim, not an upsert: the first version to record a key keeps
        // it. Both halves of the key are chosen by the uploader, so letting
        // a later push repoint the row — even one for the same package id,
        // from another feed or version — would let it take over what a
        // debugger is served. The symbol pipeline decides what an existing
        // claim means; this statement only reports whether it made one.
        // One statement, so it is atomic without a transaction; the
        // caller serializes claims on a key with `locks::lock_symbol`.
        r#"INSERT INTO symbols (ssqp_key, filename, lower_id, normalized_version)
           VALUES (?1, ?2, ?3, ?4)
           ON CONFLICT(ssqp_key, filename) DO NOTHING"#,
    )
    .bind(key.to_uppercase())
    .bind(filename.to_lowercase())
    .bind(canonical_id(id))
    .bind(version.normalized())
    .execute(&db.pool)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub(super) async fn delete_symbol(db: &SqliteDatabase, key: &str, filename: &str) -> Result<()> {
    sqlx::query("DELETE FROM symbols WHERE ssqp_key = ?1 AND filename = ?2")
        .bind(key.to_uppercase())
        .bind(filename.to_lowercase())
        .execute(&db.pool)
        .await?;
    Ok(())
}

pub(super) async fn find_symbol(
    db: &SqliteDatabase,
    key: &str,
    filename: &str,
) -> Result<Option<SymbolRef>> {
    let row = sqlx::query(
        "SELECT lower_id, normalized_version FROM symbols
         WHERE ssqp_key = ?1 AND filename = ?2",
    )
    .bind(key.to_uppercase())
    .bind(filename.to_lowercase())
    .fetch_optional(&db.pool)
    .await?;
    Ok(row.map(|r| SymbolRef {
        lower_id: r.get::<String, _>("lower_id"),
        normalized_version: r.get::<String, _>("normalized_version"),
    }))
}

pub(super) async fn find_symbols(
    db: &SqliteDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<Vec<SymbolKey>> {
    let rows = sqlx::query(
        "SELECT ssqp_key, filename FROM symbols
         WHERE lower_id = ?1 AND normalized_version = ?2",
    )
    .bind(canonical_id(id))
    .bind(version.normalized())
    .fetch_all(&db.pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| SymbolKey {
            key: r.get::<String, _>("ssqp_key"),
            filename: r.get::<String, _>("filename"),
        })
        .collect())
}

pub(super) async fn delete_symbols(
    db: &SqliteDatabase,
    id: &str,
    version: &NuGetVersion,
) -> Result<u64> {
    let result = sqlx::query("DELETE FROM symbols WHERE lower_id = ?1 AND normalized_version = ?2")
        .bind(canonical_id(id))
        .bind(version.normalized())
        .execute(&db.pool)
        .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;

    #[tokio::test]
    async fn symbol_mappings_round_trip_and_clean_up() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Sym", "1.0.0")).await.unwrap();
        let v = NuGetVersion::parse("1.0.0").unwrap();

        assert!(db
            .add_symbol("ABCDEF01FFFFFFFF", "sym.pdb", "Sym", &v)
            .await
            .unwrap());
        // A claim is made once: a second one, even for the same package from
        // another version, neither succeeds nor moves the row.
        let other = NuGetVersion::parse("2.0.0").unwrap();
        assert!(!db
            .add_symbol("abcdef01ffffffff", "SYM.pdb", "sym", &other)
            .await
            .unwrap());
        // Lookup is case-insensitive on the key.
        let found = db.find_symbol("abcdef01ffffffff", "sym.pdb").await.unwrap();
        let found = found.unwrap();
        assert_eq!(found.lower_id, "sym");
        assert_eq!(found.normalized_version, "1.0.0");
        assert!(db.find_symbol("nope", "sym.pdb").await.unwrap().is_none());

        let owned = db.find_symbols("sym", &v).await.unwrap();
        assert_eq!(owned.len(), 1);
        assert_eq!(owned[0].filename, "sym.pdb");

        assert_eq!(db.delete_symbols("sym", &v).await.unwrap(), 1);
        assert!(db
            .find_symbol("ABCDEF01FFFFFFFF", "sym.pdb")
            .await
            .unwrap()
            .is_none());

        // A single mapping can be released, which makes the key claimable.
        assert!(db.add_symbol("K", "a.pdb", "Sym", &v).await.unwrap());
        db.delete_symbol("k", "A.pdb").await.unwrap();
        assert!(db.find_symbol("K", "a.pdb").await.unwrap().is_none());
        assert!(db.add_symbol("K", "a.pdb", "Sym", &other).await.unwrap());
    }
}
