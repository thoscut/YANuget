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

use super::{PackageDatabase, SearchGroup, SearchPage, SearchRequest};

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
CREATE INDEX IF NOT EXISTS idx_packages_search
    ON packages (lower_id, listed, is_prerelease, is_semver2);
"#;

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
        Ok(Self { pool })
    }

    /// Load all versions for a set of lower-cased ids, applying visibility
    /// filters, grouped and version-sorted, preserving the order of `ids`.
    async fn load_groups(
        &self,
        ids: &[String],
        include_prerelease: bool,
        include_semver2: bool,
        listed_only: bool,
    ) -> Result<Vec<SearchGroup>> {
        let mut groups = Vec::with_capacity(ids.len());
        for id in ids {
            let packages = self
                .find_versions_filtered(id, include_prerelease, include_semver2, listed_only)
                .await?;
            if !packages.is_empty() {
                groups.push(SearchGroup { packages });
            }
        }
        Ok(groups)
    }

    async fn find_versions_filtered(
        &self,
        lower_id: &str,
        include_prerelease: bool,
        include_semver2: bool,
        listed_only: bool,
    ) -> Result<Vec<Package>> {
        let rows = sqlx::query(
            r#"SELECT * FROM packages
               WHERE lower_id = ?1
                 AND (?2 = 1 OR listed = 1)
                 AND (?3 = 1 OR is_prerelease = 0)
                 AND (?4 = 1 OR is_semver2 = 0)"#,
        )
        .bind(lower_id)
        .bind(i64::from(!listed_only))
        .bind(i64::from(include_prerelease))
        .bind(i64::from(include_semver2))
        .fetch_all(&self.pool)
        .await?;

        let mut packages = rows
            .into_iter()
            .map(row_to_package)
            .collect::<Result<Vec<_>>>()?;
        packages.sort_by(|a, b| a.version.cmp(&b.version));
        Ok(packages)
    }
}

