//! SQLite-backed [`PackageDatabase`].
//!
//! Nested metadata (authors, tags, package types, dependency groups) is stored
//! as JSON text columns, which keeps the schema small while remaining fully
//! queryable for the few fields search needs. Versions are stored both
//! normalized (the canonical key) and in their original form (so the exact
//! string the author pushed can be round-tripped), plus split numeric
//! components for fast SQL ordering of the version *core*. Pre-release ordering,
//! which SQL cannot express faithfully, is finished in Rust via
//! [`NuGetVersion`]'s `Ord`.
//!
//! Package metadata lives once in `packages`. Which feeds expose a version, and
//! that version's per-feed state (listed / enabled / pending / flagged), lives
//! in `feed_packages`. Feed-scoped reads join the two.

use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteRow, SqliteSynchronous,
};
use sqlx::{Row, SqliteConnection, SqlitePool};

use crate::error::{Error, Result};
use crate::models::{DependencyGroup, Package, PackageType};
use crate::version::NuGetVersion;

use super::{
    canonical_id, DatabaseStats, FeedVersion, Membership, MembershipChange, PackageDatabase,
    PackageFile, SearchGroup, SearchPage, SearchRequest, SearchSort, SymbolKey, SymbolRef,
    TagCount, UploadSession, VersionFootprint,
};

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

/// The feed-scoped projection: every `packages` column plus the membership's
/// state aliased so it does not collide with the package's own template flags.
///
/// A macro rather than a `const` so call sites can `concat!` it into a whole
/// statement at compile time. sqlx only accepts a `&'static str` without an
/// explicit injection audit, and `format!`-ing a constant into a `String`
/// would forfeit that check to say nothing new.
macro_rules! feed_select {
    () => {
        "SELECT p.*, fp.listed AS m_listed, fp.enabled AS m_enabled, \
         fp.pending AS m_pending, fp.flagged AS m_flagged, fp.flag_reason AS m_flag_reason, \
         fp.downloads AS m_downloads, fp.pinned AS m_pinned \
         FROM packages p \
         JOIN feed_packages fp \
           ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version"
    };
}

/// A SQLite package index.
#[derive(Debug, Clone)]
pub struct SqliteDatabase {
    pool: SqlitePool,
}

impl SqliteDatabase {
    /// Open (creating if needed) a SQLite database at `path` and run migrations.
    pub async fn connect(path: &str) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            // `FULL`, not WAL's usual `NORMAL`: under `NORMAL` a power loss can
            // roll back a committed transaction, and deletes remove the payload
            // before the rows (see `retention::purge_global_data`). A rolled
            // back DELETE then brings back rows whose files are already gone.
            // The cost is one fsync of the WAL per commit, which the payload
            // writes around it dwarf.
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(30))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(16)
            .connect_with(options)
            .await?;
        Self::from_pool(pool).await
    }

    /// Build a fresh in-memory database (primarily for tests).
    pub async fn in_memory() -> Result<Self> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")
            .map_err(Error::Database)?
            .foreign_keys(true);
        // A single, never-closed connection keeps the in-memory DB alive.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        Self::from_pool(pool).await
    }

    async fn from_pool(pool: SqlitePool) -> Result<Self> {
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        // Migrate databases created before the admin `enabled` column existed.
        ensure_column(&pool, "packages", "enabled", "INTEGER NOT NULL DEFAULT 1").await?;
        // Databases predating the requireLicenseAcceptance passthrough. The
        // default is `false`, which is exactly what those rows were reported as.
        ensure_column(
            &pool,
            "packages",
            "require_license_acceptance",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        // Databases predating pins: nothing was pinned.
        ensure_column(
            &pool,
            "feed_packages",
            "pinned",
            "INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        // Drop the legacy (feed, lower_id) index, now subsumed by the wider
        // covering index `idx_feed_packages_rank` created above.
        sqlx::query("DROP INDEX IF EXISTS idx_feed_packages_feed")
            .execute(&pool)
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
        Ok(Self { pool })
    }

    /// Begin a transaction that writes, holding the write lock from the start.
    ///
    /// A deferred `BEGIN` takes a read snapshot at its first statement and
    /// upgrades it to a write lock only when it writes, and SQLite refuses that
    /// upgrade outright (`SQLITE_BUSY_SNAPSHOT`, no busy wait) if another
    /// connection committed in between. Preparing a statement that touches the
    /// FTS5 search index reads the index's own tables first, so concurrent
    /// pushes failed that way; `BEGIN IMMEDIATE` waits its turn instead.
    async fn write_tx(&self) -> Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
        Ok(self.pool.begin_with("BEGIN IMMEDIATE").await?)
    }

    /// Load every visible version for a set of lower-cased ids in `feed`,
    /// grouped and version-sorted, preserving the order of `ids`.
    async fn load_groups(
        &self,
        feed: &str,
        ids: &[String],
        include_prerelease: bool,
        include_semver2: bool,
        listed_only: bool,
    ) -> Result<Vec<SearchGroup>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        // One query per chunk rather than one per id. This ran once for every
        // id on the page — twenty statements for a default gallery view, and up
        // to a thousand at `take=1000`, against a sixteen-connection pool, on a
        // page anyone can request.
        // Chunked: see `ID_CHUNK`.
        let mut by_id: std::collections::HashMap<String, Vec<Package>> =
            std::collections::HashMap::with_capacity(ids.len());

        for chunk in ids.chunks(ID_CHUNK) {
            // Parameters ?1..?4 are the flags; the ids follow from ?5.
            let placeholders = placeholders(5, chunk.len());
            let sql = format!(
                concat!(
                    feed_select!(),
                    " WHERE fp.feed = ?1 \
                       AND fp.enabled = 1 AND fp.pending = 0 \
                       AND (?2 = 1 OR fp.listed = 1) \
                       AND (?3 = 1 OR p.is_prerelease = 0) \
                       AND (?4 = 1 OR p.is_semver2 = 0) \
                       AND fp.lower_id IN ({placeholders})"
                ),
                placeholders = placeholders
            );
            // The only interpolation is `placeholders`, which is `?5,?6,…`
            // generated from a range — the ids themselves are bound, never
            // formatted in.
            let mut query = sqlx::query(&sql)
                .bind(feed)
                .bind(i64::from(!listed_only))
                .bind(i64::from(include_prerelease))
                .bind(i64::from(include_semver2));
            for id in chunk {
                query = query.bind(id);
            }
            for row in query.fetch_all(&self.pool).await? {
                let package = row_to_feed_package(&row)?.package;
                by_id.entry(package.lower_id()).or_default().push(package);
            }
        }

        // Emit in the order the caller asked for — that order is the search
        // ranking, and a HashMap has none.
        let mut groups = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(mut packages) = by_id.remove(id) {
                packages.sort_by(|a, b| a.version.cmp(&b.version));
                groups.push(SearchGroup { packages });
            }
        }
        Ok(groups)
    }

    async fn find_versions_filtered(
        &self,
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
        .fetch_all(&self.pool)
        .await?;

        let mut packages = rows
            .iter()
            .map(|r| row_to_feed_package(r).map(|fv| fv.package))
            .collect::<Result<Vec<_>>>()?;
        packages.sort_by(|a, b| a.version.cmp(&b.version));
        Ok(packages)
    }
}

