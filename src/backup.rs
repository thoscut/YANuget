//! `yanuget backup` and `yanuget restore`: a consistent copy of everything the
//! server keeps, taken while it runs, and the way back.
//!
//! A backup is a directory:
//!
//! ```text
//! {dest}/
//!     backup.json     the manifest, written last: no manifest, no backup
//!     yanuget.db      a snapshot of the database (`VACUUM INTO`), one file
//!     packages/       the package store, file for file
//!     tls/            the self-signed certificate pair, when there is one
//! ```
//!
//! The database is snapshotted first and the store copied second, so a push
//! that lands in between only leaves an extra file. What can go wrong is the
//! other direction: a delete, an overwrite or a retention run that removes a
//! file the snapshot still lists. So once the copy is done, every file the
//! snapshot references is checked in the backup, and if one is missing or has
//! the wrong size the whole pass runs again: a fresh snapshot, and a copy that
//! only moves what changed, which takes seconds. The files this run copied
//! are hashed too, since an overwrite can keep the size; the ones it skipped
//! were already there before the snapshot. So are the referenced files the
//! live store no longer has: the backup's copy is from some earlier run, and
//! the version may have been overwritten since. A file still wrong after that
//! is wrong in the live store too, and is reported, not hidden.
//!
//! Re-running into the same directory is incremental, like `rsync`: a stored
//! file is never rewritten in place (the store renames new content over the
//! old), so a file whose size and modification time match is already there.
//! Every file is copied to a temp name, synced and renamed, so the backup never
//! holds a partial file, and files that left the live store are removed only
//! after the new snapshot is in place: an interrupted run leaves the previous
//! snapshot with a superset of its files, and no manifest. A file the new
//! snapshot references is never removed: when the live store has lost it, the
//! backup's copy from an earlier run is the last one there is. It is kept,
//! and logged, rather than deleted after the check had counted it present.
//!
//! Nothing in the backup directory is written through a link: temp files
//! are created anew under fresh names, a directory link is refused, and the
//! snapshot is made and checked in a directory only this user can enter.
//! That holds against links left there, not against someone swapping
//! directories while a run is under way; the directory should be writable
//! by the user `backup` runs as and nobody else.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use indicatif::{HumanBytes, ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde::{Deserialize, Serialize};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{ConnectOptions, Connection, Row, SqliteConnection};

use crate::config::Config;

/// The manifest's file name inside a backup directory.
pub const MANIFEST: &str = "backup.json";
/// The database snapshot's file name inside a backup directory.
const DATABASE: &str = "yanuget.db";
/// The package store's directory inside a backup directory.
const PACKAGES: &str = "packages";
/// The self-signed TLS pair's directory, in a backup and in `data_dir`.
const TLS: &str = "tls";
/// Bumped when the layout changes in a way an older `restore` cannot read.
const FORMAT: u32 = 1;
/// How many snapshot-and-copy passes a backup makes before it settles for
/// reporting what is still missing.
const ATTEMPTS: usize = 3;
/// Suffix of the temp files a copy is staged in, next to its destination.
const TEMP_SUFFIX: &str = ".backup-tmp";

/// Directories of the package store that hold work in progress, not data:
/// in-flight and resumable uploads, and a running migration's downloads.
const SCRATCH_DIRS: [&str; 2] = [".uploads", ".migrate"];

/// What `backup.json` records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// Layout version; see `FORMAT`.
    pub format: u32,
    /// The `yanuget` version that wrote the backup.
    pub yanuget_version: String,
    /// When the database snapshot was taken.
    pub created: chrono::DateTime<chrono::Utc>,
    /// Package versions in the snapshot.
    pub packages: u64,
    /// Files in the backed-up package store.
    pub files: u64,
    /// Their total size in bytes.
    pub bytes: u64,
    /// Files the snapshot references that the live store did not have, or
    /// held with other bytes than it recorded.
    pub missing: Vec<String>,
    /// Whether the self-signed TLS pair is included.
    pub tls: bool,
}

/// What a backup did.
#[derive(Debug, Clone)]
pub struct BackupSummary {
    pub manifest: Manifest,
    /// Files copied by this run (the rest were already in place).
    pub copied: u64,
    /// Bytes copied by this run.
    pub copied_bytes: u64,
    /// Files removed from the backup because the live store no longer has them.
    pub removed: u64,
    /// Files the snapshot references that the live store no longer has, kept
    /// from an earlier run's copy: relative to the package store.
    pub kept: Vec<String>,
    /// Snapshot-and-copy passes it took.
    pub attempts: usize,
}

/// What a restore did.
#[derive(Debug, Clone)]
pub struct RestoreSummary {
    pub manifest: Manifest,
    /// Files copied into the package store.
    pub copied: u64,
    /// Files removed from the package store because the backup does not have them.
    pub removed: u64,
}

