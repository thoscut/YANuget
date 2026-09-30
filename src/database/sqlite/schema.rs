//! The schema and its migrations.

use sqlx::{Row, SqlitePool};

use crate::error::Result;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS packages (
    id                        TEXT    NOT NULL,
    lower_id                  TEXT    NOT NULL,
    normalized_version        TEXT    NOT NULL,
    original_version          TEXT    NOT NULL,
    version_major             INTEGER NOT NULL,
    version_minor             INTEGER NOT NULL,
    version_patch             INTEGER NOT NULL,
    version_revision          INTEGER NOT NULL,
    is_prerelease             INTEGER NOT NULL,
    is_semver2                INTEGER NOT NULL,
    listed                    INTEGER NOT NULL,
    enabled                   INTEGER NOT NULL DEFAULT 1,
    authors                   TEXT    NOT NULL,
    description               TEXT    NOT NULL,
    icon_url                  TEXT,
    license_url               TEXT,
    license_expression        TEXT,
    project_url               TEXT,
    repository_url            TEXT,
    repository_type           TEXT,
    min_client_version        TEXT,
    release_notes             TEXT,
    language                  TEXT,
    title                     TEXT,
    summary                   TEXT,
    tags                      TEXT    NOT NULL,
    has_readme                INTEGER NOT NULL,
    has_embedded_icon         INTEGER NOT NULL,
    is_development_dependency INTEGER NOT NULL,
    require_license_acceptance INTEGER NOT NULL DEFAULT 0,
    package_size              INTEGER NOT NULL,
    package_hash              TEXT    NOT NULL,
    package_hash_algorithm    TEXT    NOT NULL,
    published                 TEXT    NOT NULL,
    downloads                 INTEGER NOT NULL DEFAULT 0,
    package_types             TEXT    NOT NULL,
    dependencies              TEXT    NOT NULL,
    PRIMARY KEY (lower_id, normalized_version)
);
CREATE INDEX IF NOT EXISTS idx_packages_lower_id ON packages (lower_id);

CREATE TABLE IF NOT EXISTS feed_packages (
    feed               TEXT    NOT NULL,
    lower_id           TEXT    NOT NULL,
    normalized_version TEXT    NOT NULL,
    listed             INTEGER NOT NULL DEFAULT 1,
    enabled            INTEGER NOT NULL DEFAULT 1,
    pending            INTEGER NOT NULL DEFAULT 0,
    flagged            INTEGER NOT NULL DEFAULT 0,
    flag_reason        TEXT,
    added              TEXT    NOT NULL,
    downloads          INTEGER NOT NULL DEFAULT 0,
    pinned             INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (feed, lower_id, normalized_version)
);
-- Covers the search ranking: the (feed, lower_id) prefix scopes a feed and
-- supports GROUP BY lower_id, while including `downloads` lets SUM(downloads)
-- be read straight from the index instead of looking up each table row.
CREATE INDEX IF NOT EXISTS idx_feed_packages_rank
    ON feed_packages (feed, lower_id, downloads);
CREATE INDEX IF NOT EXISTS idx_feed_packages_pkg
    ON feed_packages (lower_id, normalized_version);

CREATE TABLE IF NOT EXISTS symbols (
    ssqp_key           TEXT NOT NULL,   -- upper-case {GUID}{age}
    filename           TEXT NOT NULL,   -- the .pdb file name, lower-cased
    lower_id           TEXT NOT NULL,   -- owning package, for cleanup
    normalized_version TEXT NOT NULL,
    PRIMARY KEY (ssqp_key, filename)
);
CREATE INDEX IF NOT EXISTS idx_symbols_owner
    ON symbols (lower_id, normalized_version);

-- One row per (version, lower-cased tag): what the tag filter and the tag
-- cloud read, as index lookups rather than a JSON scan of every package on
-- every page view. `packages.tags` keeps the tags as pushed, for display.
CREATE TABLE IF NOT EXISTS package_tags (
    lower_id           TEXT NOT NULL,
    normalized_version TEXT NOT NULL,
    tag                TEXT NOT NULL,
    PRIMARY KEY (lower_id, normalized_version, tag)
);
CREATE INDEX IF NOT EXISTS idx_package_tags_tag ON package_tags (tag, lower_id);

