//! The inbox: files dropped over SSH (`scp`, `sftp`, `rsync`) and attached to
//! package versions.
//!
//! YANuget runs no SSH server of its own. The host's `sshd` does what it is
//! good at — keys, accounts, a chroot for an upload-only user — and writes into
//! a directory this module scans:
//!
//! ```text
//! {inbox_dir}/{feed}/{id}/{version}/{name}          the file
//! {inbox_dir}/{feed}/{id}/{version}/{name}.sha256   its checksum, sha256sum format
//! ```
//!
//! The checksum file is both the integrity check and the "done" signal: a file
//! still being copied has none yet, so nothing half-transferred is imported,
//! and `rsync --partial --append-verify` resumes a broken transfer for free.
//! A file that cannot be imported gets a `{name}.error` explaining why, visible
//! to whoever uploaded it; remove it to try again.
//!
//! Before the checksum is computed, the file is moved out of the inbox into the
//! server's own staging area, so what is verified is what is stored: the
//! uploader can no longer change it in between.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::FilesConfig;
use crate::database::PackageDatabase;
use crate::error::{Error, Result};
use crate::storage::PackageStorage;
use crate::version::NuGetVersion;
use crate::web::hosted::{attach_to, Attached, Target};

/// What one scan did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ScanReport {
    pub imported: usize,
    pub failed: usize,
}

/// Everything a scan needs.
pub struct Inbox<'a> {
    pub dir: &'a Path,
    pub storage: &'a dyn PackageStorage,
    pub db: &'a dyn PackageDatabase,
    pub files: &'a FilesConfig,
    pub max_file_size: Option<u64>,
    /// The configured feeds; a directory named otherwise is ignored.
    pub feeds: &'a [String],
    /// The server's private staging directory (the store's `.uploads`).
    pub staging: &'a Path,
}

/// Refuse an inbox inside the package store: the importer moves files out of
/// the inbox, and the store's own files must never be among them.
pub fn check_location(inbox: &Path, storage: &Path) -> Result<()> {
    let inbox = std::fs::canonicalize(inbox)?;
    let storage = std::fs::canonicalize(storage)?;
    if inbox.starts_with(&storage) || storage.starts_with(&inbox) {
        return Err(Error::BadRequest(format!(
            "the inbox {} and the package store {} must not contain each other",
            inbox.display(),
            storage.display()
        )));
    }
    Ok(())
}

/// Whether `path` is a real directory (not a link to one).
async fn is_dir(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|m| m.file_type().is_dir())
}

/// Whether `path` is a regular file (not a link, device or directory).
async fn is_file(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|m| m.file_type().is_file())
}

/// The entries of a directory, by name. Unreadable directories yield none.
async fn entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return out;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        if let Some(name) = entry.file_name().to_str() {
            out.push((name.to_string(), entry.path()));
        }
    }
    out.sort();
    out
}