/// Back up the server `config` describes into `dest`, while it runs.
pub async fn backup(
    config: &Config,
    dest: &Path,
    draw: ProgressDrawTarget,
) -> anyhow::Result<BackupSummary> {
    let live_db = PathBuf::from(config.database_path());
    if !live_db.is_file() {
        // A relative path is fine for a server run from its own directory,
        // and a backup only reads; but run from anywhere else it finds
        // nothing, and the reason should be on screen.
        let hint = if live_db.is_relative() {
            format!(
                " (relative to the current directory {}; {ABSOLUTE_HINT})",
                cwd()
            )
        } else {
            String::new()
        };
        bail!(
            "no database at {}{hint}: nothing to back up",
            live_db.display()
        );
    }
    std::fs::create_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    // A backup inside the store would copy itself, and one inside the data
    // directory would go with it when that disk does.
    if let (Ok(d), Ok(data)) = (dest.canonicalize(), config.data_dir.canonicalize()) {
        if d.starts_with(&data) {
            bail!(
                "{} is inside the data directory {}; back up somewhere else",
                dest.display(),
                data.display()
            );
        }
    }
    // The store may live outside the data directory; either one inside the
    // other makes the copy recurse into itself.
    check_apart(dest, &config.storage_path(), "back up somewhere else")?;
    let _lock = lock(dest)?;

    // From here until the end the directory is not a backup.
    remove_if_present(&dest.join(MANIFEST))?;
    remove_stale_temps(dest)?;

    let progress = progress_bar(draw);
    let packages = dest.join(PACKAGES);
    let private = PrivateDir::create(dest)?;
    let snapshot = private.path.join(DATABASE);
    let (mut copied, mut copied_bytes) = (0, 0);
    // Every file any pass copied: the ones whose content is new to the backup.
    let mut fresh = HashSet::new();
    let mut attempts = 0;
    let (tree, Checked { missing, present }, created) = loop {
        attempts += 1;
        progress.set_message(format!("pass {attempts}: snapshotting the database"));
        let created = chrono::Utc::now();
        snapshot_database(&live_db, &snapshot).await?;

        progress.set_message(format!("pass {attempts}: copying the package store"));
        let tree = {
            let (src, dst, bar) = (config.storage_path(), packages.clone(), progress.clone());
            tokio::task::spawn_blocking(move || {
                copy_tree(
                    &src,
                    &dst,
                    &SCRATCH_DIRS,
                    Existing::KeepUnchanged,
                    Links::Refuse,
                    &bar,
                )
            })
            .await??
        };
        copied += tree.copied;
        copied_bytes += tree.copied_bytes;
        fresh.extend(tree.fresh.iter().cloned());

        progress.set_message(format!("pass {attempts}: checking the copy"));
        let checked = check(&snapshot, &packages, &fresh, Some(&tree.seen)).await?;
        if checked.missing.is_empty() || attempts == ATTEMPTS {
            break (tree, checked, created);
        }
        tracing::info!(
            missing = checked.missing.len(),
            "files changed while copying; taking a fresh snapshot"
        );
    };

    let stats = finish_snapshot(&snapshot).await?;
    std::fs::rename(&snapshot, dest.join(DATABASE))?;
    sync_dir(dest);
    drop(private);

    // What the check found present stays, whether or not the live store
    // still has it: the check ran before this, and a file it counted must
    // not go afterwards.
    let mut kept: Vec<String> = present
        .difference(&tree.seen)
        .map(|rel| rel.to_string_lossy().into_owned())
        .collect();
    kept.sort();
    let kept_bytes: u64 = kept
        .iter()
        .filter_map(|rel| std::fs::metadata(packages.join(rel)).ok())
        .map(|m| m.len())
        .sum();
    if !kept.is_empty() {
        tracing::warn!(
            files = kept.len(),
            first = %kept[0],
            "the live store lacks files its database references; the backup keeps \
             its copies from an earlier run"
        );
    }
    progress.set_message("removing files the live store no longer has");
    let removed = {
        let (dst, mut keep) = (packages.clone(), tree.seen);
        keep.extend(present);
        tokio::task::spawn_blocking(move || prune(&dst, &keep)).await??
    };

    let live_tls = config.data_dir.join(TLS);
    let tls = live_tls.is_dir();
    if tls {
        let (src, dst, bar) = (live_tls, dest.join(TLS), progress.clone());
        let t = tokio::task::spawn_blocking(move || {
            copy_tree(
                &src,
                &dst,
                &[],
                Existing::KeepUnchanged,
                Links::Refuse,
                &bar,
            )
        })
        .await??;
        let dst = dest.join(TLS);
        tokio::task::spawn_blocking(move || prune(&dst, &t.seen)).await??;
    } else {
        remove_dir_if_present(&dest.join(TLS))?;
    }
    progress.finish_and_clear();

    let manifest = Manifest {
        format: FORMAT,
        yanuget_version: env!("CARGO_PKG_VERSION").to_string(),
        created,
        packages: stats,
        files: tree.files + kept.len() as u64,
        bytes: tree.bytes + kept_bytes,
        missing,
        tls,
    };
    write_manifest(dest, &manifest)?;
    Ok(BackupSummary {
        manifest,
        copied,
        copied_bytes,
        removed,
        kept,
        attempts,
    })
}

