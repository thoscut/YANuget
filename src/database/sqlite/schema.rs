//! The schema and its migrations.
//!
//! Every change to the schema is a numbered step in [`MIGRATIONS`], and
//! `PRAGMA user_version` holds the number of the last step a database has had.
//! Opening a database runs the steps above that number, in order, each in its
//! own `BEGIN IMMEDIATE` transaction that re-reads the number first: a step
//! applies completely or not at all, exactly once, and a second process
//! opening the same file (the server and `yanuget migrate`) waits for the
//! first and then finds nothing left to do. A new database runs every step
//! from the first, so it ends up exactly where an upgraded one does.
//!
//! A step that has shipped is never edited: a change is a new step. That
//! includes an index or trigger whose definition changes, which a new step
//! drops and creates again — `CREATE … IF NOT EXISTS` would keep the old
//! definition on every existing database, silently.
//!
//! ## Steps 1 to 5: the schema before it was numbered
//!
//! Until 0.6 only data migrations were numbered. The tables, indexes and
//! triggers were created with `IF NOT EXISTS` and columns added when missing
//! on every start, whatever `user_version` said, so a database at a given
//! number can have the shape of any build since that step. Steps 1 to 5
//! therefore bring whatever they find up to the last unnumbered shape first,
//! with [`legacy_shape`], which is idempotent and frozen: it is what every
//! database created before 0.6 converges to, and it never changes again.
//!
//! ## Steps 6 onwards
//!
//! Each writes exactly the shape it wants. A table whose definition changes is
//! rebuilt — created anew, filled from the old one, the old one dropped and
//! the new one renamed — which also gives every database the same definition
//! text, column order and all, however it got there.
//!
//! ## Why not `sqlx::migrate!`
//!
//! sqlx's migrator keeps its own `_sqlx_migrations` table with a checksum per
//! file, which no existing database has. Bridging to it means detecting which
//! of the many unnumbered shapes a database is in and forging applied rows for
//! it — the same detection the steps above do, plus a second bookkeeping
//! table that could disagree with `user_version`. Its SQLite driver also opens
//! a deferred transaction per migration and takes no lock, so two processes
//! starting together could both apply one. And the steps here are not all
//! plain SQL: bringing an old shape up adds columns only where they are
//! missing, and the foreign-key step counts and logs what it removes. So the
//! steps are a table of Rust code, numbered by `user_version`.

use sqlx::{Connection, Row, SqliteConnection};

use crate::error::{Error, Result};

/// One numbered schema change.
struct Migration {
    /// The `user_version` a database has once this step is applied: the
    /// step's position in [`MIGRATIONS`], counting from 1.
    version: i64,
    /// What it does, for the log.
    description: &'static str,
    /// The change, run inside the step's transaction by [`apply`].
    step: Step,
}

/// Every schema change, in order. Append only.
const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "seed the default feed from a database that predates feeds",
        step: Step::SeedDefaultFeed,
    },
    Migration {
        version: 2,
        description: "index the tags of versions stored before the tag index",
        step: Step::IndexTags,
    },
    Migration {
        version: 3,
        description: "lower-case the pre-release label of versions stored before 0.5.0",
        step: Step::LowerCasePrereleaseKeys,
    },
    Migration {
        version: 4,
        description: "fill the search index for versions stored before it",
        step: Step::FillSearchIndex,
    },
    Migration {
        version: 5,
        description: "bring the tables of an unnumbered schema up to date",
        step: Step::LegacyShape,
    },
    Migration {
        version: 6,
        description: "drop the columns of `packages` that nothing reads",
        step: Step::DropDeadColumns,
    },
];

/// What a [`Migration`] runs. An enum rather than a function pointer in the
/// table: a boxed future borrowing the connection for any lifetime is more
/// than sqlx's executor bounds can prove `Send`.
#[derive(Debug, Clone, Copy)]
enum Step {
    SeedDefaultFeed,
    IndexTags,
    LowerCasePrereleaseKeys,
    FillSearchIndex,
    LegacyShape,
    DropDeadColumns,
}

async fn apply(step: Step, c: &mut SqliteConnection) -> Result<()> {
    match step {
        Step::SeedDefaultFeed => seed_default_feed(c).await,
        Step::IndexTags => index_tags(c).await,
        Step::LowerCasePrereleaseKeys => lower_case_prerelease_keys(c).await,
        Step::FillSearchIndex => fill_search_index(c).await,
        Step::LegacyShape => legacy_shape(c).await,
        Step::DropDeadColumns => drop_dead_columns(c).await,
    }
}

/// The schema version this build migrates to.
pub(super) const LATEST: i64 = MIGRATIONS.len() as i64;

