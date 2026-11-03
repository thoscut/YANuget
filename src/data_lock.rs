//! An advisory lock on the data a server serves, across processes.
//!
//! The locks in [`crate::locks`] only order what one process does. A server
//! holds this one for as long as it runs, and the offline commands that write
//! its database and store behind its back (`restore`, `feeds rename`,
//! `feeds remove`) take it while they work, so either side refuses while the
//! other is at it. Unlike the loopback probe those commands also make, it
//! sees a server that does not answer there: one in another container on the
//! same volume, in another network namespace, or bound to an address the
//! probe does not reach.
//!
//! The lock file sits next to the database (`{database_path}.lock`, so
//! `yanuget.db.lock` in `data_dir` by default): the database is what every
//! one of them shares, wherever `storage_path` points. It is taken with
//! `flock` on Unix and `LockFileEx` on Windows, released by the system when
//! the process ends however it ends, so a crash leaves nothing to clean up.
//! A file system that does not support locks (some network mounts) leaves
//! only the probe, with a warning.

use std::fs::File;
use std::path::{Path, PathBuf};

/// A held [`DataLock`]: released when dropped.
#[derive(Debug)]
pub struct DataLock {
    _file: File,
    path: PathBuf,
}

/// Another process holds the lock.
#[derive(Debug)]
pub struct Busy {
    /// The lock file.
    pub path: PathBuf,
}

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} is locked: a server, or an offline command such as `restore` or \
             `feeds rename`, is using this data",
            self.path.display()
        )
    }
}

impl std::error::Error for Busy {}

impl DataLock {
    /// The lock file of the database at `database_path`.
    pub fn path_for(database_path: &str) -> PathBuf {
        PathBuf::from(format!("{database_path}.lock"))
    }

    /// Take the lock of the database at `database_path` without waiting.
    ///
    /// `Ok(None)` when it cannot be taken for any other reason than another
    /// holder: the directory does not exist yet (so nothing serves from it),
    /// or the file system does not lock. That is logged, and the caller goes
    /// on without it.
    pub fn try_acquire(database_path: &str) -> Result<Option<Self>, Busy> {
        let path = Self::path_for(database_path);
        let dir_exists = match path.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.is_dir(),
            _ => true,
        };
        if !dir_exists {
            return Ok(None);
        }
        let file = match open(&path) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(lock = %path.display(), error = %e, "cannot open the data lock file");
                return Ok(None);
            }
        };
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => Ok(Some(Self { _file: file, path })),
            Err(fs4::TryLockError::WouldBlock) => Err(Busy { path }),
            Err(fs4::TryLockError::Error(e)) => {
                tracing::warn!(lock = %path.display(), error = %e,
                    "cannot lock the data lock file; only the loopback probe guards offline commands");
                Ok(None)
            }
        }
    }

    /// Where this lock is held.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn open(path: &Path) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_is_refused_until_the_first_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("yanuget.db");
        let db = db.to_str().unwrap();
        let first = DataLock::try_acquire(db).unwrap().expect("locks here");
        assert_eq!(first.path(), dir.path().join("yanuget.db.lock"));
        let busy = DataLock::try_acquire(db).unwrap_err();
        assert!(busy.to_string().contains("yanuget.db.lock"), "{busy}");
        drop(first);
        assert!(DataLock::try_acquire(db).unwrap().is_some());
    }

    #[test]
    fn a_missing_directory_is_not_locked_or_created() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("not-yet").join("yanuget.db");
        assert!(DataLock::try_acquire(db.to_str().unwrap())
            .unwrap()
            .is_none());
        assert!(!dir.path().join("not-yet").exists());
    }
}