/// Restore the backup in `src` into the locations `config` describes.
///
/// The server must not be running: the database file is replaced underneath
/// it. Unless `force` is set, the target must be empty (no database, no
/// packages); with it, the package store is made to match the backup exactly.
pub async fn restore(
    config: &Config,
    src: &Path,
    force: bool,
    draw: ProgressDrawTarget,
) -> anyhow::Result<RestoreSummary> {
    let manifest = read_manifest(src)?;
    let backup_db = src.join(DATABASE);
    if !backup_db.is_file() {
        bail!("{} is missing from the backup", backup_db.display());
    }
    check_absolute(config, manifest.tls)?;

    let live_db = PathBuf::from(config.database_path());
    let storage = config.storage_path();
    if !force {
        let mut existing = Vec::new();
        if live_db.exists() {
            existing.push(live_db.display().to_string());
        }
        if has_content(&storage, &SCRATCH_DIRS)? {
            existing.push(storage.display().to_string());
        }
        if !existing.is_empty() {
            bail!(
                "the target already holds data ({}); pass --force to replace it with the backup",
                existing.join(", ")
            );
        }
    }

    // Copying a backup inside the store onto the store would recurse, and the
    // prune afterwards would delete the backup itself.
    std::fs::create_dir_all(&storage).with_context(|| format!("creating {}", storage.display()))?;
    check_apart(src, &storage, "restore from somewhere else")?;

    // The files first and the database last: until the database is in place
    // the old one (or none) is what a start would open.
    let progress = progress_bar(draw);
    progress.set_message("copying the package store");
    let tree = {
        let (from, to, bar) = (src.join(PACKAGES), storage.clone(), progress.clone());
        tokio::task::spawn_blocking(move || {
            copy_tree(&from, &to, &[], Existing::Replace, Links::Follow, &bar)
        })
        .await??
    };
    let removed = {
        let (to, seen) = (storage.clone(), tree.seen);
        tokio::task::spawn_blocking(move || prune(&to, &seen)).await??
    };
    if manifest.tls {
        let (from, to, bar) = (src.join(TLS), config.data_dir.join(TLS), progress.clone());
        tokio::task::spawn_blocking(move || {
            copy_tree(&from, &to, &[], Existing::Replace, Links::Follow, &bar)
        })
        .await??;
    }
    progress.finish_and_clear();

    if let Some(parent) = live_db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let staged = with_suffix(&live_db, TEMP_SUFFIX);
    copy_file(&backup_db, &staged)?;
    // A write-ahead log left by the old database would be replayed into the
    // restored one on the next open, corrupting it.
    remove_sidecars(&live_db)?;
    std::fs::rename(&staged, &live_db)?;
    if let Some(parent) = live_db.parent() {
        sync_dir(parent);
    }

    // Open it the way the server does, which brings an older backup's schema
    // up to date and proves the file is a database this version can serve.
    crate::database::SqliteDatabase::connect(&config.database_path())
        .await
        .context("opening the restored database")?;

    Ok(RestoreSummary {
        manifest,
        copied: tree.copied,
        removed,
    })
}

/// How to set the data directory so a command run by hand finds it: the end
/// of a sentence that names the problem.
const ABSOLUTE_HINT: &str = "set an absolute `data_dir` in the configuration file, e.g. \
    `data_dir = \"/var/lib/yanuget\"`, and pass that file with `--config`, or set \
    YANUGET_DATA_DIR";

fn cwd() -> String {
    std::env::current_dir()
        .map(|d| d.display().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// Refuse to restore into a relative path. It resolves against the current
/// directory, which for `sudo -u yanuget yanuget restore` is wherever the
/// operator stands, not the service's working directory, and the restore
/// would write there and report success.
fn check_absolute(config: &Config, tls: bool) -> anyhow::Result<()> {
    let (store, db) = (config.storage_path(), PathBuf::from(config.database_path()));
    let mut targets = vec![
        (config.storage_path.is_some(), "storage_path", store),
        (config.database_path.is_some(), "database_path", db),
    ];
    if tls {
        targets.push((false, "data_dir", config.data_dir.join(TLS)));
    }
    for (overridden, key, path) in targets {
        if path.is_absolute() {
            continue;
        }
        if overridden {
            bail!(
                "{key} is the relative path {}, which would restore into the current \
                 directory ({}); set an absolute `{key}` in the configuration file and \
                 pass that file with `--config`, or set YANUGET_{}",
                path.display(),
                cwd(),
                key.to_ascii_uppercase()
            );
        }
        bail!(
            "data_dir is the relative path {}, which would restore into the current \
             directory ({}); {ABSOLUTE_HINT}",
            config.data_dir.display(),
            cwd()
        );
    }
    Ok(())
}

/// Where the data `config` describes lives, for the operator to read: the
/// data directory, or the store and the database when either is set apart.
pub fn target(config: &Config) -> String {
    if config.storage_path.is_none() && config.database_path.is_none() {
        config.data_dir.display().to_string()
    } else {
        format!(
            "{} and {}",
            config.storage_path().display(),
            config.database_path()
        )
    }
}

/// Refuse a backup directory and a package store that contain each other.
/// Paths that do not resolve cannot overlap anything yet.
fn check_apart(backup: &Path, storage: &Path, advice: &str) -> anyhow::Result<()> {
    if let (Ok(b), Ok(s)) = (backup.canonicalize(), storage.canonicalize()) {
        if b.starts_with(&s) || s.starts_with(&b) {
            bail!(
                "the backup {} and the package store {} must not contain each other; {advice}",
                backup.display(),
                storage.display()
            );
        }
    }
    Ok(())
}

/// Read and check a backup's manifest.
pub fn read_manifest(dir: &Path) -> anyhow::Result<Manifest> {
    let path = dir.join(MANIFEST);
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "{} not found: {} is not a complete backup (an interrupted `yanuget backup` leaves none)",
            path.display(),
            dir.display()
        )
    })?;
    let manifest: Manifest =
        serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    if manifest.format > FORMAT {
        bail!(
            "the backup was written by yanuget {} in a newer format ({}); restore it with that version or later",
            manifest.yanuget_version,
            manifest.format
        );
    }
    Ok(manifest)
}