/// Bring the schema up to date.
pub(super) async fn migrate(conn: &mut SqliteConnection) -> Result<()> {
    migrate_to(conn, LATEST).await
}

/// Apply every step up to and including `target`.
pub(super) async fn migrate_to(conn: &mut SqliteConnection, target: i64) -> Result<()> {
    // Nothing to do, the usual case: no need to queue for the write lock.
    if user_version(conn).await? == LATEST {
        return Ok(());
    }
    for step in MIGRATIONS.iter().take_while(|m| m.version <= target) {
        // `BEGIN IMMEDIATE` takes the write lock before `user_version` is
        // read. A deferred transaction only asks for it at the first write,
        // and a reader upgrading to a writer while another connection holds
        // the lock gets `SQLITE_BUSY` at once, without waiting out the busy
        // timeout: the second of two processes started together failed to
        // open the database. Taken up front, the loser waits, then reads the
        // version the winner wrote and has nothing left to do.
        let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
        let current: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut *tx)
            .await?;
        check_known(current)?;
        if current >= step.version {
            continue;
        }
        apply(step.step, &mut tx).await?;
        // `PRAGMA` takes no bound parameters; the number is from the table.
        sqlx::query(&format!("PRAGMA user_version = {}", step.version))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        if current > 0 {
            tracing::info!(
                version = step.version,
                "database migrated: {}",
                step.description
            );
        }
    }
    Ok(())
}

async fn user_version(conn: &mut SqliteConnection) -> Result<i64> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *conn)
        .await?;
    check_known(version)?;
    Ok(version)
}

/// Refuse a database a newer release has migrated: this build does not know
/// what its later steps changed, and writing to it would be a guess.
fn check_known(version: i64) -> Result<()> {
    if version > LATEST {
        return Err(Error::Other(anyhow::anyhow!(
            "the database is at schema version {version}, but this build of YANuget only \
             knows versions up to {LATEST}; it was opened by a newer release, so run that \
             one (or restore a backup taken before the upgrade)"
        )));
    }
    Ok(())
}

// --- steps 1 to 5: the schema before it was numbered ---

/// The last unnumbered shape of the tables: frozen. A change to the schema
/// goes in a new step, never here.
const LEGACY_TABLES: &str = r#"
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

CREATE TABLE IF NOT EXISTS symbols (
    ssqp_key           TEXT NOT NULL,   -- upper-case {GUID}{age}
    filename           TEXT NOT NULL,   -- the .pdb file name, lower-cased
    lower_id           TEXT NOT NULL,   -- owning package, for cleanup
    normalized_version TEXT NOT NULL,
    PRIMARY KEY (ssqp_key, filename)
);

-- One row per (version, lower-cased tag): what the tag filter and the tag
-- cloud read, as index lookups rather than a JSON scan of every package on
-- every page view. `packages.tags` keeps the tags as pushed, for display.
CREATE TABLE IF NOT EXISTS package_tags (
    lower_id           TEXT NOT NULL,
    normalized_version TEXT NOT NULL,
    tag                TEXT NOT NULL,
    PRIMARY KEY (lower_id, normalized_version, tag)
);

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
"#;

/// The indexes and triggers of the last unnumbered shape: frozen, like
/// [`LEGACY_TABLES`]. Separate because some of them cover columns that older
/// databases lack, which [`legacy_shape`] adds in between.
const LEGACY_INDEXES: &str = r#"
CREATE INDEX IF NOT EXISTS idx_packages_lower_id ON packages (lower_id);
-- Covers the search ranking: the (feed, lower_id) prefix scopes a feed and
-- supports GROUP BY lower_id, while including `downloads` lets SUM(downloads)
-- be read straight from the index instead of looking up each table row.
CREATE INDEX IF NOT EXISTS idx_feed_packages_rank
    ON feed_packages (feed, lower_id, downloads);
CREATE INDEX IF NOT EXISTS idx_feed_packages_pkg
    ON feed_packages (lower_id, normalized_version);
CREATE INDEX IF NOT EXISTS idx_symbols_owner
    ON symbols (lower_id, normalized_version);
CREATE INDEX IF NOT EXISTS idx_package_tags_tag ON package_tags (tag, lower_id);
CREATE INDEX IF NOT EXISTS idx_package_files_blob ON package_files (sha256);
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
-- The (feed, lower_id) index of the first feeds build, subsumed by
-- `idx_feed_packages_rank`.
DROP INDEX IF EXISTS idx_feed_packages_feed;
-- 0.1's search index over `packages.listed`, which nothing has read since
-- feeds took over listing, and which nothing dropped either.
DROP INDEX IF EXISTS idx_packages_search;
"#;