-- Files attached to versions. The bytes are a blob named by `sha256`; a row
-- is one reference to it, so a blob goes when its last row does.
CREATE TABLE IF NOT EXISTS package_files (
    lower_id           TEXT    NOT NULL,
    normalized_version TEXT    NOT NULL,
    name               TEXT    NOT NULL,
    lower_name         TEXT    NOT NULL,
    sha256             TEXT    NOT NULL,
    size               INTEGER NOT NULL,
    uploaded           TEXT    NOT NULL,
    downloads          INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (lower_id, normalized_version, lower_name)
);
CREATE INDEX IF NOT EXISTS idx_package_files_blob ON package_files (sha256);

-- Resumable uploads in progress; the partial bytes are `.uploads/{id}.part`.
CREATE TABLE IF NOT EXISTS uploads (
    id                 TEXT    PRIMARY KEY,
    feed               TEXT    NOT NULL,
    lower_id           TEXT    NOT NULL,
    normalized_version TEXT    NOT NULL,
    name               TEXT    NOT NULL,
    length             INTEGER NOT NULL,
    received           INTEGER NOT NULL DEFAULT 0,
    expected_sha256    TEXT,
    created            TEXT    NOT NULL,
    expires            TEXT    NOT NULL
);

-- Versions deliberately removed from a feed. Deleting drops the membership
-- row, which is all a read-through mirror checks, so without this a version
-- an admin removed came straight back on the next anonymous read.
CREATE TABLE IF NOT EXISTS tombstones (
    feed               TEXT    NOT NULL,
    lower_id           TEXT    NOT NULL,
    normalized_version TEXT    NOT NULL,
    created            TEXT    NOT NULL,
    PRIMARY KEY (feed, lower_id, normalized_version)
);
-- The search index: one row per version in an FTS5 table with the trigram
-- tokenizer, so a query is a substring match (what search has always done)
-- answered from an index instead of `LIKE '%q%'` over every version's full
-- description. The description is indexed only up to 4000 characters, as long
-- as nuget.org allows one to be; a longer one is stored and served in full.
--
-- FTS rows are addressed by rowid, and `packages` has no stable one (VACUUM
-- may renumber a table without an INTEGER PRIMARY KEY), so `search_keys` maps
-- each version to the rowid of its FTS row. Triggers keep both in step with
-- `packages`, whatever writes it.
CREATE TABLE IF NOT EXISTS search_keys (
    id                 INTEGER PRIMARY KEY,
    lower_id           TEXT NOT NULL,
    normalized_version TEXT NOT NULL,
    UNIQUE (lower_id, normalized_version)
);
CREATE VIRTUAL TABLE IF NOT EXISTS search_text USING fts5(
    lower_id, title, tags, description, tokenize = 'trigram'
);
CREATE TRIGGER IF NOT EXISTS packages_search_insert AFTER INSERT ON packages BEGIN
    INSERT OR IGNORE INTO search_keys (lower_id, normalized_version)
        VALUES (new.lower_id, new.normalized_version);
    DELETE FROM search_text WHERE rowid = (
        SELECT id FROM search_keys
        WHERE lower_id = new.lower_id AND normalized_version = new.normalized_version);
    INSERT INTO search_text (rowid, lower_id, title, tags, description)
        SELECT id, new.lower_id, IFNULL(new.title, ''),
               (SELECT IFNULL(group_concat(value, ' '), '') FROM json_each(new.tags)),
               substr(new.description, 1, 4000)
        FROM search_keys
        WHERE lower_id = new.lower_id AND normalized_version = new.normalized_version;
END;
CREATE TRIGGER IF NOT EXISTS packages_search_delete AFTER DELETE ON packages BEGIN
    DELETE FROM search_text WHERE rowid = (
        SELECT id FROM search_keys
        WHERE lower_id = old.lower_id AND normalized_version = old.normalized_version);
    DELETE FROM search_keys
        WHERE lower_id = old.lower_id AND normalized_version = old.normalized_version;