/// Write the manifest through a temp file of a fresh name, created anew:
/// with a fixed name, a link planted there (to the live database, say) would
/// be followed and its target overwritten with the manifest (`SEC-54`).
fn write_manifest(dest: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    let staged = dest.join(format!(
        "{MANIFEST}.{}{TEMP_SUFFIX}",
        uuid::Uuid::new_v4().simple()
    ));
    let written = (|| -> anyhow::Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        file.write_all(serde_json::to_string_pretty(manifest)?.as_bytes())?;
        file.sync_all()?;
        Ok(std::fs::rename(&staged, dest.join(MANIFEST))?)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written?;
    sync_dir(dest);
    Ok(())
}

/// Hold an exclusive lock on the backup directory, so two runs into the same
/// directory (a cron job overlapping a slow one) cannot interleave. A link
/// in the lock file's place is not followed, which would create or lock a
/// file elsewhere.
fn lock(dest: &Path) -> anyhow::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let path = dest.join(".backup.lock");
    let file = options
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    if fs4::FileExt::try_lock(&file).is_err() {
        bail!("another `yanuget backup` is writing to {}", dest.display());
    }
    Ok(file)
}

/// Remove what an interrupted run left at the top of the backup directory:
/// temp files and private directories. Removing a link removes the link.
fn remove_stale_temps(dest: &Path) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dest)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().ends_with(TEMP_SUFFIX) {
            continue;
        }
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            remove_dir_if_present(&path)?;
        } else {
            remove_if_present(&path)?;
        }
    }
    Ok(())
}

/// A directory of a fresh name in the backup directory that only this user
/// can enter, removed again when dropped. The snapshot is taken, checked and
/// finished in it: beside a snapshot in the backup directory itself, which
/// may be a share others write to, a `-journal` or `-wal` could be laid down
/// again after it was removed, and SQLite would replay it into what it reads;
/// and a link laid where the snapshot is about to be written would be
/// followed (`SEC-54`).
struct PrivateDir {
    path: PathBuf,
}