/// Bring a database from before numbered steps to the last unnumbered shape:
/// create what is missing, add the columns older builds did not have. What
/// every unnumbered start did, and idempotent like it.
async fn legacy_shape(c: &mut SqliteConnection) -> Result<()> {
    sqlx::raw_sql(LEGACY_TABLES).execute(&mut *c).await?;
    // The admin `enabled` flag, from before feeds.
    ensure_column(c, "packages", "enabled", "INTEGER NOT NULL DEFAULT 1").await?;
    // The requireLicenseAcceptance passthrough. `false` is exactly what those
    // rows were reported as.
    ensure_column(
        c,
        "packages",
        "require_license_acceptance",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    // Per-feed download counts: the first feeds build kept them on
    // `packages` only, and nothing ever added the column to its
    // `feed_packages`, so such a database failed step 1 until now.
    ensure_column(
        c,
        "feed_packages",
        "downloads",
        "INTEGER NOT NULL DEFAULT 0",
    )
    .await?;
    // Pins: nothing was pinned.
    ensure_column(c, "feed_packages", "pinned", "INTEGER NOT NULL DEFAULT 0").await?;
    sqlx::raw_sql(LEGACY_INDEXES).execute(&mut *c).await?;
    Ok(())
}

/// Step 1: seed each package of a single-feed database, created before feeds
/// existed, into the implicit `default` feed, copying its state. `OR IGNORE`
/// because the first feeds build seeded some without numbering it.
async fn seed_default_feed(c: &mut SqliteConnection) -> Result<()> {
    legacy_shape(c).await?;
    sqlx::query(
        r#"INSERT OR IGNORE INTO feed_packages
               (feed, lower_id, normalized_version, listed, enabled, pending,
                flagged, flag_reason, added, downloads)
           SELECT 'default', lower_id, normalized_version, listed, enabled, 0, 0, NULL,
                  published, downloads
           FROM packages"#,
    )
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Step 2: index the tags of every package stored before `package_tags`
/// existed.
async fn index_tags(c: &mut SqliteConnection) -> Result<()> {
    legacy_shape(c).await?;
    sqlx::query(
        "INSERT OR IGNORE INTO package_tags (lower_id, normalized_version, tag) \
         SELECT p.lower_id, p.normalized_version, lower(substr(trim(je.value), 1, 64)) \
         FROM packages p, json_each(p.tags) je \
         WHERE trim(je.value) <> '' AND je.key < 64",
    )
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Step 3: rewrite every stored normalized version to its lower-cased form.
///
/// 0.5.0 began lower-casing the pre-release label in
/// [`NuGetVersion::normalized`](crate::version::NuGetVersion::normalized), but
/// rows written earlier as `1.0.0-Beta` kept their case. Listings still found
/// them while every exact lookup (download, delete, unlist, retention) bound
/// the lower-cased key and missed, and a re-push created a second row sharing
/// the one lower-cased file on disk.
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
async fn lower_case_prerelease_keys(c: &mut SqliteConnection) -> Result<()> {
    legacy_shape(c).await?;
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
        sqlx::query(step).execute(&mut *c).await?;
    }
    Ok(())
}

/// Step 4: fill the search index for the versions stored before it existed.
/// Rebuilt from scratch.
async fn fill_search_index(c: &mut SqliteConnection) -> Result<()> {
    legacy_shape(c).await?;
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
    ] {
        sqlx::query(step).execute(&mut *c).await?;
    }
    Ok(())
}

/// Add a column to a table unless it has one of that name (SQLite has no
/// `ADD COLUMN IF NOT EXISTS`).
///
/// Every name is `&'static str` on purpose: neither `PRAGMA` nor `ALTER TABLE`
/// takes bound parameters, so all three have to be interpolated. Requiring
/// literals keeps that safe by construction rather than by convention — a caller
/// cannot reach this with a request-supplied name without changing the signature
/// first. It is also exactly what sqlx 0.9's injection guard will want.
///
/// Only ever called inside a step's transaction, which holds the write lock,
/// so no other process can add the column between the check and the `ALTER`.
async fn ensure_column(
    c: &mut SqliteConnection,
    table: &'static str,
    column: &'static str,
    def: &'static str,
) -> Result<()> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(&mut *c)
        .await?;
    let exists = rows
        .iter()
        .any(|r| r.get::<String, _>("name").eq_ignore_ascii_case(column));
    if !exists {
        sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {column} {def}"))
            .execute(&mut *c)
            .await?;
    }
    Ok(())
}

// --- steps 6 onwards ---

