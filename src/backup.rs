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
//! only moves what changed, which takes seconds. A file still missing after
//! that is missing from the live store too, and is reported, not hidden.
//!
//! Re-running into the same directory is incremental, like `rsync`: a stored
//! file is never rewritten in place (the store renames new content over the
//! old), so a file whose size and modification time match is already there.
//! Every file is copied to a temp name, synced and renamed, so the backup never
//! holds a partial file, and files that left the live store are removed only
//! after the new snapshot is in place: an interrupted run leaves the previous
//! snapshot with a superset of its files, and no manifest.

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
    /// Layout version; see [`FORMAT`].
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
    /// Files the snapshot references that the live store did not have.
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
        bail!("no database at {}: nothing to back up", live_db.display());
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

    let progress = progress_bar(draw);
    let packages = dest.join(PACKAGES);
    let snapshot = dest.join(format!("{DATABASE}{TEMP_SUFFIX}"));
    let (mut copied, mut copied_bytes) = (0, 0);
    let mut attempts = 0;
    let (tree, missing, created) = loop {
        attempts += 1;
        progress.set_message(format!("pass {attempts}: snapshotting the database"));
        let created = chrono::Utc::now();
        snapshot_database(&live_db, &snapshot).await?;

        progress.set_message(format!("pass {attempts}: copying the package store"));
        let tree = {
            let (src, dst, bar) = (config.storage_path(), packages.clone(), progress.clone());
            tokio::task::spawn_blocking(move || {
                copy_tree(&src, &dst, &SCRATCH_DIRS, Existing::KeepUnchanged, &bar)
            })
            .await??
        };
        copied += tree.copied;
        copied_bytes += tree.copied_bytes;

        let missing = verify(&snapshot, &packages).await?;
        if missing.is_empty() || attempts == ATTEMPTS {
            break (tree, missing, created);
        }
        tracing::info!(
            missing = missing.len(),
            "files changed while copying; taking a fresh snapshot"
        );
    };

    let stats = finish_snapshot(&snapshot).await?;
    std::fs::rename(&snapshot, dest.join(DATABASE))?;
    sync_dir(dest);

    progress.set_message("removing files the live store no longer has");
    let removed = {
        let (dst, seen) = (packages.clone(), tree.seen);
        tokio::task::spawn_blocking(move || prune(&dst, &seen)).await??
    };

    let live_tls = config.data_dir.join(TLS);
    let tls = live_tls.is_dir();
    if tls {
        let (src, dst, bar) = (live_tls, dest.join(TLS), progress.clone());
        let t = tokio::task::spawn_blocking(move || {
            copy_tree(&src, &dst, &[], Existing::KeepUnchanged, &bar)
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
        files: tree.files,
        bytes: tree.bytes,
        missing,
        tls,
    };
    write_manifest(dest, &manifest)?;
    Ok(BackupSummary {
        manifest,
        copied,
        copied_bytes,
        removed,
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
        tokio::task::spawn_blocking(move || copy_tree(&from, &to, &[], Existing::Replace, &bar))
            .await??
    };
    let removed = {
        let (to, seen) = (storage.clone(), tree.seen);
        tokio::task::spawn_blocking(move || prune(&to, &seen)).await??
    };
    if manifest.tls {
        let (from, to, bar) = (src.join(TLS), config.data_dir.join(TLS), progress.clone());
        tokio::task::spawn_blocking(move || copy_tree(&from, &to, &[], Existing::Replace, &bar))
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
    for suffix in ["-wal", "-shm", "-journal"] {
        remove_if_present(&with_suffix(&live_db, suffix))?;
    }
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

fn write_manifest(dest: &Path, manifest: &Manifest) -> anyhow::Result<()> {
    let staged = dest.join(format!("{MANIFEST}{TEMP_SUFFIX}"));
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&staged)?;
        file.write_all(serde_json::to_string_pretty(manifest)?.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&staged, dest.join(MANIFEST))?;
    sync_dir(dest);
    Ok(())
}

/// Hold an exclusive lock on the backup directory, so two runs into the same
/// directory (a cron job overlapping a slow one) cannot interleave.
fn lock(dest: &Path) -> anyhow::Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dest.join(".backup.lock"))?;
    if fs4::FileExt::try_lock(&file).is_err() {
        bail!("another `yanuget backup` is writing to {}", dest.display());
    }
    Ok(file)
}

// --- the database ---

/// Write a consistent snapshot of the live database to `to`.
///
/// `VACUUM INTO` reads inside one transaction, so it sees one committed state
/// however many writes the server makes meanwhile, and does not block them.
async fn snapshot_database(live: &Path, to: &Path) -> anyhow::Result<()> {
    remove_if_present(to)?;
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

async fn open_snapshot(path: &Path) -> anyhow::Result<SqliteConnection> {
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

/// Every file the snapshot at `db` references that `store` lacks, or holds
/// with a size other than the one recorded, as a path relative to `store`.
pub async fn verify(db: &Path, store: &Path) -> anyhow::Result<Vec<String>> {
    let mut conn = open_snapshot(db).await?;
    let mut expected: Vec<(String, Option<u64>)> = Vec::new();
    for row in sqlx::query("SELECT lower_id, normalized_version, package_size FROM packages")
        .fetch_all(&mut conn)
        .await?
    {
        let id = row.get::<String, _>(0).to_ascii_lowercase();
        let version = row.get::<String, _>(1).to_ascii_lowercase();
        let size: i64 = row.get(2);
        expected.push((
            format!("{id}/{version}/{id}.{version}.nupkg"),
            Some(size as u64),
        ));
    }
    for row in sqlx::query("SELECT DISTINCT sha256, size FROM package_files")
        .fetch_all(&mut conn)
        .await?
    {
        let sha: String = row.get(0);
        let size: i64 = row.get(1);
        if sha.len() > 2 {
            expected.push((
                format!(".blobs/sha256/{}/{sha}", &sha[..2]),
                Some(size as u64),
            ));
        }
    }
    for row in sqlx::query("SELECT ssqp_key, filename FROM symbols")
        .fetch_all(&mut conn)
        .await?
    {
        let key = row.get::<String, _>(0).to_ascii_lowercase();
        let file = row.get::<String, _>(1).to_ascii_lowercase();
        expected.push((format!(".symbols/{key}/{file}"), None));
    }
    conn.close().await?;

    let store = store.to_path_buf();
    Ok(tokio::task::spawn_blocking(move || {
        expected
            .into_iter()
            .filter(|(rel, size)| match std::fs::metadata(store.join(rel)) {
                Ok(m) => !m.is_file() || size.is_some_and(|s| s != m.len()),
                Err(_) => true,
            })
            .map(|(rel, _)| rel)
            .collect()
    })
    .await?)
}

// --- the files ---

/// What [`copy_tree`] found and did.
struct TreeCopy {
    /// Every file copied or already in place, relative to the root.
    seen: HashSet<PathBuf>,
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

/// Make every regular file under `src` exist under `dst` with the same
/// content, skipping the top-level directories in `skip` and any temp file.
/// What happens to a file already in `dst` is up to `existing`. Removes
/// nothing; see [`prune`].
fn copy_tree(
    src: &Path,
    dst: &Path,
    skip: &[&str],
    existing: Existing,
    progress: &ProgressBar,
) -> anyhow::Result<TreeCopy> {
    let mut out = TreeCopy {
        seen: HashSet::new(),
        files: 0,
        bytes: 0,
        copied: 0,
        copied_bytes: 0,
    };
    if !src.is_dir() {
        return Ok(out);
    }
    std::fs::create_dir_all(dst)?;
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
                std::fs::create_dir_all(dst.join(&child))?;
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
/// modification time so the next run recognizes it.
fn copy_file(from: &Path, to: &Path) -> anyhow::Result<()> {
    let staged = with_suffix(
        to,
        &format!(".{}{TEMP_SUFFIX}", uuid::Uuid::new_v4().simple()),
    );
    let result = (|| -> std::io::Result<()> {
        std::fs::copy(from, &staged)?;
        let file = std::fs::OpenOptions::new().write(true).open(&staged)?;
        file.set_modified(std::fs::metadata(from)?.modified()?)?;
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
        copy_tree(src.path(), dst.path(), &[], Existing::KeepUnchanged, &bar).unwrap();

        touch(&file, b"newer content");
        let again = copy_tree(src.path(), dst.path(), &[], Existing::KeepUnchanged, &bar).unwrap();
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

    #[test]
    fn a_directory_without_a_manifest_is_not_a_backup() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_manifest(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not a complete backup"), "{err}");
    }
}