impl PrivateDir {
    fn create(dest: &Path) -> anyhow::Result<Self> {
        let path = dest.join(format!(
            ".snapshot.{}{TEMP_SUFFIX}",
            uuid::Uuid::new_v4().simple()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        // `mkdir` neither follows a link in its place nor accepts an
        // existing directory.
        builder
            .create(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = remove_dir_if_present(&self.path);
    }
}

// --- the database ---

/// Write a consistent snapshot of the live database to `to`.
///
/// `VACUUM INTO` reads inside one transaction, so it sees one committed state
/// however many writes the server makes meanwhile, and does not block them.
async fn snapshot_database(live: &Path, to: &Path) -> anyhow::Result<()> {
    remove_if_present(to)?;
    remove_sidecars(to)?;
    let mut conn = SqliteConnectOptions::new()
        .filename(live)
        .create_if_missing(false)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(30))
        .connect()
        .await
        .with_context(|| format!("opening {}", live.display()))?;
    sqlx::query("VACUUM INTO ?")
        .bind(to.to_string_lossy().into_owned())
        .execute(&mut conn)
        .await
        .context("snapshotting the database")?;
    conn.close().await?;
    Ok(())
}

/// Open the snapshot in the backup directory. That directory is often a
/// share other systems write to, and SQLite replays a `-wal` or hot
/// `-journal` it finds beside a database into what it reads, so whatever lies
/// there is removed first, as `restore` does beside the live database. The
/// snapshot is a single file whenever no connection to it is open.
async fn open_snapshot(path: &Path) -> anyhow::Result<SqliteConnection> {
    remove_sidecars(path)?;
    SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .connect()
        .await
        .with_context(|| format!("opening {}", path.display()))
}

/// Make the snapshot a self-contained backup: drop the resumable uploads (their
/// partial bytes are not backed up, so a restored server would offer to resume
/// from data it does not have), check it, and leave it a single file. Returns
/// how many package versions it holds.
async fn finish_snapshot(path: &Path) -> anyhow::Result<u64> {
    let mut conn = open_snapshot(path).await?;
    sqlx::query("DELETE FROM uploads")
        .execute(&mut conn)
        .await?;
    let check: String = sqlx::query_scalar("PRAGMA quick_check")
        .fetch_one(&mut conn)
        .await?;
    if check != "ok" {
        bail!("the database snapshot fails its integrity check: {check}");
    }
    sqlx::query("PRAGMA journal_mode = DELETE")
        .execute(&mut conn)
        .await?;
    let packages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM packages")
        .fetch_one(&mut conn)
        .await?;
    conn.close().await?;
    // `close` flushed it; sync the file itself before it is renamed into place.
    std::fs::File::open(path)?.sync_all()?;
    Ok(packages as u64)
}

/// The content a database row records for a file.
#[derive(Debug, Clone)]
enum Digest {
    /// A package's `package_hash` under `SHA512`: base64, as NuGet has it.
    Sha512Base64(String),
    /// An attached file's blob: lowercase hex, also its name.
    Sha256Hex(String),
}

/// One file the snapshot references.
struct Expected {
    /// Relative to the store.
    rel: String,
    size: Option<u64>,
    digest: Option<Digest>,
}

/// What [`check`] found.
struct Checked {
    /// See [`verify`].
    missing: Vec<String>,
    /// Every file the snapshot references that `store` holds at the
    /// recorded size, relative to it. A copy kept from an earlier run that
    /// fails its hash is among them as well as in `missing`: what it holds
    /// is not what the snapshot names, but it may be the last copy of
    /// anything there is.
    present: HashSet<PathBuf>,
}

/// Every file the snapshot at `db` references that `store` lacks, or holds
/// with a size other than the one recorded, as a path relative to `store`.
/// The files in `hash` (relative to `store`) must also have the recorded
/// hash, where the row has one this can compute.
///
/// Sizes catch a delete, and nearly every overwrite. Hashing every file would
/// read the whole store on each run and make an incremental backup as slow as
/// the first, so only the files the caller just copied are hashed: their bytes
/// are the ones that may come from a different build than the snapshot's.
pub async fn verify(
    db: &Path,
    store: &Path,
    hash: &HashSet<PathBuf>,
) -> anyhow::Result<Vec<String>> {
    Ok(check(db, store, hash, None).await?.missing)
}

/// [`verify`], and which referenced files it found in place. With `live`,
/// the files the live store had in this pass (relative to `store`), every
/// referenced file outside it is hashed too: the store's copy of it comes
/// from an earlier run, and the version may have been overwritten since
/// with one of the same size (`COR-54`). Normally there are none, so an
/// incremental run stays one.
async fn check(
    db: &Path,
    store: &Path,
    hash: &HashSet<PathBuf>,
    live: Option<&HashSet<PathBuf>>,
) -> anyhow::Result<Checked> {
    let mut conn = open_snapshot(db).await?;
    let mut expected: Vec<Expected> = Vec::new();
    for row in sqlx::query(
        "SELECT lower_id, normalized_version, package_size, package_hash, \
                package_hash_algorithm FROM packages",
    )
    .fetch_all(&mut conn)
    .await?
    {
        let id = row.try_get::<String, _>(0)?.to_ascii_lowercase();
        let version = row.try_get::<String, _>(1)?.to_ascii_lowercase();
        let size: i64 = row.try_get(2)?;
        let algorithm: String = row.try_get(4)?;
        let digest = if algorithm.eq_ignore_ascii_case("SHA512") {
            Some(Digest::Sha512Base64(row.try_get(3)?))
        } else {
            None
        };
        expected.push(Expected {
            rel: format!("{id}/{version}/{id}.{version}.nupkg"),
            size: Some(size as u64),
            digest,
        });
    }
    for row in sqlx::query("SELECT DISTINCT sha256, size FROM package_files")
        .fetch_all(&mut conn)
        .await?
    {
        let sha: String = row.try_get(0)?;
        let size: i64 = row.try_get(1)?;
        if sha.len() > 2 {
            expected.push(Expected {
                rel: format!(".blobs/sha256/{}/{sha}", &sha[..2]),
                size: Some(size as u64),
                digest: Some(Digest::Sha256Hex(sha.to_ascii_lowercase())),
            });
        }
    }
    for row in sqlx::query("SELECT ssqp_key, filename FROM symbols")
        .fetch_all(&mut conn)
        .await?
    {
        let key = row.try_get::<String, _>(0)?.to_ascii_lowercase();
        let file = row.try_get::<String, _>(1)?.to_ascii_lowercase();
        expected.push(Expected {
            rel: format!(".symbols/{key}/{file}"),
            size: None,
            digest: None,
        });
    }
    conn.close().await?;

    let (store, hash, live) = (store.to_path_buf(), hash.clone(), live.cloned());
    Ok(tokio::task::spawn_blocking(move || {
        let mut out = Checked {
            missing: Vec::new(),
            present: HashSet::new(),
        };
        for e in expected {
            let path = store.join(&e.rel);
            // A link in the backup is no copy, whatever it points at.
            let sized = std::fs::symlink_metadata(&path)
                .is_ok_and(|m| m.is_file() && e.size.is_none_or(|s| s == m.len()));
            let rel = Path::new(&e.rel);
            let hashed = hash.contains(rel) || live.as_ref().is_some_and(|l| !l.contains(rel));
            // A file of the wrong size is not read at all.
            let good = sized
                && match &e.digest {
                    Some(digest) if hashed => has_digest(&path, digest),
                    _ => true,
                };
            if sized {
                out.present.insert(PathBuf::from(&e.rel));
            }
            if !good {
                out.missing.push(e.rel);
            }
        }
        out
    })
    .await?)
}

/// Whether the file at `path` hashes to `digest`. A file that cannot be read
/// does not.
fn has_digest(path: &Path, digest: &Digest) -> bool {
    use base64::Engine;
    fn hash<D: sha2::Digest>(path: &Path) -> Option<Vec<u8>> {
        let mut hasher = D::new();
        crate::streaming::hash_reader(&mut std::fs::File::open(path).ok()?, &mut hasher).ok()?;
        Some(hasher.finalize().to_vec())
    }
    match digest {
        Digest::Sha512Base64(want) => hash::<sha2::Sha512>(path)
            .is_some_and(|got| base64::engine::general_purpose::STANDARD.encode(got) == *want),
        Digest::Sha256Hex(want) => {
            hash::<sha2::Sha256>(path).is_some_and(|got| hex::encode(got) == *want)
        }
    }
}

// --- the files ---

/// What [`copy_tree`] found and did.
struct TreeCopy {
    /// Every file copied or already in place, relative to the root.
    seen: HashSet<PathBuf>,
    /// The ones copied, relative to the root.
    fresh: HashSet<PathBuf>,
    files: u64,
    bytes: u64,
    copied: u64,
    copied_bytes: u64,
}

/// What [`copy_tree`] does with a file `dst` already has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Existing {
    /// Leave it alone when its size and modification time match: what makes
    /// a repeated backup incremental. A file damaged in place keeps both, so
    /// this trusts the destination.
    KeepUnchanged,
    /// Copy over it regardless. A restore makes the store match the backup,
    /// and the live files are the ones it cannot trust.
    Replace,
}

/// Whether [`copy_tree`] writes through a directory link it finds in `dst`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Links {
    /// Refuse to: `dst` is in a backup directory, which may be a share
    /// others write to, and a link planted under it would put files (whose
    /// names and contents a push key chooses) wherever it points (`SEC-54`).
    /// `dst` itself is checked as well.
    Refuse,
    /// Follow it: `dst` is the server's own store, and a link there is the
    /// operator's.
    Follow,
}