/// Step 6: rebuild `packages` without the columns nothing reads.
///
/// `listed`, `enabled` and `downloads` are each feed's own and have lived on
/// `feed_packages` since feeds; the copies here were written and never read.
/// The split version core (`version_major` … `version_revision`) was meant
/// for ordering in SQL, which cannot order pre-releases anyway and never did
/// it. The index on `lower_id` alone duplicated the primary key's, and 0.1's
/// index over `listed` goes with the table.
///
/// A rebuild rather than `ALTER TABLE … DROP COLUMN`: the columns 0.1 and
/// 0.4 added with `ADD COLUMN` sit at the end of their table, so only a
/// rebuild gives every database the same `packages`. Nothing references the
/// table yet (foreign keys come in step 7), so dropping it is safe with
/// enforcement on; its search triggers go with it and are created again.
async fn drop_dead_columns(c: &mut SqliteConnection) -> Result<()> {
    sqlx::raw_sql(concat!(
        "CREATE TABLE packages_new (",
        packages_columns!(),
        ");
        INSERT INTO packages_new (
            id, lower_id, normalized_version, original_version, is_prerelease, is_semver2,
            authors, description, icon_url, license_url, license_expression, project_url,
            repository_url, repository_type, min_client_version, release_notes, language,
            title, summary, tags, has_readme, has_embedded_icon, is_development_dependency,
            require_license_acceptance, package_size, package_hash, package_hash_algorithm,
            published, package_types, dependencies)
        SELECT
            id, lower_id, normalized_version, original_version, is_prerelease, is_semver2,
            authors, description, icon_url, license_url, license_expression, project_url,
            repository_url, repository_type, min_client_version, release_notes, language,
            title, summary, tags, has_readme, has_embedded_icon, is_development_dependency,
            require_license_acceptance, package_size, package_hash, package_hash_algorithm,
            published, package_types, dependencies
        FROM packages;
        DROP TABLE packages;
        ALTER TABLE packages_new RENAME TO packages;",
        search_triggers!(),
    ))
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// The columns of `packages` from step 6 on.
macro_rules! packages_columns {
    () => {
        "
    id                         TEXT    NOT NULL,
    lower_id                   TEXT    NOT NULL,
    normalized_version         TEXT    NOT NULL,
    original_version           TEXT    NOT NULL,
    is_prerelease              INTEGER NOT NULL,
    is_semver2                 INTEGER NOT NULL,
    authors                    TEXT    NOT NULL,
    description                TEXT    NOT NULL,
    icon_url                   TEXT,
    license_url                TEXT,
    license_expression         TEXT,
    project_url                TEXT,
    repository_url             TEXT,
    repository_type            TEXT,
    min_client_version         TEXT,
    release_notes              TEXT,
    language                   TEXT,
    title                      TEXT,
    summary                    TEXT,
    tags                       TEXT    NOT NULL,
    has_readme                 INTEGER NOT NULL,
    has_embedded_icon          INTEGER NOT NULL,
    is_development_dependency  INTEGER NOT NULL,
    require_license_acceptance INTEGER NOT NULL,
    package_size               INTEGER NOT NULL,
    package_hash               TEXT    NOT NULL,
    package_hash_algorithm     TEXT    NOT NULL,
    published                  TEXT    NOT NULL,
    package_types              TEXT    NOT NULL,
    dependencies               TEXT    NOT NULL,
    PRIMARY KEY (lower_id, normalized_version)
"
    };
}
use packages_columns;

/// The triggers that keep the search index in step with `packages`, as step 6
/// creates them. Their bodies are those of the unnumbered shape.
macro_rules! search_triggers {
    () => {
        "
CREATE TRIGGER packages_search_insert AFTER INSERT ON packages BEGIN
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
CREATE TRIGGER packages_search_delete AFTER DELETE ON packages BEGIN
    DELETE FROM search_text WHERE rowid = (
        SELECT id FROM search_keys
        WHERE lower_id = old.lower_id AND normalized_version = old.normalized_version);
    DELETE FROM search_keys
        WHERE lower_id = old.lower_id AND normalized_version = old.normalized_version;
END;
CREATE TRIGGER packages_search_update
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
"
    };
}
use search_triggers;

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;

    #[test]
    fn steps_are_numbered_in_order_from_one() {
        for (i, step) in MIGRATIONS.iter().enumerate() {
            assert_eq!(step.version, i as i64 + 1, "{}", step.description);
        }
    }

    #[tokio::test]
    async fn ensure_column_adds_a_column_once() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Keep", "1.0.0"))
            .await
            .unwrap();
        let mut conn = pool(&db).acquire().await.unwrap();
        ensure_column(&mut conn, "packages", "extra_col", "TEXT")
            .await
            .unwrap();
        ensure_column(&mut conn, "packages", "extra_col", "TEXT")
            .await
            .unwrap();
        drop(conn);
        assert!(db
            .find(FEED, "keep", &NuGetVersion::parse("1.0.0").unwrap())
            .await
            .unwrap()
            .is_some());
    }
}