#[async_trait]
impl PackageDatabase for SqliteDatabase {
    async fn ping(&self) -> Result<()> {
        // Touch a real table rather than `SELECT 1`, so a database file that has
        // vanished or been replaced by an empty one is reported as unhealthy.
        sqlx::query("SELECT COUNT(*) FROM packages LIMIT 1")
            .fetch_one(&self.pool)
            .await?;
        Ok(())
    }

    async fn upsert_package_data(&self, p: &Package) -> Result<bool> {
        // The row and its tag index in one transaction: as two autocommit
        // statements, a failed tag insert left a package the tag filter and
        // the tag cloud never saw, and nothing ever retried it.
        let mut tx = self.write_tx().await?;
        let inserted = insert_package(&mut tx, p).await?;
        tx.commit().await?;
        Ok(inserted)
    }

    async fn add_version(&self, p: &Package, m: &Membership) -> Result<bool> {
        let mut tx = self.write_tx().await?;
        let inserted = insert_package(&mut tx, p).await?;
        insert_membership(&mut tx, m).await?;
        tx.commit().await?;
        Ok(inserted)
    }

    async fn replace_version(&self, p: &Package, m: &Membership) -> Result<()> {
        let mut tx = self.write_tx().await?;
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

    async fn orphaned_versions(&self, limit: i64) -> Result<Vec<Package>> {
        let rows = sqlx::query(
            "SELECT * FROM packages p WHERE NOT EXISTS ( \
                 SELECT 1 FROM feed_packages fp \
                 WHERE fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version) \
             LIMIT ?1",
        )
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_package).collect()
    }