/// Create the directory `path` unless it exists, refusing a link in its
/// place. Its parent must exist.
fn real_dir(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(meta) if meta.file_type().is_symlink() => bail!(
            "{} is a symbolic link; a backup does not write through links in its \
             destination",
            path.display()
        ),
        Ok(_) => bail!("{} is not a directory", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(path).with_context(|| format!("creating {}", path.display()))
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Make every regular file under `src` exist under `dst` with the same
/// content, skipping the top-level directories in `skip` and any temp file.
/// What happens to a file already in `dst` is up to `existing`, and to a
/// directory link there up to `links`. Removes nothing; see [`prune`].
fn copy_tree(
    src: &Path,
    dst: &Path,
    skip: &[&str],
    existing: Existing,
    links: Links,
    progress: &ProgressBar,
) -> anyhow::Result<TreeCopy> {
    let make_dir = |path: &Path| match links {
        Links::Refuse => real_dir(path),
        Links::Follow => Ok(std::fs::create_dir_all(path)?),
    };
    let mut out = TreeCopy {
        seen: HashSet::new(),
        fresh: HashSet::new(),
        files: 0,
        bytes: 0,
        copied: 0,
        copied_bytes: 0,
    };
    if !src.is_dir() {
        return Ok(out);
    }
    make_dir(dst)?;
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let entries = match std::fs::read_dir(src.join(&rel)) {
            Ok(entries) => entries,
            // Removed since its parent was listed (a version being deleted).
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context(format!("listing {}", src.join(&rel).display())),
        };
        let mut wrote_here = false;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let child = rel.join(&name);
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if rel.as_os_str().is_empty() && skip.iter().any(|s| name == *s) {
                    continue;
                }
                make_dir(&dst.join(&child))?;
                stack.push(child);
                continue;
            }
            if !meta.is_file() || is_temp(&name) {
                continue;
            }
            let (from, to) = (src.join(&child), dst.join(&child));
            if existing == Existing::Replace || !same_file(&meta, &to) {
                match copy_file(&from, &to) {
                    Ok(()) => {}
                    Err(e) if is_not_found(&e) => continue,
                    Err(e) => return Err(e.context(format!("copying {}", from.display()))),
                }
                out.copied += 1;
                out.copied_bytes += meta.len();
                out.fresh.insert(child.clone());
                wrote_here = true;
            }
            out.files += 1;
            out.bytes += meta.len();
            out.seen.insert(child);
            progress.set_message(format!(
                "{} files, {} copied ({})",
                out.files,
                out.copied,
                HumanBytes(out.copied_bytes)
            ));
        }
        if wrote_here {
            sync_dir(&dst.join(&rel));
        }
    }
    Ok(out)
}

