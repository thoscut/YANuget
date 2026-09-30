//! Upgrades from every shape a database has had.
//!
//! Each fixture writes a database file the way a past build left it — the
//! released schemas verbatim from git history, the unreleased ones derived
//! from the last unnumbered shape by taking away what they did not have yet —
//! fills it with rows through plain SQL, and opens it with this build.

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{Row, SqlitePool};

use super::schema::{migrate_to, LATEST};
use super::test_support::*;

/// `packages` as 0.1 created it: no admin flag, no requireLicenseAcceptance,
/// and an index over `listed` that no later build dropped.
const SCHEMA_0_1: &str = r#"
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

/// What the admin flag and the symbol server added to a 0.1 database.
const PRE_FEEDS_ADDITIONS: &str = r#"
ALTER TABLE packages ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1;
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

/// The first feeds build's `feed_packages`: no downloads, no pins.
const FIRST_FEEDS_ADDITIONS: &str = r#"
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
    PRIMARY KEY (feed, lower_id, normalized_version)
);
CREATE INDEX IF NOT EXISTS idx_feed_packages_feed ON feed_packages (feed, lower_id);
CREATE INDEX IF NOT EXISTS idx_feed_packages_pkg
    ON feed_packages (lower_id, normalized_version);
"#;

/// The schema of 0.5.0 and 0.5.1 as released.
const SCHEMA_0_5: &str = r#"
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

/// A shape some build left a database in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Era {
    /// 0.1: `packages` alone. `user_version` 0.
    V0_1,
    /// Before feeds: the admin flag (added to a 0.1 table) and symbols.
    /// `user_version` 0.
    PreFeeds,
    /// The first feeds build: memberships without downloads or pins, seeded
    /// without a number. `user_version` 0.
    FirstFeeds,
    /// 0.5.0 and 0.5.1. `user_version` 1.
    V0_5,
    /// The tag index, before pins. `user_version` 2.
    Tags,
    /// Pins, attached files and resumable uploads. `user_version` 3.
    Files,
    /// The search index, on a branch without tombstones. `user_version` 4.
    Search,
    /// The last unnumbered shape. `user_version` 4.
    Unnumbered,
}

impl Era {
    fn has_symbols(self) -> bool {
        self >= Era::PreFeeds
    }
    fn has_feeds(self) -> bool {
        self >= Era::FirstFeeds
    }
    fn has_tag_index(self) -> bool {
        self >= Era::Tags
    }
    fn has_files(self) -> bool {
        self >= Era::Files
    }
    /// Keys stored before step 3 may keep a pre-release label's case.
    fn beta(self) -> &'static str {
        if self >= Era::Files {
            "2.0.0-beta"
        } else {
            "2.0.0-Beta"
        }
    }
}

/// A database file holding nothing yet, opened as the server opens one.
async fn open(path: &str) -> SqlitePool {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true);
    SqlitePool::connect_with(options).await.unwrap()
}

async fn run(pool: &SqlitePool, sql: &str) {
    sqlx::raw_sql(sql).execute(pool).await.unwrap();
}

/// Write the schema `era` had, empty.
async fn shape(pool: &SqlitePool, era: Era) {
    match era {
        Era::V0_1 => run(pool, SCHEMA_0_1).await,
        Era::PreFeeds => {
            run(pool, SCHEMA_0_1).await;
            run(pool, PRE_FEEDS_ADDITIONS).await;
        }
        Era::FirstFeeds => {
            run(pool, SCHEMA_0_1).await;
            run(pool, PRE_FEEDS_ADDITIONS).await;
            run(pool, FIRST_FEEDS_ADDITIONS).await;
        }
        Era::V0_5 => {
            run(pool, SCHEMA_0_5).await;
            run(pool, "PRAGMA user_version = 1").await;
        }
        // The unreleased shapes: the last unnumbered one, less what they did
        // not have yet.
        Era::Tags | Era::Files | Era::Search | Era::Unnumbered => {
            migrate_to(pool, 4).await.unwrap();
            if era <= Era::Search {
                run(pool, "DROP TABLE tombstones").await;
            }
            if era <= Era::Files {
                run(
                    pool,
                    "DROP TRIGGER packages_search_insert; DROP TRIGGER packages_search_delete; \
                     DROP TRIGGER packages_search_update; DROP TABLE search_text; \
                     DROP TABLE search_keys; PRAGMA user_version = 3",
                )
                .await;
            }
            if era <= Era::Tags {
                run(
                    pool,
                    "DROP TABLE package_files; DROP TABLE uploads; \
                     ALTER TABLE feed_packages DROP COLUMN pinned; PRAGMA user_version = 2",
                )
                .await;
            }
        }
    }
}