#[async_trait]
impl PackageDatabase for SqliteDatabase {
    async fn add(&self, p: &Package) -> Result<()> {
        let (major, minor, patch, revision) = p.version.core();
        let result = sqlx::query(
            r#"INSERT INTO packages (
                id, lower_id, normalized_version, original_version,
                version_major, version_minor, version_patch, version_revision,
                is_prerelease, is_semver2, listed,
                authors, description, icon_url, license_url, license_expression,
                project_url, repository_url, repository_type, min_client_version,
                release_notes, language, title, summary, tags,
                has_readme, has_embedded_icon, is_development_dependency,
                package_size, package_hash, package_hash_algorithm,
                published, downloads, package_types, dependencies
            ) VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20,
                ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28,
                ?29, ?30, ?31, ?32, ?33, ?34, ?35
            )"#,
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
        .bind(p.package_size as i64)
        .bind(&p.package_hash)
        .bind(&p.package_hash_algorithm)
        .bind(p.published.to_rfc3339())
        .bind(p.downloads as i64)
        .bind(json(&p.package_types)?)
        .bind(json(&p.dependencies)?)
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => Ok(()),
            Err(e) if is_unique_violation(&e) => Err(Error::PackageAlreadyExists),
            Err(e) => Err(Error::Database(e)),
        }
    }

    async fn exists(&self, id: &str, version: &NuGetVersion) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM packages WHERE lower_id = ?1 AND normalized_version = ?2 LIMIT 1",
        )
        .bind(id.to_lowercase())
        .bind(version.normalized())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn find(&self, id: &str, version: &NuGetVersion) -> Result<Option<Package>> {
        let row = sqlx::query("SELECT * FROM packages WHERE lower_id = ?1 AND normalized_version = ?2")
            .bind(id.to_lowercase())
            .bind(version.normalized())
            .fetch_optional(&self.pool)
            .await?;
        row.map(row_to_package).transpose()
    }

    async fn find_versions(&self, id: &str, include_unlisted: bool) -> Result<Vec<Package>> {
        self.find_versions_filtered(&id.to_lowercase(), true, true, !include_unlisted)
            .await
    }

    async fn set_listed(&self, id: &str, version: &NuGetVersion, listed: bool) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE packages SET listed = ?3 WHERE lower_id = ?1 AND normalized_version = ?2",
        )
        .bind(id.to_lowercase())
        .bind(version.normalized())
        .bind(i64::from(listed))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn delete(&self, id: &str, version: &NuGetVersion) -> Result<bool> {
        let result =
            sqlx::query("DELETE FROM packages WHERE lower_id = ?1 AND normalized_version = ?2")
                .bind(id.to_lowercase())
                .bind(version.normalized())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn increment_downloads(&self, id: &str, version: &NuGetVersion) -> Result<()> {
        sqlx::query(
            "UPDATE packages SET downloads = downloads + 1 WHERE lower_id = ?1 AND normalized_version = ?2",
        )
        .bind(id.to_lowercase())
        .bind(version.normalized())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn search(&self, request: &SearchRequest) -> Result<SearchPage> {
        let query = request.query.trim().to_lowercase();
        let pattern = like_pattern(&query);

        // Phase 1: pick the page of matching package ids, ranked by downloads.
        let id_rows = sqlx::query(
            r#"SELECT lower_id, SUM(downloads) AS total
               FROM packages
               WHERE listed = 1
                 AND (?1 = 1 OR is_prerelease = 0)
                 AND (?2 = 1 OR is_semver2 = 0)
                 AND (?3 = ''
                      OR lower_id LIKE ?4 ESCAPE '\'
                      OR lower(description) LIKE ?4 ESCAPE '\'
                      OR lower(tags) LIKE ?4 ESCAPE '\'
                      OR lower(IFNULL(title, '')) LIKE ?4 ESCAPE '\')
               GROUP BY lower_id
               ORDER BY total DESC, lower_id ASC
               LIMIT ?5 OFFSET ?6"#,
        )
        .bind(i64::from(request.include_prerelease))
        .bind(i64::from(request.include_semver2))
        .bind(&query)
        .bind(&pattern)
        .bind(request.take.max(0))
        .bind(request.skip.max(0))
        .fetch_all(&self.pool)
        .await?;

        let ids: Vec<String> = id_rows
            .iter()
            .map(|r| r.get::<String, _>("lower_id"))
            .collect();

        let total_hits: i64 = sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM (
                   SELECT lower_id FROM packages
                   WHERE listed = 1
                     AND (?1 = 1 OR is_prerelease = 0)
                     AND (?2 = 1 OR is_semver2 = 0)
                     AND (?3 = ''
                          OR lower_id LIKE ?4 ESCAPE '\'
                          OR lower(description) LIKE ?4 ESCAPE '\'
                          OR lower(tags) LIKE ?4 ESCAPE '\'
                          OR lower(IFNULL(title, '')) LIKE ?4 ESCAPE '\')
                   GROUP BY lower_id
               )"#,
        )
        .bind(i64::from(request.include_prerelease))
        .bind(i64::from(request.include_semver2))
        .bind(&query)
        .bind(&pattern)
        .fetch_one(&self.pool)
        .await?;

        // Phase 2: load every visible version for the chosen ids.
        let mut groups = self
            .load_groups(
                &ids,
                request.include_prerelease,
                request.include_semver2,
                true,
            )
            .await?;

        // Optional package-type filter (applied to the page).
        if let Some(pt) = &request.package_type {
            groups.retain(|g| {
                g.latest()
                    .package_types
                    .iter()
                    .any(|t| t.name.eq_ignore_ascii_case(pt))
            });
        }

        Ok(SearchPage { total_hits, groups })
    }

    async fn autocomplete(&self, query: &str, skip: i64, take: i64) -> Result<Vec<String>> {
        let q = query.trim().to_lowercase();
        let pattern = like_pattern(&q);
        let rows = sqlx::query(
            r#"SELECT MAX(id) AS id FROM packages
               WHERE listed = 1 AND (?1 = '' OR lower_id LIKE ?2 ESCAPE '\')
               GROUP BY lower_id
               ORDER BY lower_id ASC
               LIMIT ?3 OFFSET ?4"#,
        )
        .bind(&q)
        .bind(&pattern)
        .bind(take.max(0))
        .bind(skip.max(0))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(|r| r.get::<String, _>("id")).collect())
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

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .map(|d| d.is_unique_violation())
        .unwrap_or(false)
}

fn row_to_package(row: SqliteRow) -> Result<Package> {
    let original_version: String = row.try_get("original_version")?;
    let version = NuGetVersion::parse(&original_version)
        .map_err(|e| Error::InvalidVersion(e.to_string()))?;
    let published: String = row.try_get("published")?;
    let published = DateTime::parse_from_rfc3339(&published)
        .map_err(|e| Error::Other(anyhow::anyhow!("bad published timestamp: {e}")))?
        .with_timezone(&Utc);

    let authors: Vec<String> = from_json(&row.try_get::<String, _>("authors")?)?;
    let tags: Vec<String> = from_json(&row.try_get::<String, _>("tags")?)?;
    let package_types: Vec<PackageType> =
        from_json(&row.try_get::<String, _>("package_types")?)?;
    let dependencies: Vec<DependencyGroup> =
        from_json(&row.try_get::<String, _>("dependencies")?)?;

    Ok(Package {
        id: row.try_get("id")?,
        version,
        listed: row.try_get::<i64, _>("listed")? != 0,
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
        is_semver2: row.try_get::<i64, _>("is_semver2")? != 0,
        package_size: row.try_get::<i64, _>("package_size")? as u64,
        package_hash: row.try_get("package_hash")?,
        package_hash_algorithm: row.try_get("package_hash_algorithm")?,
        published,
        downloads: row.try_get::<i64, _>("downloads")? as u64,
        package_types,
        dependencies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str, version: &str) -> Package {
        Package {
            id: id.to_string(),
            version: NuGetVersion::parse(version).unwrap(),
            listed: true,
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
        db.add(&p).await.unwrap();

        let found = db.find("contoso.utils", &p.version).await.unwrap().unwrap();
        assert_eq!(found.id, "Contoso.Utils");
        assert_eq!(found.package_size, 25_000_000_000);
        assert_eq!(found.license_expression.as_deref(), Some("MIT"));

        // Duplicate insert is rejected.
        let err = db.add(&p).await.unwrap_err();
        assert!(matches!(err, Error::PackageAlreadyExists));
    }

    #[tokio::test]
    async fn versions_are_sorted_and_filtered() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        for v in ["1.0.0", "1.0.1", "2.0.0-rc.1", "0.9.0"] {
            db.add(&sample("Pkg", v)).await.unwrap();
        }
        let all = db.find_versions("pkg", false).await.unwrap();
        let versions: Vec<String> = all.iter().map(|p| p.normalized_version()).collect();
        assert_eq!(versions, vec!["0.9.0", "1.0.0", "1.0.1", "2.0.0-rc.1"]);

        // Hide an unlisted version.
        let v = NuGetVersion::parse("1.0.1").unwrap();
        assert!(db.set_listed("pkg", &v, false).await.unwrap());
        let listed = db.find_versions("pkg", false).await.unwrap();
        assert_eq!(listed.len(), 3);
        let with_unlisted = db.find_versions("pkg", true).await.unwrap();
        assert_eq!(with_unlisted.len(), 4);
    }

    #[tokio::test]
    async fn search_groups_and_ranks() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add(&sample("Alpha.Tools", "1.0.0")).await.unwrap();
        db.add(&sample("Alpha.Tools", "1.1.0")).await.unwrap();
        db.add(&sample("Beta.Lib", "2.0.0")).await.unwrap();
        // Give Beta.Lib more downloads so it ranks first.
        let v = NuGetVersion::parse("2.0.0").unwrap();
        for _ in 0..5 {
            db.increment_downloads("beta.lib", &v).await.unwrap();
        }

        let page = db
            .search(&SearchRequest {
                query: String::new(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total_hits, 2);
        assert_eq!(page.groups.len(), 2);
        assert_eq!(page.groups[0].latest().id, "Beta.Lib");
        assert_eq!(page.groups[1].packages.len(), 2); // both Alpha versions

        // Targeted query.
        let page = db
            .search(&SearchRequest {
                query: "alpha".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total_hits, 1);
        assert_eq!(page.groups[0].latest().id, "Alpha.Tools");
    }

    #[tokio::test]
    async fn prerelease_filter_hides_prereleases() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add(&sample("Only.Pre", "1.0.0-alpha")).await.unwrap();
        let page = db
            .search(&SearchRequest {
                include_prerelease: false,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page.total_hits, 0);
    }

    #[tokio::test]
    async fn delete_and_autocomplete() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add(&sample("Contoso.Cli", "1.0.0")).await.unwrap();
        db.add(&sample("Contoso.Core", "1.0.0")).await.unwrap();

        let ac = db.autocomplete("contoso", 0, 20).await.unwrap();
        assert_eq!(ac.len(), 2);
        assert!(ac.contains(&"Contoso.Cli".to_string()));

        let v = NuGetVersion::parse("1.0.0").unwrap();
        assert!(db.delete("contoso.cli", &v).await.unwrap());
        assert!(!db.exists("contoso.cli", &v).await.unwrap());
        let ac = db.autocomplete("contoso", 0, 20).await.unwrap();
        assert_eq!(ac, vec!["Contoso.Core".to_string()]);
    }
}