END;
CREATE TRIGGER IF NOT EXISTS packages_search_update
AFTER UPDATE OF lower_id, normalized_version, title, tags, description ON packages BEGIN
    DELETE FROM search_text WHERE rowid = (
        SELECT id FROM search_keys
        WHERE lower_id = old.lower_id AND normalized_version = old.normalized_version);
    DELETE FROM search_keys
        WHERE lower_id = old.lower_id AND normalized_version = old.normalized_version;
    INSERT OR IGNORE INTO search_keys (lower_id, normalized_version)
        VALUES (new.lower_id, new.normalized_version);
    INSERT INTO search_text (rowid, lower_id, title, tags, description)
        SELECT id, new.lower_id, IFNULL(new.title, ''),
               (SELECT IFNULL(group_concat(value, ' '), '') FROM json_each(new.tags)),
               substr(new.description, 1, 4000)
        FROM search_keys
        WHERE lower_id = new.lower_id AND normalized_version = new.normalized_version;
END;
"#;

/// Bring the schema up to date.
pub(super) async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::raw_sql(SCHEMA).execute(pool).await?;
    // Migrate databases created before the admin `enabled` column existed.
    ensure_column(pool, "packages", "enabled", "INTEGER NOT NULL DEFAULT 1").await?;
    // Databases predating the requireLicenseAcceptance passthrough. The
    // default is `false`, which is exactly what those rows were reported as.
    ensure_column(
        pool,
        "packages",
        "require_license_acceptance",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    // Databases predating pins: nothing was pinned.
    ensure_column(
        pool,
        "feed_packages",
        "pinned",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    // Drop the legacy (feed, lower_id) index, now subsumed by the wider
    // covering index `idx_feed_packages_rank` created above.
    sqlx::query("DROP INDEX IF EXISTS idx_feed_packages_feed")
        .execute(pool)
        .await?;

    // One-shot migration of single-feed databases created before feeds
    // existed: seed each existing package into the implicit `default` feed,
    // copying its state. Gated by `PRAGMA user_version` so it runs exactly
    // once on a pre-feeds database and never resurrects memberships that
    // were later deleted (which an "is feed_packages empty?" guard would).
    //
    // In one transaction, and with `OR IGNORE`, because neither the crash
    // nor the concurrency case is hypothetical: as three separate
    // autocommit statements, a crash between the insert and the version
    // bump left the rows written and the version unset, so every later
    // start re-ran an insert that now violated the primary key — the server
    // refused to start again, permanently, until someone set
    // `user_version` by hand. Two processes opening the same file (the
    // server and `yanuget migrate`) both read 0 and produced the same
    // failure. Together these make re-running the migration a no-op instead
    // of an error.
    //
    // `BEGIN IMMEDIATE` takes the write lock before `user_version` is read.
    // A deferred transaction only asks for it at the first write, and a
    // reader upgrading to a writer while another connection holds the lock
    // gets `SQLITE_BUSY` at once, without waiting out the busy timeout: the
    // second of two processes started together failed to open the
    // database. Taken up front, the loser waits, then reads the version the
    // winner wrote and has nothing left to do.
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let schema_version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *tx)
        .await?;
    if schema_version < 1 {
        sqlx::query(
            r#"INSERT OR IGNORE INTO feed_packages
                   (feed, lower_id, normalized_version, listed, enabled, pending,
                    flagged, flag_reason, added, downloads)
               SELECT 'default', lower_id, normalized_version, listed, enabled, 0, 0, NULL,
                      published, downloads
               FROM packages"#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("PRAGMA user_version = 1")
            .execute(&mut *tx)
            .await?;
    }
    // Version 2: index the tags of every package stored before
    // `package_tags` existed. Same transaction and `OR IGNORE`, so it is
    // exactly-once and a no-op when re-run.
    if schema_version < 2 {
        sqlx::query(
            "INSERT OR IGNORE INTO package_tags (lower_id, normalized_version, tag) \
             SELECT p.lower_id, p.normalized_version, lower(substr(trim(je.value), 1, 64)) \
             FROM packages p, json_each(p.tags) je \
             WHERE trim(je.value) <> '' AND je.key < 64",
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query("PRAGMA user_version = 2")
            .execute(&mut *tx)
            .await?;
    }
    // Version 3: lower-case the pre-release label of versions stored
    // before 0.5.0 made that part of the normalized form.
    if schema_version < 3 {
        migrate_prerelease_keys(&mut tx).await?;
        sqlx::query("PRAGMA user_version = 3")
            .execute(&mut *tx)
            .await?;
    }
    // Version 4: fill the search index for the versions stored before it
    // existed. Rebuilt from scratch, so it is a no-op to re-run.
    if schema_version < 4 {
        for step in [
            "DELETE FROM search_text",
            "DELETE FROM search_keys",
            "INSERT INTO search_keys (lower_id, normalized_version) \
             SELECT lower_id, normalized_version FROM packages",
            "INSERT INTO search_text (rowid, lower_id, title, tags, description) \
             SELECT k.id, p.lower_id, IFNULL(p.title, ''), \
                    (SELECT IFNULL(group_concat(value, ' '), '') FROM json_each(p.tags)), \
                    substr(p.description, 1, 4000) \
             FROM packages p JOIN search_keys k \
               ON k.lower_id = p.lower_id AND k.normalized_version = p.normalized_version",
            "PRAGMA user_version = 4",
        ] {
            sqlx::query(step).execute(&mut *tx).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Rewrite every stored normalized version to its lower-cased form: the
/// `user_version = 3` data migration.
///
/// 0.5.0 began lower-casing the pre-release label in
/// [`NuGetVersion::normalized`], but rows written earlier as `1.0.0-Beta` kept
/// their case. Listings still found them while every exact lookup (download,
/// delete, unlist, retention) bound the lower-cased key and missed, and a
/// re-push created a second row sharing the one lower-cased file on disk.
///
/// The core of a normalized version is digits and dots, so lower-casing the
/// whole string lower-cases exactly the label (which is ASCII by grammar).
/// Where both spellings exist, one `packages` row survives: the most recently
/// published, because its push is the one whose bytes were written to the
/// shared file last. Memberships of the same feed merge the cautious way:
/// withheld if either was, pinned or flagged if either was, downloads summed.
/// Symbol rows are keyed by their SSQP key, not the version, so they are only
/// re-pointed; attached files of the losing row that collide by name with the
/// survivor's are dropped (their blob is then referenced by nothing and stays
/// on disk, which is safer than guessing at a live one from here).
///
/// Runs in the caller's transaction, so it applies completely or not at all.
async fn migrate_prerelease_keys(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>) -> Result<()> {
    const STEPS: &[&str] = &[
        "DROP TABLE IF EXISTS temp.v3_packages",
        "DROP TABLE IF EXISTS temp.v3_memberships",
        // Every row of a version that has a mixed-case spelling, ranked so the
        // survivor is rank 1.
        "CREATE TEMP TABLE v3_packages AS \
         SELECT rowid AS rid, lower_id, normalized_version AS old_v, \
                ROW_NUMBER() OVER (PARTITION BY lower_id, lower(normalized_version) \
                                   ORDER BY published DESC, rowid DESC) AS rank \
         FROM packages \
         WHERE (lower_id, lower(normalized_version)) IN ( \
             SELECT lower_id, lower(normalized_version) FROM packages \
             WHERE normalized_version <> lower(normalized_version))",
        // The losers' tags describe metadata that is going away.
        "DELETE FROM package_tags WHERE (lower_id, normalized_version) IN ( \
             SELECT lower_id, old_v FROM v3_packages WHERE rank > 1)",
        "UPDATE OR IGNORE package_tags SET normalized_version = lower(normalized_version) \
         WHERE normalized_version <> lower(normalized_version)",
        "DELETE FROM package_tags WHERE normalized_version <> lower(normalized_version)",
        // The survivor's files first, so a name clash keeps its row.
        "UPDATE OR IGNORE package_files SET normalized_version = lower(normalized_version) \
         WHERE normalized_version <> lower(normalized_version) \
           AND (lower_id, normalized_version) IN ( \
               SELECT lower_id, old_v FROM v3_packages WHERE rank = 1)",
        "UPDATE OR IGNORE package_files SET normalized_version = lower(normalized_version) \
         WHERE normalized_version <> lower(normalized_version)",
        "DELETE FROM package_files WHERE normalized_version <> lower(normalized_version)",
        "DELETE FROM packages WHERE rowid IN (SELECT rid FROM v3_packages WHERE rank > 1)",
        "UPDATE packages SET normalized_version = lower(normalized_version) \
         WHERE normalized_version <> lower(normalized_version)",
        "CREATE TEMP TABLE v3_memberships AS \
         SELECT feed, lower_id, lower(normalized_version) AS nv, \
                MAX(listed) AS listed, MIN(enabled) AS enabled, MAX(pending) AS pending, \
                MAX(flagged) AS flagged, MAX(flag_reason) AS flag_reason, MIN(added) AS added, \
                SUM(downloads) AS downloads, MAX(pinned) AS pinned \
         FROM feed_packages \
         GROUP BY feed, lower_id, lower(normalized_version) \
         HAVING SUM(normalized_version <> lower(normalized_version)) > 0",
        "DELETE FROM feed_packages WHERE (feed, lower_id, lower(normalized_version)) IN ( \
             SELECT feed, lower_id, nv FROM v3_memberships)",
        "INSERT INTO feed_packages (feed, lower_id, normalized_version, listed, enabled, \
                                    pending, flagged, flag_reason, added, downloads, pinned) \
         SELECT feed, lower_id, nv, listed, enabled, pending, flagged, flag_reason, added, \
                downloads, pinned \
         FROM v3_memberships",
        "UPDATE symbols SET normalized_version = lower(normalized_version) \
         WHERE normalized_version <> lower(normalized_version)",
        "UPDATE uploads SET normalized_version = lower(normalized_version) \
         WHERE normalized_version <> lower(normalized_version)",
        "DROP TABLE temp.v3_packages",
        "DROP TABLE temp.v3_memberships",
    ];
    for step in STEPS {
        sqlx::query(step).execute(&mut **tx).await?;
    }
    Ok(())
}

/// Idempotently add a column to an existing table (SQLite has no
/// `ADD COLUMN IF NOT EXISTS`). Checks `PRAGMA table_info` first so re-running
/// migrations is a no-op.
///
/// Every name is `&'static str` on purpose: neither `PRAGMA` nor `ALTER TABLE`
/// takes bound parameters, so all three have to be interpolated. Requiring
/// literals keeps that safe by construction rather than by convention — a caller
/// cannot reach this with a request-supplied name without changing the signature
/// first. It is also exactly what sqlx 0.9's injection guard will want.
async fn ensure_column(
    pool: &SqlitePool,
    table: &'static str,
    column: &'static str,
    def: &'static str,
) -> Result<()> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await?;
    let exists = rows
        .iter()
        .any(|r| r.get::<String, _>("name").eq_ignore_ascii_case(column));
    if !exists {
        // Check-then-act, so two processes opening the same file — the server
        // and `yanuget migrate`, or two replicas on a shared volume — can both
        // decide to add it and the loser gets `duplicate column name`. SQLite
        // has no `ADD COLUMN IF NOT EXISTS`, so the race is absorbed here: the
        // column existing is precisely the outcome this function is asking for.
        if let Err(e) = sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {column} {def}"))
            .execute(pool)
            .await
        {
            if !is_duplicate_column(&e) {
                return Err(e.into());
            }
        }
    }
    Ok(())
}

/// Whether a failed `ALTER TABLE … ADD COLUMN` failed only because the column
/// was already added — by an earlier run, or by another process just now.
fn is_duplicate_column(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .map(|d| d.message().contains("duplicate column name"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::pool;
    use super::super::test_support::*;
    use super::ensure_column;

    #[tokio::test]
    async fn tags_stored_before_the_index_are_indexed_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db").to_string_lossy().into_owned();
        {
            let db = SqliteDatabase::connect(&path).await.unwrap();
            db.add_to_feed(FEED, &tagged("Old.Pkg", "1.0.0", &["Legacy", "tools"]))
                .await
                .unwrap();
            // As a database from before the tag index: no index rows, and the
            // schema version it had then.
            sqlx::query("DELETE FROM package_tags")
                .execute(pool(&db))
                .await
                .unwrap();
            sqlx::query("PRAGMA user_version = 1")
                .execute(pool(&db))
                .await
                .unwrap();
        }
        let db = SqliteDatabase::connect(&path).await.unwrap();
        let counts = db.tag_counts(FEED, 10).await.unwrap();
        assert_eq!(counts.len(), 2, "{counts:?}");
        assert!(counts.iter().any(|t| t.tag == "legacy"));
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(pool(&db))
            .await
            .unwrap();
        assert_eq!(version, 4);
    }

    /// Give every row of one version the key a pre-0.5.0 server wrote: the
    /// pre-release label as pushed rather than lower-cased.
    async fn respell(db: &SqliteDatabase, lower_id: &str, from: &str, to: &str) {
        for table in [
            "packages",
            "feed_packages",
            "package_tags",
            "package_files",
            "symbols",
        ] {
            sqlx::query(&format!(
                "UPDATE {table} SET normalized_version = ?3 \
                 WHERE lower_id = ?1 AND normalized_version = ?2"
            ))
            .bind(lower_id)
            .bind(from)
            .bind(to)
            .execute(pool(db))
            .await
            .unwrap();
        }
    }

    #[tokio::test]
    async fn pre_release_keys_from_before_0_5_are_lower_cased_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db").to_string_lossy().into_owned();
        let beta = NuGetVersion::parse("1.0.0-Beta").unwrap();
        let rc = NuGetVersion::parse("2.0.0-rc").unwrap();
        {
            let db = SqliteDatabase::connect(&path).await.unwrap();
            // A version only ever stored the old way, with a symbol and a file.
            db.add_to_feed(FEED, &tagged("Old.Pkg", "1.0.0-Beta", &["Legacy"]))
                .await
                .unwrap();
            db.add_symbol("ABCDEF01FFFFFFFF", "old.pdb", "Old.Pkg", &beta)
                .await
                .unwrap();
            db.add_file(&PackageFile {
                lower_id: "old.pkg".into(),
                normalized_version: "1.0.0-beta".into(),
                name: "image.iso".into(),
                sha256: "aa".repeat(32),
                size: 3,
                uploaded: Utc::now(),
                downloads: 0,
            })
            .await
            .unwrap();
            respell(&db, "old.pkg", "1.0.0-beta", "1.0.0-Beta").await;

            // A version stored the old way and then pushed again after the
            // upgrade: two rows, one file. The re-push wrote the file last.
            let mut before = sample("Dup.Pkg", "2.0.0-RC");
            before.published = Utc::now() - chrono::Duration::days(10);
            before.package_hash = "old-bytes".into();
            db.add_to_feed(FEED, &before).await.unwrap();
            db.add_to_feed("other", &before).await.unwrap();
            db.set_pinned(FEED, "dup.pkg", &rc, true).await.unwrap();
            for _ in 0..3 {
                db.increment_downloads(FEED, "dup.pkg", &rc).await.unwrap();
            }
            respell(&db, "dup.pkg", "2.0.0-rc", "2.0.0-RC").await;
            let mut after = sample("Dup.Pkg", "2.0.0-rc");
            after.package_hash = "new-bytes".into();
            db.add_to_feed(FEED, &after).await.unwrap();
            for _ in 0..2 {
                db.increment_downloads(FEED, "dup.pkg", &rc).await.unwrap();
            }
            // What exact lookups saw before the migration.
            assert!(db.find(FEED, "old.pkg", &beta).await.unwrap().is_none());

            sqlx::query("PRAGMA user_version = 2")
                .execute(pool(&db))
                .await
                .unwrap();
        }

        let db = SqliteDatabase::connect(&path).await.unwrap();
        let old = db.find(FEED, "old.pkg", &beta).await.unwrap().unwrap();
        assert_eq!(old.version.original(), "1.0.0-Beta", "display form is kept");
        assert_eq!(db.files_for("old.pkg", &beta).await.unwrap().len(), 1);
        let symbol = db
            .find_symbol("ABCDEF01FFFFFFFF", "old.pdb")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(symbol.normalized_version, "1.0.0-beta");
        let by_tag = SearchRequest {
            tag: Some("legacy".into()),
            ..Default::default()
        };
        assert_eq!(db.search(FEED, &by_tag).await.unwrap().total_hits, 1);

        // One row survives, the one matching the bytes on disk, and the two
        // memberships of `default` merged.
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM packages WHERE lower_id = 'dup.pkg'")
                .fetch_one(pool(&db))
                .await
                .unwrap();
        assert_eq!(rows, 1);
        let data = db.get_package_data("dup.pkg", &rc).await.unwrap().unwrap();
        assert_eq!(data.package_hash, "new-bytes");
        let merged = db.find_all_versions(FEED, "dup.pkg").await.unwrap();
        assert_eq!(merged.len(), 1);
        assert!(merged[0].pinned);
        assert_eq!(merged[0].package.downloads, 5);
        assert!(db.exists("other", "dup.pkg", &rc).await.unwrap());
        assert!(db.delete_package_data("dup.pkg", &rc).await.unwrap());
        assert_eq!(db.feed_count("dup.pkg", &rc).await.unwrap(), 0);

        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(pool(&db))
            .await
            .unwrap();
        assert_eq!(version, 4);
    }

    #[tokio::test]
    async fn two_processes_can_migrate_the_same_file_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race.db").to_string_lossy().into_owned();
        {
            let db = SqliteDatabase::connect(&path).await.unwrap();
            db.add_to_feed(FEED, &sample("Race.Pkg", "1.0.0-Beta"))
                .await
                .unwrap();
            sqlx::query("PRAGMA user_version = 0")
                .execute(pool(&db))
                .await
                .unwrap();
        }
        // The server and `yanuget migrate` started together: both must open
        // the file, and the migrations must run once.
        let (a, b) = tokio::join!(
            SqliteDatabase::connect(&path),
            SqliteDatabase::connect(&path)
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        let v = NuGetVersion::parse("1.0.0-beta").unwrap();
        assert!(a.find(FEED, "race.pkg", &v).await.unwrap().is_some());
        assert_eq!(b.feed_count("race.pkg", &v).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn versions_stored_before_the_search_index_are_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db").to_string_lossy().into_owned();
        {
            let db = SqliteDatabase::connect(&path).await.unwrap();
            db.add_to_feed(FEED, &sample("Before.Index", "1.0.0"))
                .await
                .unwrap();
            for step in [
                "DELETE FROM search_text",
                "DELETE FROM search_keys",
                "PRAGMA user_version = 3",
            ] {
                sqlx::query(step).execute(pool(&db)).await.unwrap();
            }
        }
        let db = SqliteDatabase::connect(&path).await.unwrap();
        assert_eq!(hits(&db, "before.ind").await, ["Before.Index"]);
    }

    #[tokio::test]
    async fn ensure_column_is_idempotent() {
        // Re-opening (which re-runs migrations) must not fail or drop data.
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Keep", "1.0.0"))
            .await
            .unwrap();
        ensure_column(
            pool(&db),
            "packages",
            "enabled",
            "INTEGER NOT NULL DEFAULT 1",
        )
        .await
        .unwrap();
        // Adding a genuinely new column then re-running is a no-op the 2nd time.
        ensure_column(pool(&db), "packages", "extra_col", "TEXT")
            .await
            .unwrap();
        ensure_column(pool(&db), "packages", "extra_col", "TEXT")
            .await
            .unwrap();
        assert!(db
            .find(FEED, "keep", &NuGetVersion::parse("1.0.0").unwrap())
            .await
            .unwrap()
            .is_some());
    }
}