/// Insert a `packages` row with the columns every era has.
#[allow(clippy::too_many_arguments)]
async fn package(
    pool: &SqlitePool,
    id: &str,
    version: &str,
    listed: bool,
    downloads: i64,
    tags: &str,
    title: Option<&str>,
    published: &str,
    hash: &str,
) {
    sqlx::query(
        "INSERT INTO packages (id, lower_id, normalized_version, original_version, \
             version_major, version_minor, version_patch, version_revision, is_prerelease, \
             is_semver2, listed, authors, description, title, tags, has_readme, \
             has_embedded_icon, is_development_dependency, package_size, package_hash, \
             package_hash_algorithm, published, downloads, package_types, dependencies) \
         VALUES (?1, lower(?1), ?2, ?2, 1, 0, 0, 0, ?3, 0, ?4, '[\"A\"]', \
             'described ' || ?1, ?5, ?6, 0, 0, 0, 1000, ?7, 'SHA512', ?8, ?9, '[]', '[]')",
    )
    .bind(id)
    .bind(version)
    .bind(i64::from(version.contains('-')))
    .bind(i64::from(listed))
    .bind(title)
    .bind(tags)
    .bind(hash)
    .bind(published)
    .bind(downloads)
    .execute(pool)
    .await
    .unwrap();
}

const PUBLISHED: &str = "2020-01-01T00:00:00+00:00";

/// Fill a database of `era` with rows: two packages (one hidden), their
/// memberships of two feeds where the era had feeds, and one row of every
/// other kind the era knew, plus rows left behind by versions that are gone.
async fn fill(pool: &SqlitePool, era: Era) {
    let beta = era.beta();
    package(
        pool,
        "Alpha",
        "1.0.0",
        true,
        7,
        r#"["Tools","json"]"#,
        Some("Alpha Title"),
        PUBLISHED,
        "h1",
    )
    .await;
    package(
        pool,
        "Alpha",
        beta,
        true,
        2,
        r#"["Tools"]"#,
        None,
        PUBLISHED,
        "h2",
    )
    .await;
    package(
        pool, "Hidden", "1.0.0", false, 0, "[]", None, PUBLISHED, "h3",
    )
    .await;
    if era >= Era::PreFeeds {
        run(
            pool,
            "UPDATE packages SET enabled = 0 WHERE lower_id = 'hidden'",
        )
        .await;
    }
    if era.has_symbols() {
        run(
            pool,
            "INSERT INTO symbols VALUES ('KEY1', 'alpha.pdb', 'alpha', '1.0.0'); \
             INSERT INTO symbols VALUES ('KEY2', 'ghost.pdb', 'ghost', '1.0.0')",
        )
        .await;
    }
    if era.has_feeds() {
        let with_downloads = era >= Era::V0_5;
        for (feed, id, version, listed, enabled, pending, downloads) in [
            ("default", "alpha", "1.0.0", 1, 1, 0, 7),
            ("default", "alpha", beta, 1, 1, 0, 2),
            ("default", "hidden", "1.0.0", 0, 0, 0, 0),
            ("stable", "alpha", "1.0.0", 1, 1, 1, 0),
            // A membership of a version whose data is gone.
            ("default", "ghost", "1.0.0", 1, 1, 0, 1),
        ] {
            let sql = if with_downloads {
                "INSERT INTO feed_packages (feed, lower_id, normalized_version, listed, enabled, \
                     pending, flagged, added, downloads) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8)"
            } else {
                "INSERT INTO feed_packages (feed, lower_id, normalized_version, listed, enabled, \
                     pending, flagged, added) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)"
            };
            let mut query = sqlx::query(sql)
                .bind(feed)
                .bind(id)
                .bind(version)
                .bind(listed)
                .bind(enabled)
                .bind(pending)
                .bind(PUBLISHED);
            if with_downloads {
                query = query.bind(downloads);
            }
            query.execute(pool).await.unwrap();
        }
    }
    if era.has_tag_index() {
        run(
            pool,
            "INSERT INTO package_tags VALUES ('alpha', '1.0.0', 'tools'); \
             INSERT INTO package_tags VALUES ('alpha', '1.0.0', 'json'); \
             INSERT INTO package_tags VALUES ('alpha', '2.0.0-beta', 'tools'); \
             INSERT INTO package_tags VALUES ('ghost', '1.0.0', 'gone')",
        )
        .await;
    }
    if era.has_files() {
        run(
            pool,
            "UPDATE feed_packages SET pinned = 1 \
                 WHERE feed = 'default' AND lower_id = 'alpha' AND normalized_version = '1.0.0'; \
             INSERT INTO package_files VALUES ('alpha', '1.0.0', 'Disk.iso', 'disk.iso', \
                 'aa', 3, '2020-01-01T00:00:00+00:00', 4); \
             INSERT INTO package_files VALUES ('ghost', '1.0.0', 'lost.bin', 'lost.bin', \
                 'bb', 5, '2020-01-01T00:00:00+00:00', 0); \
             INSERT INTO uploads VALUES ('up1', 'default', 'alpha', '1.0.0', 'more.iso', 10, 4, \
                 NULL, '2020-01-01T00:00:00+00:00', '2999-01-01T00:00:00+00:00')",
        )
        .await;
    }
    if era == Era::Unnumbered {
        run(
            pool,
            "INSERT INTO tombstones VALUES ('default', 'gone', '1.0.0', \
                 '2020-01-01T00:00:00+00:00')",
        )
        .await;
    }
}

