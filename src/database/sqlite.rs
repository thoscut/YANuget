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
use sqlx::{Row, SqlitePool};

use crate::error::{Error, Result};
use crate::models::{DependencyGroup, Package, PackageType};
use crate::version::NuGetVersion;

use super::{
    DatabaseStats, FeedVersion, Membership, PackageDatabase, SearchGroup, SearchPage,
    SearchRequest, SymbolKey, SymbolRef,
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
"#;

/// The feed-scoped projection: every `packages` column plus the membership's
/// state aliased so it does not collide with the package's own template flags.
const FEED_SELECT: &str = "SELECT p.*, fp.listed AS m_listed, fp.enabled AS m_enabled, \
     fp.pending AS m_pending, fp.flagged AS m_flagged, fp.flag_reason AS m_flag_reason, \
     fp.downloads AS m_downloads \
     FROM packages p \
     JOIN feed_packages fp \
       ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version";

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
            .synchronous(SqliteSynchronous::Normal)
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
        let mut tx = pool.begin().await?;
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
        tx.commit().await?;
        Ok(Self { pool })
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
        let mut groups = Vec::with_capacity(ids.len());
        for id in ids {
            let packages = self
                .find_versions_filtered(feed, id, include_prerelease, include_semver2, listed_only)
                .await?;
            if !packages.is_empty() {
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
        let sql = format!(
            "{FEED_SELECT} WHERE fp.feed = ?1 AND fp.lower_id = ?2 \
               AND fp.enabled = 1 AND fp.pending = 0 \
               AND (?3 = 1 OR fp.listed = 1) \
               AND (?4 = 1 OR p.is_prerelease = 0) \
               AND (?5 = 1 OR p.is_semver2 = 0)"
        );
        let rows = sqlx::query(&sql)
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
        let (major, minor, patch, revision) = p.version.core();
        let result = sqlx::query(
            r#"INSERT INTO packages (
                id, lower_id, normalized_version, original_version,
                version_major, version_minor, version_patch, version_revision,
                is_prerelease, is_semver2, listed, enabled,
                authors, description, icon_url, license_url, license_expression,
                project_url, repository_url, repository_type, min_client_version,
                release_notes, language, title, summary, tags,
                has_readme, has_embedded_icon, is_development_dependency,
                require_license_acceptance,
                package_size, package_hash, package_hash_algorithm,
                published, downloads, package_types, dependencies
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21,
                ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29,
                ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37
            )
            ON CONFLICT(lower_id, normalized_version) DO NOTHING"#,
        )
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
        .bind(json(&p.dependencies)?)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn package_data_exists(&self, id: &str, version: &NuGetVersion) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM packages WHERE lower_id = ?1 AND normalized_version = ?2 LIMIT 1",
        )
        .bind(id.to_lowercase())
        .bind(version.normalized())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn get_package_data(&self, id: &str, version: &NuGetVersion) -> Result<Option<Package>> {
        let row =
            sqlx::query("SELECT * FROM packages WHERE lower_id = ?1 AND normalized_version = ?2")
                .bind(id.to_lowercase())
                .bind(version.normalized())
                .fetch_optional(&self.pool)
                .await?;
        row.as_ref().map(row_to_package).transpose()
    }

    async fn delete_package_data(&self, id: &str, version: &NuGetVersion) -> Result<bool> {
        let lower = id.to_lowercase();
        let normalized = version.normalized();
        sqlx::query("DELETE FROM feed_packages WHERE lower_id = ?1 AND normalized_version = ?2")
            .bind(&lower)
            .bind(&normalized)
            .execute(&self.pool)
            .await?;
        let result =
            sqlx::query("DELETE FROM packages WHERE lower_id = ?1 AND normalized_version = ?2")
                .bind(&lower)
                .bind(&normalized)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn feed_count(&self, id: &str, version: &NuGetVersion) -> Result<i64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM feed_packages WHERE lower_id = ?1 AND normalized_version = ?2",
        )
        .bind(id.to_lowercase())
        .bind(version.normalized())
        .fetch_one(&self.pool)
        .await?;
        Ok(n)
    }

    async fn add_membership(&self, m: &Membership) -> Result<()> {
        let result = sqlx::query(
            r#"INSERT INTO feed_packages
                   (feed, lower_id, normalized_version, listed, enabled, pending,
                    flagged, flag_reason, added, downloads)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0)"#,
        )
        .bind(&m.feed)
        .bind(m.lower_id.to_lowercase())
        .bind(&m.normalized_version)
        .bind(i64::from(m.listed))
        .bind(i64::from(m.enabled))
        .bind(i64::from(m.pending))
        .bind(i64::from(m.flagged))
        .bind(&m.flag_reason)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await;
        match result {
            Ok(_) => Ok(()),
            Err(e) if is_unique_violation(&e) => Err(Error::PackageAlreadyExists),
            Err(e) => Err(Error::Database(e)),
        }
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
        .bind(id.to_lowercase())
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
        .bind(id.to_lowercase())
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
        .bind(id.to_lowercase())
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
        .bind(id.to_lowercase())
        .bind(version.normalized())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn find(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<Option<Package>> {
        let sql = format!(
            "{FEED_SELECT} WHERE fp.feed = ?1 AND fp.lower_id = ?2 \
               AND p.normalized_version = ?3 AND fp.enabled = 1 AND fp.pending = 0"
        );
        let row = sqlx::query(&sql)
            .bind(feed)
            .bind(id.to_lowercase())
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
        self.find_versions_filtered(feed, &id.to_lowercase(), true, true, !include_unlisted)
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
        .bind(id.to_lowercase())
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
        .bind(id.to_lowercase())
        .bind(version.normalized())
        .bind(i64::from(enabled))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn is_servable(&self, feed: &str, id: &str, version: &NuGetVersion) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM feed_packages
             WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3
               AND enabled = 1 AND pending = 0 LIMIT 1",
        )
        .bind(feed)
        .bind(id.to_lowercase())
        .bind(version.normalized())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn find_all_versions(&self, feed: &str, id: &str) -> Result<Vec<FeedVersion>> {
        let sql = format!("{FEED_SELECT} WHERE fp.feed = ?1 AND fp.lower_id = ?2");
        let rows = sqlx::query(&sql)
            .bind(feed)
            .bind(id.to_lowercase())
            .fetch_all(&self.pool)
            .await?;
        let mut versions = rows
            .iter()
            .map(row_to_feed_package)
            .collect::<Result<Vec<_>>>()?;
        versions.sort_by(|a, b| a.package.version.cmp(&b.package.version));
        Ok(versions)
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
        .bind(id.to_lowercase())
        .bind(version.normalized())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn search(&self, feed: &str, request: &SearchRequest) -> Result<SearchPage> {
        let query = request.query.trim().to_lowercase();
        let pattern = like_pattern(&query);

        // An optional package-type filter, applied in SQL so the page and the
        // total count stay consistent. `''` (no filter) makes the predicate a
        // no-op; otherwise a package id matches when any of its versions
        // declares the type. `json_each`/`json_extract` parse the stored JSON
        // so there is no quoting/escaping ambiguity.
        let package_type = request.package_type.as_deref().unwrap_or("").to_lowercase();

        // Phase 1: pick the page of matching package ids, ranked by downloads.
        let filter = "fp.feed = ?1 AND fp.listed = 1 AND fp.enabled = 1 AND fp.pending = 0 \
             AND (?2 = 1 OR p.is_prerelease = 0) \
             AND (?3 = 1 OR p.is_semver2 = 0) \
             AND (?4 = '' \
                  OR p.lower_id LIKE ?5 ESCAPE '\\' \
                  OR lower(p.description) LIKE ?5 ESCAPE '\\' \
                  OR lower(p.tags) LIKE ?5 ESCAPE '\\' \
                  OR lower(IFNULL(p.title, '')) LIKE ?5 ESCAPE '\\') \
             AND (?6 = '' OR EXISTS ( \
                  SELECT 1 FROM json_each(p.package_types) je \
                  WHERE lower(json_extract(je.value, '$.name')) = ?6))";

        let id_sql = format!(
            "SELECT p.lower_id AS lower_id, SUM(fp.downloads) AS total \
             FROM packages p JOIN feed_packages fp \
               ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version \
             WHERE {filter} \
             GROUP BY p.lower_id ORDER BY total DESC, p.lower_id ASC LIMIT ?7 OFFSET ?8"
        );
        let id_rows = sqlx::query(&id_sql)
            .bind(feed)
            .bind(i64::from(request.include_prerelease))
            .bind(i64::from(request.include_semver2))
            .bind(&query)
            .bind(&pattern)
            .bind(&package_type)
            .bind(request.take.max(0))
            .bind(request.skip.max(0))
            .fetch_all(&self.pool)
            .await?;

        let ids: Vec<String> = id_rows
            .iter()
            .map(|r| r.get::<String, _>("lower_id"))
            .collect();

        let count_sql = format!(
            "SELECT COUNT(*) FROM ( \
                 SELECT p.lower_id FROM packages p JOIN feed_packages fp \
                   ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version \
                 WHERE {filter} GROUP BY p.lower_id )"
        );
        let total_hits: i64 = sqlx::query_scalar(&count_sql)
            .bind(feed)
            .bind(i64::from(request.include_prerelease))
            .bind(i64::from(request.include_semver2))
            .bind(&query)
            .bind(&pattern)
            .bind(&package_type)
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
        let q = query.trim().to_lowercase();
        let pattern = like_pattern(&q);
        // The version predicates sit inside the grouped scan, so an id survives
        // only if it still has at least one version the caller would accept.
        const MATCHING_IDS: &str = r#"
            FROM packages p JOIN feed_packages fp
                ON fp.lower_id = p.lower_id AND fp.normalized_version = p.normalized_version
            WHERE fp.feed = ?1 AND fp.listed = 1 AND fp.enabled = 1 AND fp.pending = 0
              AND (?2 = '' OR p.lower_id LIKE ?3 ESCAPE '\')
              AND (?4 = 1 OR p.is_prerelease = 0)
              AND (?5 = 1 OR p.is_semver2 = 0)
            GROUP BY p.lower_id"#;

        let rows = sqlx::query(&format!(
            "SELECT MAX(p.id) AS id {MATCHING_IDS} ORDER BY p.lower_id ASC LIMIT ?6 OFFSET ?7"
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
        let total: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM (SELECT p.lower_id {MATCHING_IDS})"
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
        Ok(DatabaseStats {
            package_count: row.try_get("package_count")?,
            version_count: row.try_get("version_count")?,
            listed_count: row.try_get("listed_count")?,
            total_downloads: row.try_get("total_downloads")?,
            total_size: row.try_get("total_size")?,
            symbol_count,
        })
    }

    async fn recent_packages(&self, feed: &str, limit: i64) -> Result<Vec<Package>> {
        let sql = format!(
            "{FEED_SELECT} WHERE fp.feed = ?1 AND fp.enabled = 1 AND fp.pending = 0 \
             ORDER BY p.published DESC LIMIT ?2"
        );
        let rows = sqlx::query(&sql)
            .bind(feed)
            .bind(limit.max(0))
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| row_to_feed_package(r).map(|fv| fv.package))
            .collect()
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
        .bind(id.to_lowercase())
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
        .bind(id.to_lowercase())
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
                .bind(id.to_lowercase())
                .bind(version.normalized())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected())
    }
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
async fn ensure_column(pool: &SqlitePool, table: &str, column: &str, def: &str) -> Result<()> {
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