    async fn package_data_exists(&self, id: &str, version: &NuGetVersion) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM packages WHERE lower_id = ?1 AND normalized_version = ?2 LIMIT 1",
        )
        .bind(canonical_id(id))
        .bind(version.normalized())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn get_package_data(&self, id: &str, version: &NuGetVersion) -> Result<Option<Package>> {
        let row =
            sqlx::query("SELECT * FROM packages WHERE lower_id = ?1 AND normalized_version = ?2")
                .bind(canonical_id(id))
                .bind(version.normalized())
                .fetch_optional(&self.pool)
                .await?;
        row.as_ref().map(row_to_package).transpose()
    }

    async fn delete_package_data(&self, id: &str, version: &NuGetVersion) -> Result<bool> {
        let lower = canonical_id(id);
        let normalized = version.normalized();
        // All or nothing. As separate statements, a failure after the
        // memberships went left a `packages` row that no feed held and nothing
        // would ever revisit; a later push then adopted its missing payload.
        let mut tx = self.write_tx().await?;
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

    async fn feed_count(&self, id: &str, version: &NuGetVersion) -> Result<i64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM feed_packages WHERE lower_id = ?1 AND normalized_version = ?2",
        )
        .bind(canonical_id(id))
        .bind(version.normalized())
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    async fn add_membership(&self, m: &Membership) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        insert_membership(&mut conn, m).await
    }

    async fn remove_membership(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM feed_packages WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
        )
        .bind(feed)
        .bind(canonical_id(id))
        .bind(version.normalized())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn get_membership(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<Option<Membership>> {
        let row = sqlx::query(
            "SELECT * FROM feed_packages WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
        )
        .bind(feed)
        .bind(canonical_id(id))
        .bind(version.normalized())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| Membership {
            feed: r.get("feed"),
            lower_id: r.get("lower_id"),
            normalized_version: r.get("normalized_version"),
            listed: r.get::<i64, _>("listed") != 0,
            enabled: r.get::<i64, _>("enabled") != 0,
            pending: r.get::<i64, _>("pending") != 0,
            flagged: r.get::<i64, _>("flagged") != 0,
            flag_reason: r.get("flag_reason"),
            pinned: r.get::<i64, _>("pinned") != 0,
        }))
    }

    async fn approve_membership(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE feed_packages SET pending = 0 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
        )
        .bind(feed)
        .bind(canonical_id(id))
        .bind(version.normalized())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn exists(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM feed_packages WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3 LIMIT 1",
        )
        .bind(feed)
        .bind(canonical_id(id))
        .bind(version.normalized())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn find(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<Option<Package>> {
        let row = sqlx::query(concat!(
            feed_select!(),
            " WHERE fp.feed = ?1 AND fp.lower_id = ?2 \
               AND p.normalized_version = ?3 AND fp.enabled = 1 AND fp.pending = 0"
        ))
        .bind(feed)
        .bind(canonical_id(id))
        .bind(version.normalized())
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref()
            .map(|r| row_to_feed_package(r).map(|fv| fv.package))
            .transpose()
    }

    async fn find_versions(
        &self,
        feed: &str,
        id: &str,
        include_unlisted: bool,
    ) -> Result<Vec<Package>> {
        self.find_versions_filtered(feed, &canonical_id(id), true, true, !include_unlisted)
            .await
    }

    async fn set_listed(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
        listed: bool,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE feed_packages SET listed = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
        )
        .bind(feed)
        .bind(canonical_id(id))
        .bind(version.normalized())
        .bind(i64::from(listed))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn set_enabled(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
        enabled: bool,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE feed_packages SET enabled = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
        )
        .bind(feed)
        .bind(canonical_id(id))
        .bind(version.normalized())
        .bind(i64::from(enabled))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn set_pinned(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
        pinned: bool,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE feed_packages SET pinned = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
        )
        .bind(feed)
        .bind(canonical_id(id))
        .bind(version.normalized())
        .bind(i64::from(pinned))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn update_memberships(
        &self,
        feed: &str,
        id: &str,
        versions: &[NuGetVersion],
        change: MembershipChange,
    ) -> Result<u64> {
        let (sql, value) = match change {
            MembershipChange::Enabled(on) => (
                "UPDATE feed_packages SET enabled = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
                i64::from(on),
            ),
            MembershipChange::Pinned(on) => (
                "UPDATE feed_packages SET pinned = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
                i64::from(on),
            ),
            MembershipChange::Approve => (
                "UPDATE feed_packages SET pending = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
                0,
            ),
        };
        // Dropping the transaction without committing rolls it back, so an
        // early return below leaves every row as it was.
        let mut tx = self.write_tx().await?;
        let mut updated = 0;
        for version in versions {
            let result = sqlx::query(sql)
                .bind(feed)
                .bind(canonical_id(id))
                .bind(version.normalized())
                .bind(value)
                .execute(&mut *tx)
                .await?;
            if result.rows_affected() == 0 {
                return Err(Error::PackageNotFound);
            }
            updated += result.rows_affected();
        }
        tx.commit().await?;
        Ok(updated)
    }

    async fn is_servable(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM feed_packages
             WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3
               AND enabled = 1 AND pending = 0 LIMIT 1",
        )
        .bind(feed)
        .bind(canonical_id(id))
        .bind(version.normalized())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn find_all_versions(&self, feed: &str, id: &str) -> Result<Vec<FeedVersion>> {
        let rows = sqlx::query(concat!(
            feed_select!(),
            " WHERE fp.feed = ?1 AND fp.lower_id = ?2"
        ))
        .bind(feed)
        .bind(canonical_id(id))
        .fetch_all(&self.pool)
        .await?;
        let mut versions = rows
            .iter()
            .map(row_to_feed_package)
            .collect::<Result<Vec<_>>>()?;
        versions.sort_by(|a, b| a.package.version.cmp(&b.package.version));
        Ok(versions)
    }

    async fn find_all_versions_of(&self, feed: &str, ids: &[String]) -> Result<Vec<FeedVersion>> {
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
            for row in query.fetch_all(&self.pool).await? {
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

    async fn version_footprints(&self, ids: &[String]) -> Result<Vec<VersionFootprint>> {
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
            for row in query.fetch_all(&self.pool).await? {
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

    async fn increment_downloads(
        &self,
        feed: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE feed_packages SET downloads = downloads + 1
             WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
        )
        .bind(feed)
        .bind(canonical_id(id))
        .bind(version.normalized())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn search(&self, feed: &str, request: &SearchRequest) -> Result<SearchPage> {
        let query = search_terms(&request.query);
        // Two or more characters... three, in fact: a trigram index can only
        // answer a query that has at least one trigram. Shorter ones scan the
        // (capped) indexed text instead, which is cheap for what they are.
        let short = query.chars().count() < 3;
        let needle = if short {
            like_pattern(&query)
        } else {
            // An FTS5 phrase: the whole query, quotes doubled, matched as a
            // substring of any indexed column.
            format!("\"{}\"", query.replace('"', "\"\""))
        };

        // An optional package-type filter, applied in SQL so the page and the
        // total count stay consistent. `''` (no filter) makes the predicate a
        // no-op; otherwise a package id matches when any of its versions
        // declares the type. `json_each`/`json_extract` parse the stored JSON
        // so there is no quoting/escaping ambiguity.
        let package_type = request.package_type.as_deref().unwrap_or("").to_lowercase();
        // An optional tag, matched exactly (case-insensitively) against the
        // tag index: a package matches when any of its visible versions has it.
        let tag = request.tag.as_deref().unwrap_or("").trim().to_lowercase();

        // Phase 1: pick the page of matching package ids, ranked by downloads.
        //
        // `$how` names the `matcher!` that selects the rowids of the matching
        // FTS rows (`?5`).
        macro_rules! filter {
            ($how:ident) => {
                concat!(
                "fp.feed = ?1 AND fp.listed = 1 AND fp.enabled = 1 AND fp.pending = 0 \
                 AND (?2 = 1 OR p.is_prerelease = 0) \
                 AND (?3 = 1 OR p.is_semver2 = 0) \
                 AND (?4 = '' OR (p.lower_id, p.normalized_version) IN ( \
                      SELECT k.lower_id, k.normalized_version FROM search_keys k \
                      WHERE k.id IN (", matcher!($how), "))) \
                 AND (?6 = '' OR EXISTS ( \
                      SELECT 1 FROM json_each(p.package_types) je \
                      WHERE lower(json_extract(je.value, '$.name')) = ?6)) \
                 AND (?7 = '' OR EXISTS ( \
                      SELECT 1 FROM package_tags pt \
                      WHERE pt.lower_id = p.lower_id \
                        AND pt.normalized_version = p.normalized_version AND pt.tag = ?7))"
                )
            };
        }
        macro_rules! matcher {
            (phrase) => {
                "SELECT rowid FROM search_text WHERE search_text MATCH ?5"
            };
            (scan) => {
                "SELECT rowid FROM search_text WHERE lower_id LIKE ?5 ESCAPE '\\' \
                 OR title LIKE ?5 ESCAPE '\\' OR tags LIKE ?5 ESCAPE '\\' \
                 OR description LIKE ?5 ESCAPE '\\'"
            };
        }

        // One statement per order, each still a compile-time constant. The id
        // is the last key of every order, so a page boundary never falls
        // between two packages that tie.
        macro_rules! page_of_ids {
            ($order:literal, $how:ident) => {
                concat!(
                    "SELECT p.lower_id AS lower_id, SUM(fp.downloads) AS total, \
                     MAX(p.published) AS updated \
                     FROM packages p JOIN feed_packages fp \
                       ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version \
                     WHERE ",
                    filter!($how),
                    " GROUP BY p.lower_id ORDER BY ",
                    $order,
                    " LIMIT ?8 OFFSET ?9"
                )
            };
        }
        // `published` is stored as RFC 3339 in UTC, so it orders as text.
        let page_sql = match (request.sort, short) {
            (SearchSort::Downloads, false) => page_of_ids!("total DESC, p.lower_id ASC", phrase),
            (SearchSort::Downloads, true) => page_of_ids!("total DESC, p.lower_id ASC", scan),
            (SearchSort::Name, false) => page_of_ids!("p.lower_id ASC", phrase),
            (SearchSort::Name, true) => page_of_ids!("p.lower_id ASC", scan),
            (SearchSort::Updated, false) => {
                page_of_ids!("updated DESC, p.lower_id ASC", phrase)
            }
            (SearchSort::Updated, true) => page_of_ids!("updated DESC, p.lower_id ASC", scan),
        };

        let id_rows = sqlx::query(page_sql)
            .bind(feed)
            .bind(i64::from(request.include_prerelease))
            .bind(i64::from(request.include_semver2))
            .bind(&query)
            .bind(&needle)
            .bind(&package_type)
            .bind(&tag)
            .bind(request.take.max(0))
            .bind(request.skip.max(0))
            .fetch_all(&self.pool)
            .await?;

        let ids: Vec<String> = id_rows
            .iter()
            .map(|r| r.get::<String, _>("lower_id"))
            .collect();

        macro_rules! count {
            ($how:ident) => {
                concat!(
                    "SELECT COUNT(*) FROM ( \
                         SELECT p.lower_id FROM packages p JOIN feed_packages fp \
                           ON fp.lower_id = p.lower_id \
                          AND fp.normalized_version = p.normalized_version \
                         WHERE ",
                    filter!($how),
                    " GROUP BY p.lower_id )"
                )
            };
        }
        let count_sql = if short { count!(scan) } else { count!(phrase) };
        let total_hits: i64 = sqlx::query_scalar(count_sql)
            .bind(feed)
            .bind(i64::from(request.include_prerelease))
            .bind(i64::from(request.include_semver2))
            .bind(&query)
            .bind(&needle)
            .bind(&package_type)
            .bind(&tag)
            .fetch_one(&self.pool)
            .await?;

        // Phase 2: load every visible version for the chosen ids.
        let groups = self
            .load_groups(
                feed,
                &ids,
                request.include_prerelease,
                request.include_semver2,
                true,
            )
            .await?;

        Ok(SearchPage { total_hits, groups })
    }

    async fn autocomplete(
        &self,
        feed: &str,
        query: &str,
        include_prerelease: bool,
        include_semver2: bool,
        skip: i64,
        take: i64,
    ) -> Result<(Vec<String>, i64)> {
        let q = canonical_id(&search_terms(query));
        let pattern = like_pattern(&q);
        // The version predicates sit inside the grouped scan, so an id survives
        // only if it still has at least one version the caller would accept.
        macro_rules! matching_ids {
            () => {
                r#"
            FROM packages p JOIN feed_packages fp
                ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version
            WHERE fp.feed = ?1 AND fp.listed = 1 AND fp.enabled = 1 AND fp.pending = 0
              AND (?2 = '' OR p.lower_id LIKE ?3 ESCAPE '\')
              AND (?4 = 1 OR p.is_prerelease = 0)
              AND (?5 = 1 OR p.is_semver2 = 0)
            GROUP BY p.lower_id"#
            };
        }

        let rows = sqlx::query(concat!(
            "SELECT MAX(p.id) AS id ",
            matching_ids!(),
            " ORDER BY p.lower_id ASC LIMIT ?6 OFFSET ?7"
        ))
        .bind(feed)
        .bind(&q)
        .bind(&pattern)
        .bind(i64::from(include_prerelease))
        .bind(i64::from(include_semver2))
        .bind(take.max(0))
        .bind(skip.max(0))
        .fetch_all(&self.pool)
        .await?;
        let ids: Vec<String> = rows.iter().map(|r| r.get::<String, _>("id")).collect();

        // `GROUP BY` makes this a count of groups, not of rows, so it has to be
        // wrapped rather than written as a bare `COUNT(*)`.
        let total: i64 = sqlx::query_scalar(concat!(
            "SELECT COUNT(*) FROM (SELECT p.lower_id ",
            matching_ids!(),
            ")"
        ))
        .bind(feed)
        .bind(&q)
        .bind(&pattern)
        .bind(i64::from(include_prerelease))
        .bind(i64::from(include_semver2))
        .fetch_one(&self.pool)
        .await?;

        Ok((ids, total))
    }

    async fn all_package_ids(&self, feed: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            r#"SELECT MAX(p.id) AS id FROM packages p JOIN feed_packages fp
                   ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version
               WHERE fp.feed = ?1
               GROUP BY p.lower_id ORDER BY p.lower_id ASC"#,
        )
        .bind(feed)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
    }

    async fn stats(&self, feed: &str) -> Result<DatabaseStats> {
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
        .fetch_one(&self.pool)
        .await?;
        let symbol_count: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM symbols s WHERE EXISTS (
                   SELECT 1 FROM feed_packages fp
                   WHERE fp.feed = ?1 AND fp.lower_id = s.lower_id
                     AND fp.normalized_version = s.normalized_version)"#,
        )
        .bind(feed)
        .fetch_one(&self.pool)
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
        .fetch_one(&self.pool)
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

    async fn recent_packages(&self, feed: &str, limit: i64) -> Result<Vec<Package>> {
        let rows = sqlx::query(concat!(
            feed_select!(),
            " WHERE fp.feed = ?1 AND fp.enabled = 1 AND fp.pending = 0 \
             ORDER BY p.published DESC LIMIT ?2"
        ))
        .bind(feed)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| row_to_feed_package(r).map(|fv| fv.package))
            .collect()
    }

    async fn tag_counts(&self, feed: &str, limit: i64) -> Result<Vec<TagCount>> {
        // The same visibility as search, so every tag listed leads somewhere.
        let rows = sqlx::query(
            "SELECT pt.tag AS tag, COUNT(DISTINCT pt.lower_id) AS packages \
             FROM package_tags pt JOIN feed_packages fp \
               ON fp.lower_id = pt.lower_id AND fp.normalized_version = pt.normalized_version \
             WHERE fp.feed = ?1 AND fp.listed = 1 AND fp.enabled = 1 AND fp.pending = 0 \
             GROUP BY pt.tag ORDER BY packages DESC, pt.tag ASC LIMIT ?2",
        )
        .bind(feed)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| TagCount {
                tag: r.get("tag"),
                packages: r.get("packages"),
            })
            .collect())
    }

    async fn add_file(&self, f: &PackageFile) -> Result<()> {
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
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if is_unique_violation(&e) => Err(Error::PackageAlreadyExists),
            Err(e) => Err(Error::Database(e)),
        }
    }

    async fn files_for(&self, id: &str, version: &NuGetVersion) -> Result<Vec<PackageFile>> {
        let rows = sqlx::query(
            "SELECT * FROM package_files WHERE lower_id = ?1 AND normalized_version = ?2 \
             ORDER BY lower_name",
        )
        .bind(canonical_id(id))
        .bind(version.normalized())
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_file).collect()
    }

    async fn files_for_id(&self, id: &str) -> Result<Vec<PackageFile>> {
        let rows = sqlx::query(
            "SELECT * FROM package_files WHERE lower_id = ?1 \
             ORDER BY normalized_version, lower_name",
        )
        .bind(canonical_id(id))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_file).collect()
    }

    async fn get_file(
        &self,
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
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(row_to_file).transpose()
    }

    async fn delete_file(
        &self,
        id: &str,
        version: &NuGetVersion,
        name: &str,
    ) -> Result<Option<PackageFile>> {
        let Some(file) = self.get_file(id, version, name).await? else {
            return Ok(None);
        };
        sqlx::query(
            "DELETE FROM package_files \
             WHERE lower_id = ?1 AND normalized_version = ?2 AND lower_name = ?3",
        )
        .bind(canonical_id(id))
        .bind(version.normalized())
        .bind(name.to_lowercase())
        .execute(&self.pool)
        .await?;
        Ok(Some(file))
    }

    async fn blob_references(&self, sha256: &str) -> Result<i64> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM package_files WHERE sha256 = ?1")
                .bind(sha256)
                .fetch_one(&self.pool)
                .await?,
        )
    }

    async fn increment_file_downloads(
        &self,
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn create_upload(&self, u: &UploadSession) -> Result<()> {
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
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn get_upload(&self, id: &str) -> Result<Option<UploadSession>> {
        let row = sqlx::query("SELECT * FROM uploads WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        row.as_ref().map(row_to_upload).transpose()
    }

    async fn set_upload_received(&self, id: &str, received: u64) -> Result<()> {
        sqlx::query("UPDATE uploads SET received = ?2 WHERE id = ?1")
            .bind(id)
            .bind(received as i64)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_upload(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM uploads WHERE id = ?1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn expired_uploads(&self, now: DateTime<Utc>) -> Result<Vec<UploadSession>> {
        // Stored as RFC 3339 in UTC, so the times compare as text.
        let rows = sqlx::query("SELECT * FROM uploads WHERE expires < ?1")
            .bind(now.to_rfc3339())
            .fetch_all(&self.pool)
            .await?;
        rows.iter().map(row_to_upload).collect()
    }

    async fn add_symbol(
        &self,
        key: &str,
        filename: &str,
        id: &str,
        version: &NuGetVersion,
    ) -> Result<()> {
        sqlx::query(
            // The `WHERE` is what keeps a symbol key attached to the package
            // that first claimed it. Both halves of the key are chosen by the
            // uploader, so without it any push credential could repoint another
            // package's — or another feed's — symbols at itself. A package may
            // still update its own key, which is what re-pushing a `.snupkg`
            // does; a different package's attempt becomes a no-op here.
            r#"INSERT INTO symbols (ssqp_key, filename, lower_id, normalized_version)
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT(ssqp_key, filename) DO UPDATE SET
                   lower_id = excluded.lower_id,
                   normalized_version = excluded.normalized_version
               WHERE symbols.lower_id = excluded.lower_id"#,
        )
        .bind(key.to_uppercase())
        .bind(filename.to_lowercase())
        .bind(canonical_id(id))
        .bind(version.normalized())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_symbol(&self, key: &str, filename: &str) -> Result<Option<SymbolRef>> {
        let row = sqlx::query(
            "SELECT lower_id, normalized_version FROM symbols
             WHERE ssqp_key = ?1 AND filename = ?2",
        )
        .bind(key.to_uppercase())
        .bind(filename.to_lowercase())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SymbolRef {
            lower_id: r.get::<String, _>("lower_id"),
            normalized_version: r.get::<String, _>("normalized_version"),
        }))
    }

    async fn find_symbols(&self, id: &str, version: &NuGetVersion) -> Result<Vec<SymbolKey>> {
        let rows = sqlx::query(
            "SELECT ssqp_key, filename FROM symbols
             WHERE lower_id = ?1 AND normalized_version = ?2",
        )
        .bind(canonical_id(id))
        .bind(version.normalized())
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| SymbolKey {
                key: r.get::<String, _>("ssqp_key"),
                filename: r.get::<String, _>("filename"),
            })
            .collect())
    }

    async fn delete_symbols(&self, id: &str, version: &NuGetVersion) -> Result<u64> {
        let result =
            sqlx::query("DELETE FROM symbols WHERE lower_id = ?1 AND normalized_version = ?2")
                .bind(canonical_id(id))
                .bind(version.normalized())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected())
    }
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

/// Insert a membership; [`Error::PackageAlreadyExists`] when the version is
/// already in the feed.
async fn insert_membership(conn: &mut SqliteConnection, m: &Membership) -> Result<()> {
    let result = sqlx::query(
        r#"INSERT INTO feed_packages
               (feed, lower_id, normalized_version, listed, enabled, pending,
                flagged, flag_reason, added, downloads, pinned)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)"#,
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
    .execute(&mut *conn)
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(e) if is_unique_violation(&e) => Err(Error::PackageAlreadyExists),
        Err(e) => Err(Error::Database(e)),
    }
}