/// A database file as `era` left it, filled with [`fill`]'s rows.
async fn fixture(era: Era) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db").to_string_lossy().into_owned();
    let pool = open(&path).await;
    shape(&pool, era).await;
    fill(&pool, era).await;
    pool.close().await;
    (dir, path)
}

async fn user_version(db: &SqliteDatabase) -> i64 {
    sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool(db))
        .await
        .unwrap()
}

/// Every table's columns and every index, trigger and table by name.
async fn tables(db: &SqliteDatabase) -> Vec<(String, Vec<String>)> {
    let names: Vec<(String, String)> =
        sqlx::query_as("SELECT type, name FROM sqlite_master ORDER BY type, name")
            .fetch_all(pool(db))
            .await
            .unwrap();
    let mut out = Vec::new();
    for (kind, name) in names {
        let mut columns: Vec<String> = Vec::new();
        if kind == "table" {
            for row in sqlx::query(&format!("PRAGMA table_info(\"{name}\")"))
                .fetch_all(pool(db))
                .await
                .unwrap()
            {
                columns.push(row.get("name"));
            }
            columns.sort();
        }
        out.push((format!("{kind} {name}"), columns));
    }
    out
}

fn v(s: &str) -> NuGetVersion {
    NuGetVersion::parse_stored(s).unwrap()
}