/// Whether `to` already holds the file described by `meta`.
fn same_file(meta: &std::fs::Metadata, to: &Path) -> bool {
    let Ok(existing) = std::fs::symlink_metadata(to) else {
        return false;
    };
    existing.is_file()
        && existing.len() == meta.len()
        && matches!((existing.modified(), meta.modified()), (Ok(a), Ok(b)) if a == b)
}

/// Copy `from` to `to` through a synced temp file and a rename, keeping the
/// modification time so the next run recognizes it, and the permissions.
///
/// The temp file has a fresh name and is created anew, so a link in its
/// place is never followed (`SEC-54`); the rename replaces a link at `to`
/// rather than writing through it.
fn copy_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    let staged = with_suffix(
        to,
        &format!(".{}{TEMP_SUFFIX}", uuid::Uuid::new_v4().simple()),
    );
    let result = (|| -> std::io::Result<()> {
        let mut source = std::fs::File::open(from)?;
        let meta = source.metadata()?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Nobody else's until it has the source's permissions: it may be a
        // TLS key.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&staged)?;
        std::io::copy(&mut source, &mut file)?;
        file.set_modified(meta.modified()?)?;
        file.set_permissions(meta.permissions())?;
        file.sync_all()?;
        std::fs::rename(&staged, to)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    Ok(result?)
}

/// Remove every file under `root` that is not in `keep`, and the directories
/// that leaves empty. Returns how many files went.
fn prune(root: &Path, keep: &HashSet<PathBuf>) -> anyhow::Result<u64> {
    fn walk(
        root: &Path,
        rel: &Path,
        keep: &HashSet<PathBuf>,
        removed: &mut u64,
    ) -> anyhow::Result<bool> {
        let mut empty = true;
        for entry in std::fs::read_dir(root.join(rel))? {
            let entry = entry?;
            let child = rel.join(entry.file_name());
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if walk(root, &child, keep, removed)? {
                    std::fs::remove_dir(root.join(&child))?;
                } else {
                    empty = false;
                }
            } else if keep.contains(&child) {
                empty = false;
            } else {
                std::fs::remove_file(root.join(&child))?;
                *removed += 1;
            }
        }
        Ok(empty)
    }
    let mut removed = 0;
    if root.is_dir() {
        walk(root, Path::new(""), keep, &mut removed)?;
    }
    Ok(removed)
}

/// Whether `dir` holds any file outside the top-level directories in `skip`.
fn has_content(dir: &Path, skip: &[&str]) -> anyhow::Result<bool> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let name = entry?.file_name();
        if !skip.iter().any(|s| name == *s) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_temp(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.ends_with(".tmp") || name.ends_with(TEMP_SUFFIX)
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Remove the journal files SQLite may keep beside the database at `db`.
fn remove_sidecars(db: &Path) -> std::io::Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        remove_if_present(&with_suffix(db, suffix))?;
    }
    Ok(())
}

fn remove_dir_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// Flush a directory so renames into it survive a power cut. Best effort, as
/// in the package store: not every platform can open a directory to sync it.
fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