impl Inbox<'_> {
    /// Import every file whose checksum file has arrived.
    pub async fn scan(&self) -> ScanReport {
        let mut report = ScanReport::default();
        for (feed, feed_dir) in entries(self.dir).await {
            if !self.feeds.contains(&feed) || !is_dir(&feed_dir).await {
                continue;
            }
            for (id, id_dir) in entries(&feed_dir).await {
                if crate::validation::validate_package_id(&id).is_err() || !is_dir(&id_dir).await {
                    continue;
                }
                for (version, version_dir) in entries(&id_dir).await {
                    let Ok(v) = NuGetVersion::parse(&version) else {
                        continue;
                    };
                    if !is_dir(&version_dir).await {
                        continue;
                    }
                    for (entry, marker) in entries(&version_dir).await {
                        let Some(name) = entry.strip_suffix(".sha256") else {
                            continue;
                        };
                        let file = version_dir.join(name);
                        let error = version_dir.join(format!("{name}.error"));
                        // Waiting for the file, or for someone to read why it
                        // failed.
                        if !is_file(&file).await
                            || tokio::fs::try_exists(&error).await.unwrap_or(true)
                        {
                            continue;
                        }
                        match self.import(&feed, &id, &v, name, &file, &marker).await {
                            Ok(()) => report.imported += 1,
                            Err(e) => {
                                report.failed += 1;
                                tracing::warn!(%feed, %id, version = %v.normalized(), %name, error = %e, "inbox import failed");
                                let _ = tokio::fs::write(
                                    &error,
                                    format!(
                                        "{}: not imported: {e}\nRemove this file to try again.\n",
                                        chrono::Utc::now().to_rfc3339()
                                    ),
                                )
                                .await;
                            }
                        }
                    }
                }
            }
        }
        report
    }

    async fn import(
        &self,
        feed: &str,
        id: &str,
        v: &NuGetVersion,
        name: &str,
        file: &Path,
        marker: &Path,
    ) -> Result<()> {
        crate::validation::validate_file_name(name, self.files)?;
        if !is_file(marker).await {
            return Err(Error::BadRequest(
                "the checksum file is not a regular file".into(),
            ));
        }
        let expected = read_marker(marker).await?;
        if !self.db.exists(feed, id, v).await? {
            return Err(Error::PackageNotFound);
        }
        let size = tokio::fs::symlink_metadata(file).await?.len();
        if let Some(limit) = self.max_file_size {
            if size > limit {
                return Err(Error::PayloadTooLarge(format!(
                    "{size} bytes is over the {limit}-byte limit for files"
                )));
            }
        }

        // Out of the uploader's reach first, then verified.
        let staged = self
            .staging
            .join(format!("inbox-{}.part", uuid::Uuid::new_v4().simple()));
        let moved = tokio::fs::rename(file, &staged).await.is_ok();
        let actual = if moved {
            hash_file(&staged).await
        } else {
            // Another filesystem: copy and hash in one pass, and leave the
            // original until the copy has proved good.
            copy_hashed(file, &staged).await
        };
        let actual = match actual {
            Ok(h) => h,
            Err(e) => {
                self.put_back(moved, &staged, file).await;
                return Err(e);
            }
        };
        if actual != expected {
            self.put_back(moved, &staged, file).await;
            return Err(Error::BadRequest(format!(
                "checksum mismatch: the file's SHA-256 is {actual}, the checksum file says {expected}"
            )));
        }
        let target = Target {
            storage: self.storage,
            db: self.db,
            feed,
        };
        match attach_to(&target, id, v, name, staged.clone(), &actual, size).await {
            Ok(Attached::New(_)) | Ok(Attached::Same(_)) => {}
            Err(e) => {
                // `attach_to` removed the staged copy; the original is gone
                // too when it was moved, so say where the bytes went.
                if moved {
                    return Err(Error::BadRequest(format!(
                        "{e}; the file was removed from the inbox"
                    )));
                }
                return Err(e);
            }
        }
        if !moved {
            let _ = tokio::fs::remove_file(file).await;
        }
        let _ = tokio::fs::remove_file(marker).await;
        tracing::info!(%feed, %id, version = %v.normalized(), %name, size, "imported file from the inbox");
        Ok(())
    }

    /// Undo the staging move after a failed check, so the uploader can see
    /// (and fix) the file where they left it.
    async fn put_back(&self, moved: bool, staged: &Path, original: &Path) {
        if moved {
            if tokio::fs::rename(staged, original).await.is_err() {
                let _ = tokio::fs::remove_file(staged).await;
            }
        } else {
            let _ = tokio::fs::remove_file(staged).await;
        }
    }
}

/// The SHA-256 a checksum file names: its first word, 64 hex digits, as
/// `sha256sum` writes it (`<hex>  <name>`).
async fn read_marker(marker: &Path) -> Result<String> {
    let mut raw = String::new();
    tokio::fs::File::open(marker)
        .await?
        .take(4096)
        .read_to_string(&mut raw)
        .await
        .map_err(|_| Error::BadRequest("the checksum file is not text".into()))?;
    let hex = raw
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_start_matches('\u{feff}')
        .to_ascii_lowercase();
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hex)
    } else {
        Err(Error::BadRequest(
            "the checksum file must start with the file's SHA-256 in hex".into(),
        ))
    }
}

async fn hash_file(path: &Path) -> Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

async fn copy_hashed(from: &Path, to: &Path) -> Result<String> {
    let mut src = tokio::fs::File::open(from).await?;
    let mut dst = tokio::fs::File::create(to).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n]).await?;
    }
    dst.sync_all().await?;
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_checksum_file_is_sha256sum_output() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("base.wim.sha256");
        let hex = "ab".repeat(32);
        tokio::fs::write(&marker, format!("{}  base.wim\n", hex.to_uppercase()))
            .await
            .unwrap();
        assert_eq!(read_marker(&marker).await.unwrap(), hex);
        tokio::fs::write(&marker, "not a hash").await.unwrap();
        assert!(read_marker(&marker).await.is_err());
    }

    #[test]
    fn the_inbox_cannot_overlap_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("packages");
        let inside = store.join("inbox");
        std::fs::create_dir_all(&inside).unwrap();
        let apart = dir.path().join("inbox");
        std::fs::create_dir_all(&apart).unwrap();
        assert!(check_location(&inside, &store).is_err());
        assert!(check_location(&store, &store).is_err());
        assert!(check_location(&apart, &store).is_ok());
    }
}