/// Open a database of `era` and check it reached the current schema with its
/// data intact.
async fn upgrade(era: Era) {
    let (_dir, path) = fixture(era).await;
    let db = SqliteDatabase::connect(&path).await.unwrap();
    assert_eq!(user_version(&db).await, LATEST, "{era:?}");

    // The schema a new database gets, whatever the path here.
    let fresh = SqliteDatabase::in_memory().await.unwrap();
    assert_eq!(tables(&db).await, tables(&fresh).await, "{era:?}");

    // Memberships, with their state: copied from the package where the era
    // had no feeds.
    let alpha = db.find(FEED, "alpha", &v("1.0.0")).await.unwrap().unwrap();
    assert_eq!(alpha.id, "Alpha");
    let downloads = if era == Era::FirstFeeds { 0 } else { 7 };
    assert_eq!(alpha.downloads, downloads, "{era:?}");
    let beta = db.find(FEED, "ALPHA", &v("2.0.0-BETA")).await.unwrap();
    assert_eq!(beta.unwrap().version.original(), era.beta(), "{era:?}");
    let hidden = db.find_all_versions(FEED, "hidden").await.unwrap();
    assert_eq!(hidden.len(), 1, "{era:?}");
    assert!(!hidden[0].package.listed, "{era:?}");
    assert_eq!(hidden[0].package.enabled, era == Era::V0_1, "{era:?}");
    if era.has_feeds() {
        let stable = db.get_membership("stable", "alpha", &v("1.0.0")).await;
        assert!(stable.unwrap().unwrap().pending, "{era:?}");
    }
    let pinned = db.find_all_versions(FEED, "alpha").await.unwrap()[0].pinned;
    assert_eq!(pinned, era.has_files(), "{era:?}");

    // The tag index and the search index, filled or kept.
    let tags = db.tag_counts(FEED, 10).await.unwrap();
    assert!(
        tags.contains(&TagCount {
            tag: "tools".into(),
            packages: 1
        }),
        "{era:?}: {tags:?}"
    );
    assert_eq!(hits(&db, "pha tit").await, ["Alpha"], "{era:?}");
    assert_eq!(hits(&db, "described alp").await, ["Alpha"], "{era:?}");

    if era.has_symbols() {
        let symbol = db.find_symbol("key1", "ALPHA.pdb").await.unwrap().unwrap();
        assert_eq!(symbol.lower_id, "alpha");
    }
    if era.has_files() {
        let files = db.files_for("alpha", &v("1.0.0")).await.unwrap();
        assert_eq!(files.len(), 1, "{era:?}");
        assert_eq!(files[0].downloads, 4);
        let upload = db.get_upload("up1").await.unwrap().unwrap();
        assert_eq!(upload.received, 4);
    }
    if era == Era::Unnumbered {
        assert!(db.is_tombstoned(FEED, "gone", &v("1.0.0")).await.unwrap());
    }

    // It takes new versions like any other database.
    db.add_to_feed(FEED, &sample("Alpha", "3.0.0"))
        .await
        .unwrap();
    assert_eq!(
        db.find_versions(FEED, "alpha", true).await.unwrap().len(),
        3
    );

    // And opening it again changes nothing.
    drop(db);
    let db = SqliteDatabase::connect(&path).await.unwrap();
    assert_eq!(
        db.find_versions(FEED, "alpha", true).await.unwrap().len(),
        3
    );
}

#[tokio::test]
async fn a_0_1_database_is_upgraded() {
    upgrade(Era::V0_1).await;
}

#[tokio::test]
async fn a_database_from_before_feeds_is_upgraded() {
    upgrade(Era::PreFeeds).await;
}

#[tokio::test]
async fn a_database_from_the_first_feeds_build_is_upgraded() {
    upgrade(Era::FirstFeeds).await;
}

#[tokio::test]
async fn a_0_5_database_is_upgraded() {
    upgrade(Era::V0_5).await;
}

#[tokio::test]
async fn a_database_with_the_tag_index_is_upgraded() {
    upgrade(Era::Tags).await;
}

#[tokio::test]
async fn a_database_with_attached_files_is_upgraded() {
    upgrade(Era::Files).await;
}

#[tokio::test]
async fn a_database_with_search_but_no_tombstones_is_upgraded() {
    upgrade(Era::Search).await;
}

#[tokio::test]
async fn the_last_unnumbered_database_is_upgraded() {
    upgrade(Era::Unnumbered).await;
}