fn progress_bar(draw: ProgressDrawTarget) -> ProgressBar {
    let bar = ProgressBar::with_draw_target(None, draw);
    bar.set_style(ProgressStyle::with_template("{spinner} [{elapsed_precise}] {msg}").unwrap());
    bar.enable_steady_tick(Duration::from_millis(200));
    bar
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn copy_skips_scratch_and_temp_files_and_reuses_what_is_there() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        touch(&src.path().join("a/1.0.0/a.1.0.0.nupkg"), b"payload");
        touch(&src.path().join(".uploads/x.part"), b"partial");
        touch(&src.path().join(".migrate/run/y.nupkg"), b"download");
        touch(&src.path().join("a/1.0.0/store-1.tmp"), b"staged");
        let bar = ProgressBar::hidden();

        let first = copy_tree(
            src.path(),
            dst.path(),
            &SCRATCH_DIRS,
            Existing::KeepUnchanged,
            Links::Refuse,
            &bar,
        )
        .unwrap();
        assert_eq!((first.files, first.copied), (1, 1));
        assert_eq!(
            std::fs::read(dst.path().join("a/1.0.0/a.1.0.0.nupkg")).unwrap(),
            b"payload"
        );
        assert!(!dst.path().join(".uploads").exists());
        assert!(!dst.path().join(".migrate").exists());
        assert!(!dst.path().join("a/1.0.0/store-1.tmp").exists());

        let again = copy_tree(
            src.path(),
            dst.path(),
            &SCRATCH_DIRS,
            Existing::KeepUnchanged,
            Links::Refuse,
            &bar,
        )
        .unwrap();
        assert_eq!((again.files, again.copied), (1, 0), "nothing changed");
    }

    #[test]
    fn a_replaced_file_is_copied_again() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let file = src.path().join("a/1.0.0/a.1.0.0.nupkg");
        touch(&file, b"old");
        let bar = ProgressBar::hidden();
        copy_tree(
            src.path(),
            dst.path(),
            &[],
            Existing::KeepUnchanged,
            Links::Refuse,
            &bar,
        )
        .unwrap();

        touch(&file, b"newer content");
        let again = copy_tree(
            src.path(),
            dst.path(),
            &[],
            Existing::KeepUnchanged,
            Links::Refuse,
            &bar,
        )
        .unwrap();
        assert_eq!(again.copied, 1);
        assert_eq!(
            std::fs::read(dst.path().join("a/1.0.0/a.1.0.0.nupkg")).unwrap(),
            b"newer content"
        );
    }

    #[test]
    fn prune_removes_what_is_not_kept_and_the_empty_directories() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("a/1.0.0/a.1.0.0.nupkg"), b"x");
        touch(&dir.path().join("b/2.0.0/b.2.0.0.nupkg"), b"y");
        let keep: HashSet<PathBuf> = [PathBuf::from("a/1.0.0/a.1.0.0.nupkg")].into();
        assert_eq!(prune(dir.path(), &keep).unwrap(), 1);
        assert!(dir.path().join("a/1.0.0/a.1.0.0.nupkg").exists());
        assert!(!dir.path().join("b").exists());
    }

    /// `DEP-17`: SQLite replays a `-wal` it finds beside a database into what
    /// it reads, and the backup directory is not the server's to trust. A
    /// log planted beside the snapshot while the store is copied must not
    /// reach the check (or the backup it turns into).
    #[tokio::test]
    async fn a_log_planted_beside_the_snapshot_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = dir.path().join("snapshot.db");
        let store = dir.path().join("packages");
        std::fs::create_dir_all(&store).unwrap();
        let mut conn = SqliteConnectOptions::new()
            .filename(&snapshot)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Delete)
            .connect()
            .await
            .unwrap();
        for table in [
            "CREATE TABLE packages (lower_id TEXT, normalized_version TEXT, \
             package_size INTEGER, package_hash TEXT, package_hash_algorithm TEXT)",
            "CREATE TABLE package_files (sha256 TEXT, size INTEGER)",
            "CREATE TABLE symbols (ssqp_key TEXT, filename TEXT)",
        ] {
            sqlx::query(table).execute(&mut conn).await.unwrap();
        }
        conn.close().await.unwrap();

        // A log that adds a row, written against a byte-identical copy and
        // taken before that copy checkpoints it.
        let twin = dir.path().join("twin.db");
        std::fs::copy(&snapshot, &twin).unwrap();
        let mut writer = SqliteConnectOptions::new()
            .filename(&twin)
            .journal_mode(SqliteJournalMode::Wal)
            .pragma("wal_autocheckpoint", "0")
            .connect()
            .await
            .unwrap();
        sqlx::query("INSERT INTO packages VALUES ('planted', '1.0.0', 1, 'x', 'SHA512')")
            .execute(&mut writer)
            .await
            .unwrap();
        std::fs::copy(with_suffix(&twin, "-wal"), with_suffix(&snapshot, "-wal")).unwrap();
        writer.close().await.unwrap();
        std::fs::write(with_suffix(&snapshot, "-shm"), b"stale").unwrap();

        let missing = verify(&snapshot, &store, &HashSet::new()).await.unwrap();
        assert!(missing.is_empty(), "the planted log was read: {missing:?}");
        for suffix in ["-wal", "-shm", "-journal"] {
            assert!(!with_suffix(&snapshot, suffix).exists(), "{suffix} is left");
        }
    }

    /// `SEC-54`: the snapshot is taken in a directory nobody else can
    /// enter, so nothing can be laid beside it, and that goes again.
    #[cfg(unix)]
    #[test]
    fn the_snapshot_directory_is_private_and_goes_when_dropped() {
        use std::os::unix::fs::PermissionsExt;
        let dest = tempfile::tempdir().unwrap();
        let private = PrivateDir::create(dest.path()).unwrap();
        let mode = std::fs::metadata(&private.path)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(is_temp(private.path.file_name().unwrap()));
        let path = private.path.clone();
        drop(private);
        assert!(!path.exists());
    }

    /// A copy keeps the source's permissions, and a temp file never has
    /// looser ones on the way: the TLS key is copied this way.
    #[cfg(unix)]
    #[test]
    fn a_copy_keeps_the_permissions_of_its_source() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("key.pem"), dir.path().join("copy.pem"));
        std::fs::write(&from, b"secret").unwrap();
        std::fs::set_permissions(&from, std::fs::Permissions::from_mode(0o600)).unwrap();
        copy_file(&from, &to).unwrap();
        let mode = std::fs::metadata(&to).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(std::fs::read(&to).unwrap(), b"secret");
    }

    #[test]
    fn a_directory_without_a_manifest_is_not_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_manifest(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not a complete backup"), "{err}");
    }
}