/// How many ids one `IN (…)` list binds. Each is a parameter, and SQLite
/// builds before 3.32 cap those at 999; a single list of a thousand would be a
/// latent failure on exactly the deployments least able to debug it.
const ID_CHUNK: usize = 400;

/// `?first,?first+1,…`: `count` numbered parameters for an `IN (…)` list.
fn placeholders(first: usize, count: usize) -> String {
    (first..first + count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The longest search query acted on, in characters. Longer ones are cut:
/// no package id, title or tag is anywhere near it, and every character
/// makes an anonymous request's matching dearer.
pub const MAX_QUERY_CHARS: usize = 256;

/// A search or autocomplete query as matched: trimmed, lower-cased and cut at
/// [`MAX_QUERY_CHARS`].
fn search_terms(query: &str) -> String {
    query
        .trim()
        .chars()
        .take(MAX_QUERY_CHARS)
        .collect::<String>()
        .to_lowercase()
}

/// Build a `%...%` LIKE pattern, escaping the LIKE metacharacters in `query`.
fn like_pattern(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len() + 2);
    escaped.push('%');
    for ch in query.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('%');
    escaped
}

fn json<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|e| Error::Other(e.into()))
}

fn from_json<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_str(s).map_err(|e| Error::Other(e.into()))
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

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .map(|d| d.is_unique_violation())
        .unwrap_or(false)
}