#[tokio::test]
async fn pre_release_keys_from_before_0_5_are_lower_cased_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db").to_string_lossy().into_owned();
    let beta = v("1.0.0-Beta");
    let rc = v("2.0.0-rc");
    {
        let pool = open(&path).await;
        migrate_to(&pool, 2).await.unwrap();
        // A version only ever stored the old way, with a symbol and a file.
        package(
            &pool,
            "Old.Pkg",
            "1.0.0-Beta",
            true,
            0,
            r#"["Legacy"]"#,
            None,
            PUBLISHED,
            "h",
        )
        .await;
        run(
            &pool,
            "INSERT INTO package_tags VALUES ('old.pkg', '1.0.0-Beta', 'legacy'); \
             INSERT INTO feed_packages (feed, lower_id, normalized_version, added) \
                 VALUES ('default', 'old.pkg', '1.0.0-Beta', '2020-01-01T00:00:00+00:00'); \
             INSERT INTO symbols VALUES ('ABCDEF01FFFFFFFF', 'old.pdb', 'old.pkg', '1.0.0-Beta'); \
             INSERT INTO package_files VALUES ('old.pkg', '1.0.0-Beta', 'image.iso', \
                 'image.iso', 'aa', 3, '2020-01-01T00:00:00+00:00', 0)",
        )
        .await;

        // A version stored the old way and then pushed again after the
        // upgrade: two rows, one file. The re-push wrote the file last.
        let earlier = (Utc::now() - chrono::Duration::days(10)).to_rfc3339();
        let now = Utc::now().to_rfc3339();
        package(
            &pool,
            "Dup.Pkg",
            "2.0.0-RC",
            true,
            0,
            "[]",
            None,
            &earlier,
            "old-bytes",
        )
        .await;
        package(
            &pool,
            "Dup.Pkg",
            "2.0.0-rc",
            true,
            0,
            "[]",
            None,
            &now,
            "new-bytes",
        )
        .await;
        run(
            &pool,
            "INSERT INTO feed_packages (feed, lower_id, normalized_version, added, downloads, \
                 pinned) VALUES ('default', 'dup.pkg', '2.0.0-RC', '2020', 3, 1); \
             INSERT INTO feed_packages (feed, lower_id, normalized_version, added) \
                 VALUES ('other', 'dup.pkg', '2.0.0-RC', '2020'); \
             INSERT INTO feed_packages (feed, lower_id, normalized_version, added, downloads) \
                 VALUES ('default', 'dup.pkg', '2.0.0-rc', '2021', 2)",
        )
        .await;
        pool.close().await;
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
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM packages WHERE lower_id = 'dup.pkg'")
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
    assert_eq!(user_version(&db).await, LATEST);
}

#[tokio::test]
async fn two_processes_can_migrate_the_same_file_at_once() {
    let (_dir, path) = fixture(Era::V0_1).await;
    // The server and `yanuget migrate` started together: both must open
    // the file, and the migrations must run once.
    let (a, b) = tokio::join!(
        SqliteDatabase::connect(&path),
        SqliteDatabase::connect(&path)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let beta = v("2.0.0-beta");
    assert!(a.find(FEED, "alpha", &beta).await.unwrap().is_some());
    assert_eq!(b.feed_count("alpha", &beta).await.unwrap(), 1);
    assert_eq!(user_version(&a).await, LATEST);
}

#[tokio::test]
async fn a_database_from_a_newer_release_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new.db").to_string_lossy().into_owned();
    {
        let db = SqliteDatabase::connect(&path).await.unwrap();
        sqlx::query(&format!("PRAGMA user_version = {}", LATEST + 1))
            .execute(pool(&db))
            .await
            .unwrap();
    }
    let err = SqliteDatabase::connect(&path).await.unwrap_err();
    assert!(err.to_string().contains("newer release"), "{err}");
}

/// A step that fails leaves the database at the step before, not half-way.
#[tokio::test]
async fn a_failed_step_changes_nothing() {
    let (_dir, path) = fixture(Era::V0_5).await;
    {
        // Something step 2 cannot index: a tag list that is not JSON.
        let pool = open(&path).await;
        run(
            &pool,
            "UPDATE packages SET tags = 'not json' WHERE lower_id = 'hidden'",
        )
        .await;
        pool.close().await;
    }
    assert!(SqliteDatabase::connect(&path).await.is_err());
    let pool = open(&path).await;
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(version, 1);
    let exists: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = 'package_tags'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(exists, 0, "step 2's tables were rolled back with it");
}