/// Build a [`Package`] from a `packages` row, taking the listed/enabled flags
/// and download count from explicit arguments so it works for both global and
/// feed reads (each of which sources those values from a different column).
fn build_package(row: &SqliteRow, listed: bool, enabled: bool, downloads: u64) -> Result<Package> {
    let original_version: String = row.try_get("original_version")?;
    let version =
        NuGetVersion::parse(&original_version).map_err(|e| Error::InvalidVersion(e.to_string()))?;
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

/// A feed-scoped row (package joined to a membership): listed/enabled/pending/
/// flagged/downloads come from the membership's aliased columns.
fn parse_time(raw: &str, what: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| Error::Other(anyhow::anyhow!("bad {what} timestamp: {e}")))
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

fn row_to_feed_package(row: &SqliteRow) -> Result<FeedVersion> {
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
    use super::*;

    const FEED: &str = "default";

    fn sample(id: &str, version: &str) -> Package {
        Package {
            id: id.to_string(),
            version: NuGetVersion::parse(version).unwrap(),
            listed: true,
            enabled: true,
            authors: vec!["Alice".into()],
            description: format!("description for {id}"),
            icon_url: None,
            license_url: None,
            license_expression: Some("MIT".into()),
            project_url: None,
            repository_url: None,
            repository_type: None,
            min_client_version: None,
            release_notes: None,
            language: None,
            title: None,
            summary: None,
            tags: vec!["sample".into()],
            has_readme: false,
            has_embedded_icon: false,
            is_development_dependency: false,
            require_license_acceptance: false,
            is_semver2: NuGetVersion::parse(version).unwrap().is_semver2(),
            package_size: 25_000_000_000, // 25 GB — exercises i64 sizing
            package_hash: "aGFzaA==".into(),
            package_hash_algorithm: "SHA512".into(),
            published: Utc::now(),
            downloads: 0,
            package_types: vec![],
            dependencies: vec![],
        }
    }

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

    #[tokio::test]
    async fn a_batch_membership_update_is_all_or_nothing() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let (a, b) = (sample("Batch.Pkg", "1.0.0"), sample("Batch.Pkg", "2.0.0"));
        db.add_to_feed(FEED, &a).await.unwrap();
        db.add_to_feed(FEED, &b).await.unwrap();
        let missing = NuGetVersion::parse("3.0.0").unwrap();

        // One version the feed does not hold: nothing changes.
        let err = db
            .update_memberships(
                FEED,
                "batch.pkg",
                &[a.version.clone(), missing, b.version.clone()],
                MembershipChange::Enabled(false),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PackageNotFound));
        assert!(db.is_servable(FEED, "batch.pkg", &a.version).await.unwrap());

        let n = db
            .update_memberships(
                FEED,
                "Batch.Pkg",
                &[a.version.clone(), b.version.clone()],
                MembershipChange::Enabled(false),
            )
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert!(!db.is_servable(FEED, "batch.pkg", &a.version).await.unwrap());
        assert!(!db.is_servable(FEED, "batch.pkg", &b.version).await.unwrap());
    }

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
    async fn pending_membership_is_withheld_until_approved() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let p = sample("Ring.Pkg", "1.0.0");
        db.upsert_package_data(&p).await.unwrap();
        db.add_membership(&Membership {
            pending: true,
            ..Membership::active("stable", &p)
        })
        .await
        .unwrap();

        // Pending: present, but not servable and hidden from listings/search.
        assert!(db.exists("stable", "ring.pkg", &p.version).await.unwrap());
        assert!(!db
            .is_servable("stable", "ring.pkg", &p.version)
            .await
            .unwrap());
        assert!(db
            .find("stable", "ring.pkg", &p.version)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            db.search("stable", &SearchRequest::default())
                .await
                .unwrap()
                .total_hits,
            0
        );
        // Admin still sees it (as pending).
        let all = db.find_all_versions("stable", "ring.pkg").await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].pending);

        // Approving clears the gate.
        assert!(db
            .approve_membership("stable", "ring.pkg", &p.version)
            .await
            .unwrap());
        assert!(db
            .is_servable("stable", "ring.pkg", &p.version)
            .await
            .unwrap());
        assert_eq!(
            db.search("stable", &SearchRequest::default())
                .await
                .unwrap()
                .total_hits,
            1
        );
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

    fn tagged(id: &str, version: &str, tags: &[&str]) -> Package {
        let mut p = sample(id, version);
        p.tags = tags.iter().map(|t| t.to_string()).collect();
        p
    }

    #[tokio::test]
    async fn the_tag_filter_and_counts_see_packages_not_versions() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &tagged("Log.A", "1.0.0", &["Logging", "json"]))
            .await
            .unwrap();
        db.add_to_feed(FEED, &tagged("Log.A", "1.1.0", &["logging"]))
            .await
            .unwrap();
        db.add_to_feed(FEED, &tagged("Log.B", "2.0.0", &["LOGGING"]))
            .await
            .unwrap();
        db.add_to_feed(FEED, &tagged("Other", "1.0.0", &["json"]))
            .await
            .unwrap();
        // Another feed's tags are not this feed's.
        db.add_to_feed("elsewhere", &tagged("Far", "1.0.0", &["logging", "far"]))
            .await
            .unwrap();

        let by_tag = |tag: &str| SearchRequest {
            tag: Some(tag.into()),
            ..Default::default()
        };
        let page = db.search(FEED, &by_tag("Logging")).await.unwrap();
        assert_eq!(page.total_hits, 2);
        let ids: Vec<_> = page.groups.iter().map(|g| g.latest().id.clone()).collect();
        assert!(ids.contains(&"Log.A".to_string()) && ids.contains(&"Log.B".to_string()));
        assert_eq!(db.search(FEED, &by_tag("far")).await.unwrap().total_hits, 0);
        // It narrows a search rather than replacing it.
        let page = db
            .search(
                FEED,
                &SearchRequest {
                    query: "other".into(),
                    tag: Some("json".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total_hits, 1);

        let counts = db.tag_counts(FEED, 10).await.unwrap();
        assert_eq!(
            counts,
            vec![
                TagCount {
                    tag: "json".into(),
                    packages: 2
                },
                TagCount {
                    tag: "logging".into(),
                    packages: 2
                },
            ]
        );
        assert_eq!(db.tag_counts(FEED, 1).await.unwrap().len(), 1);

        // Deleting a version takes its tags with it.
        let v = NuGetVersion::parse("2.0.0").unwrap();
        db.delete_package_data("Log.B", &v).await.unwrap();
        assert_eq!(
            db.search(FEED, &by_tag("logging"))
                .await
                .unwrap()
                .total_hits,
            1
        );
    }

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
                .execute(&db.pool)
                .await
                .unwrap();
            sqlx::query("PRAGMA user_version = 1")
                .execute(&db.pool)
                .await
                .unwrap();
        }
        let db = SqliteDatabase::connect(&path).await.unwrap();
        let counts = db.tag_counts(FEED, 10).await.unwrap();
        assert_eq!(counts.len(), 2, "{counts:?}");
        assert!(counts.iter().any(|t| t.tag == "legacy"));
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&db.pool)
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
            .execute(&db.pool)
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
                .execute(&db.pool)
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
                .fetch_one(&db.pool)
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
            .fetch_one(&db.pool)
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
                .execute(&db.pool)
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

    fn query(q: &str) -> SearchRequest {
        SearchRequest {
            query: q.into(),
            ..Default::default()
        }
    }

    async fn hits(db: &SqliteDatabase, q: &str) -> Vec<String> {
        let page = db.search(FEED, &query(q)).await.unwrap();
        assert_eq!(page.total_hits as usize, page.groups.len(), "{q:?}");
        page.groups.iter().map(|g| g.latest().id.clone()).collect()
    }

    #[tokio::test]
    async fn search_matches_substrings_from_the_index() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let mut long = tagged("Long.Text", "1.0.0", &["Parsing", "json"]);
        long.title = Some("The Title Here".into());
        // Past the indexed 4000 characters: stored, served, not searched.
        long.description = format!("{} needle-at-the-end", "x".repeat(5000));
        db.add_to_feed(FEED, &long).await.unwrap();
        db.add_to_feed(FEED, &tagged("Other.Pkg", "1.0.0", &["tools"]))
            .await
            .unwrap();

        assert_eq!(hits(&db, "ng.te").await, ["Long.Text"], "id substring");
        assert_eq!(
            hits(&db, "TITLE HE").await,
            ["Long.Text"],
            "title, any case"
        );
        assert_eq!(hits(&db, "arsin").await, ["Long.Text"], "a tag");
        assert_eq!(hits(&db, "description for oth").await, ["Other.Pkg"]);
        assert!(hits(&db, "needle-at-the-end").await.is_empty());
        let stored = db
            .find(FEED, "long.text", &NuGetVersion::parse("1.0.0").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.description.len(), 5018, "stored in full");

        // Short queries, and characters that mean something to FTS or LIKE.
        assert_eq!(hits(&db, "g.").await, ["Long.Text"]);
        assert_eq!(hits(&db, "x").await, ["Long.Text"]);
        assert!(hits(&db, "%").await.is_empty());
        assert!(hits(&db, "\"x").await.is_empty());
        assert!(hits(&db, "x\" OR \"o").await.is_empty());
        // The tags are words, not the JSON they are stored as.
        assert!(hits(&db, "\",\"").await.is_empty());
        // An absurd query is cut, not refused or run in full.
        assert!(hits(&db, &"xy".repeat(10_000)).await.is_empty());

        // Deleting a version takes it out of the index.
        db.delete_package_data("Other.Pkg", &NuGetVersion::parse("1.0.0").unwrap())
            .await
            .unwrap();
        assert!(hits(&db, "description for oth").await.is_empty());
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
                sqlx::query(step).execute(&db.pool).await.unwrap();
            }
        }
        let db = SqliteDatabase::connect(&path).await.unwrap();
        assert_eq!(hits(&db, "before.ind").await, ["Before.Index"]);
    }

    #[tokio::test]
    async fn search_groups_and_ranks() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Alpha.Tools", "1.0.0"))
            .await
            .unwrap();
        db.add_to_feed(FEED, &sample("Alpha.Tools", "1.1.0"))
            .await
            .unwrap();
        db.add_to_feed(FEED, &sample("Beta.Lib", "2.0.0"))
            .await
            .unwrap();
        // Give Beta.Lib more downloads so it ranks first.
        let v = NuGetVersion::parse("2.0.0").unwrap();
        for _ in 0..5 {
            db.increment_downloads(FEED, "beta.lib", &v).await.unwrap();
        }

        let page = db
            .search(
                FEED,
                &SearchRequest {
                    query: String::new(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total_hits, 2);
        assert_eq!(page.groups.len(), 2);
        assert_eq!(page.groups[0].latest().id, "Beta.Lib");
        assert_eq!(page.groups[1].packages.len(), 2); // both Alpha versions

        // Targeted query.
        let page = db
            .search(
                FEED,
                &SearchRequest {
                    query: "alpha".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total_hits, 1);
        assert_eq!(page.groups[0].latest().id, "Alpha.Tools");
    }

    #[tokio::test]
    async fn package_type_filter_keeps_count_and_page_consistent() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let mut tool = sample("Contoso.Tool", "1.0.0");
        tool.package_types = vec![PackageType {
            name: "DotnetTool".into(),
            version: None,
        }];
        db.add_to_feed(FEED, &tool).await.unwrap();
        db.add_to_feed(FEED, &sample("Contoso.Lib", "1.0.0"))
            .await
            .unwrap();

        // The filter is applied in SQL, so total_hits matches the page.
        let page = db
            .search(
                FEED,
                &SearchRequest {
                    package_type: Some("dotnettool".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total_hits, 1);
        assert_eq!(page.groups.len(), 1);
        assert_eq!(page.groups[0].latest().id, "Contoso.Tool");

        // A type nothing declares yields zero, consistently (case-insensitive).
        let none = db
            .search(
                FEED,
                &SearchRequest {
                    package_type: Some("Template".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(none.total_hits, 0);
        assert_eq!(none.groups.len(), 0);
    }

    #[tokio::test]
    async fn prerelease_filter_hides_prereleases() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Only.Pre", "1.0.0-alpha"))
            .await
            .unwrap();
        let page = db
            .search(
                FEED,
                &SearchRequest {
                    include_prerelease: false,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(page.total_hits, 0);
    }

    /// `load_groups` batches its ids into one query per chunk and reassembles
    /// the result. Two things have to survive that: the caller's order, which
    /// *is* the search ranking and which a `HashMap` does not preserve, and the
    /// version order inside each group.
    #[tokio::test]
    async fn grouped_loading_keeps_both_orderings() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        for (id, version) in [
            ("Alpha.Pkg", "1.0.0"),
            ("Alpha.Pkg", "2.0.0"),
            ("Alpha.Pkg", "1.5.0"),
            ("Beta.Pkg", "0.1.0"),
            ("Gamma.Pkg", "3.0.0"),
        ] {
            db.add_to_feed(FEED, &sample(id, version)).await.unwrap();
        }

        // Deliberately not alphabetical: this is the ranking the caller chose.
        let ids = vec![
            "gamma.pkg".to_string(),
            "alpha.pkg".to_string(),
            "beta.pkg".to_string(),
        ];
        let groups = db.load_groups(FEED, &ids, true, true, false).await.unwrap();

        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].latest().lower_id(), "gamma.pkg");
        assert_eq!(groups[1].latest().lower_id(), "alpha.pkg");
        assert_eq!(groups[2].latest().lower_id(), "beta.pkg");

        // Versions ascend within a group, so `latest()` really is the latest.
        let alpha: Vec<String> = groups[1]
            .packages
            .iter()
            .map(|p| p.normalized_version())
            .collect();
        assert_eq!(alpha, ["1.0.0", "1.5.0", "2.0.0"]);

        // An id with nothing visible is dropped rather than yielding an empty
        // group, which `latest()` would panic on.
        let missing = vec!["nope.pkg".to_string(), "beta.pkg".to_string()];
        let groups = db
            .load_groups(FEED, &missing, true, true, false)
            .await
            .unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].latest().lower_id(), "beta.pkg");

        assert!(db
            .load_groups(FEED, &[], true, true, false)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn delete_and_autocomplete() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Contoso.Cli", "1.0.0"))
            .await
            .unwrap();
        db.add_to_feed(FEED, &sample("Contoso.Core", "1.0.0"))
            .await
            .unwrap();

        let (ac, total) = db
            .autocomplete(FEED, "contoso", true, true, 0, 20)
            .await
            .unwrap();
        assert_eq!(ac.len(), 2);
        assert_eq!(total, 2);
        assert!(ac.contains(&"Contoso.Cli".to_string()));

        // The total counts every match, not the page: a caller paging on it
        // must be able to reach the ids the first page left out.
        let (page, total) = db
            .autocomplete(FEED, "contoso", true, true, 0, 1)
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(total, 2, "totalHits must count matches, not the page");

        let v = NuGetVersion::parse("1.0.0").unwrap();
        assert!(db.delete_package_data("contoso.cli", &v).await.unwrap());
        assert!(!db.exists(FEED, "contoso.cli", &v).await.unwrap());
        let (ac, total) = db
            .autocomplete(FEED, "contoso", true, true, 0, 20)
            .await
            .unwrap();
        assert_eq!(ac, vec!["Contoso.Core".to_string()]);
        assert_eq!(total, 1);
    }

    #[tokio::test]
    async fn disabled_versions_are_hidden_but_present() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Pkg", "1.0.0")).await.unwrap();
        db.add_to_feed(FEED, &sample("Pkg", "2.0.0")).await.unwrap();
        let v1 = NuGetVersion::parse("1.0.0").unwrap();

        // Disable 1.0.0 — hidden from public listings, search and serving.
        assert!(db.set_enabled(FEED, "pkg", &v1, false).await.unwrap());
        assert!(!db.is_servable(FEED, "pkg", &v1).await.unwrap());
        assert!(db
            .is_servable(FEED, "pkg", &NuGetVersion::parse("2.0.0").unwrap())
            .await
            .unwrap());
        assert!(db.find(FEED, "pkg", &v1).await.unwrap().is_none());

        let listed = db.find_versions(FEED, "pkg", true).await.unwrap();
        assert_eq!(listed.len(), 1); // only 2.0.0
        let page = db.search(FEED, &SearchRequest::default()).await.unwrap();
        assert_eq!(page.groups[0].packages.len(), 1);

        // But it still exists and admin listing shows it.
        assert!(db.exists(FEED, "pkg", &v1).await.unwrap());
        let all = db.find_all_versions(FEED, "pkg").await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|p| !p.package.enabled));

        // Re-enable restores visibility.
        assert!(db.set_enabled(FEED, "pkg", &v1, true).await.unwrap());
        assert!(db.is_servable(FEED, "pkg", &v1).await.unwrap());
        assert_eq!(db.find_versions(FEED, "pkg", true).await.unwrap().len(), 2);
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

    #[tokio::test]
    async fn symbol_mappings_round_trip_and_clean_up() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Sym", "1.0.0")).await.unwrap();
        let v = NuGetVersion::parse("1.0.0").unwrap();

        db.add_symbol("ABCDEF01FFFFFFFF", "sym.pdb", "Sym", &v)
            .await
            .unwrap();
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
    }

    #[tokio::test]
    async fn ensure_column_is_idempotent() {
        // Re-opening (which re-runs migrations) must not fail or drop data.
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Keep", "1.0.0"))
            .await
            .unwrap();
        ensure_column(
            &db.pool,
            "packages",
            "enabled",
            "INTEGER NOT NULL DEFAULT 1",
        )
        .await
        .unwrap();
        // Adding a genuinely new column then re-running is a no-op the 2nd time.
        ensure_column(&db.pool, "packages", "extra_col", "TEXT")
            .await
            .unwrap();
        ensure_column(&db.pool, "packages", "extra_col", "TEXT")
            .await
            .unwrap();
        assert!(db
            .find(FEED, "keep", &NuGetVersion::parse("1.0.0").unwrap())
            .await
            .unwrap()
            .is_some());
    }
}
