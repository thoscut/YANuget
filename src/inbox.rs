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
//! Everything under the inbox belongs to the uploader, who can put a link, a
//! pipe or a hard link wherever the scanner is about to look, and swap one in
//! between two looks. So nothing here is reached by a path. Each directory is
//! opened relative to its parent's handle without following links, and every
//! file relative to its directory's; on Unix, a link the uploader planted is
//! never followed, whether it stands for a directory, the file, the checksum
//! file or the `.error` report. The file is opened once, checked through that
//! handle (a regular file with no other links), and copied through it into the
//! server's own staging area while it is hashed. What is verified is that
//! copy, and that copy is what is stored: the uploader's inode never enters
//! the store, so writing to it later through a handle still open changes
//! nothing.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::FilesConfig;
use crate::database::PackageDatabase;
use crate::error::{Error, Result};
use crate::storage::{PackageStorage, TempPath};
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
    /// Free space an import must leave on the staging volume
    /// (`min_free_disk_bytes`); 0 checks nothing.
    pub min_free_disk_bytes: u64,
}

/// Refuse an inbox inside the package store: the importer removes files from
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

impl Inbox<'_> {
    /// Import every file whose checksum file has arrived.
    pub async fn scan(&self) -> ScanReport {
        let mut report = ScanReport::default();
        let root = match dir::InboxDir::open_root(self.dir) {
            Ok(root) => root,
            Err(e) => {
                tracing::warn!(inbox = %self.dir.display(), error = %e, "cannot open the file inbox");
                return report;
            }
        };
        // Directories are held open only along the current path, so however
        // many the uploader creates, a scan holds four handles at most.
        for feed in root.names() {
            if !self.feeds.contains(&feed) {
                continue;
            }
            let Ok(feed_dir) = root.open_dir(&feed) else {
                continue;
            };
            for id in feed_dir.names() {
                if crate::validation::validate_package_id(&id).is_err() {
                    continue;
                }
                let Ok(id_dir) = feed_dir.open_dir(&id) else {
                    continue;
                };
                for version in id_dir.names() {
                    let Ok(v) = NuGetVersion::parse(&version) else {
                        continue;
                    };
                    let Ok(version_dir) = id_dir.open_dir(&version) else {
                        continue;
                    };
                    for entry in version_dir.names() {
                        let Some(name) = entry.strip_suffix(".sha256") else {
                            continue;
                        };
                        let error = format!("{name}.error");
                        // Waiting for the file, or for someone to read why it
                        // failed. A report is a regular file; a directory of
                        // that name is left alone too. Anything else there (a
                        // link, most likely) is not a report, and is replaced
                        // by one if this import fails.
                        if version_dir.kind(name) != dir::Kind::File
                            || matches!(version_dir.kind(&error), dir::Kind::File | dir::Kind::Dir)
                        {
                            continue;
                        }
                        match self.import(&feed, &id, &v, name, &version_dir).await {
                            Ok(()) => report.imported += 1,
                            Err(e) => {
                                report.failed += 1;
                                tracing::warn!(%feed, %id, version = %v.normalized(), %name, error = %e, "inbox import failed");
                                write_report(&version_dir, &error, &e);
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
        dir: &dir::InboxDir,
    ) -> Result<()> {
        crate::validation::validate_file_name(name, self.files)?;
        let marker = format!("{name}.sha256");
        let expected = read_marker(open_regular(dir, &marker)?)?;
        if !self.db.exists(feed, id, v).await? {
            return Err(Error::PackageNotFound);
        }

        // One handle, opened without following links and checked through
        // itself, is all that is read from here on.
        let source = open_regular(dir, name)?;
        let meta = source.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // Another name for the same inode is another way to change it,
            // and a link to a file the uploader could not otherwise read.
            if meta.nlink() != 1 {
                return Err(Error::BadRequest(format!(
                    "{name} has other hard links; upload it as a file of its own"
                )));
            }
        }
        let size = meta.len();
        if let Some(limit) = self.max_file_size {
            if size > limit {
                return Err(Error::PayloadTooLarge(format!(
                    "{size} bytes is over the {limit}-byte limit for files"
                )));
            }
        }

        // Out of the uploader's reach first, then verified: a copy only the
        // server can write, removed on any way out of here but the store.
        tokio::fs::create_dir_all(self.staging).await?;
        // The copy needs the file's size again on the store's volume, and the
        // uploader's quota is not the server's: held to the same reserve as a
        // push, so an inbox full of images cannot fill the disk the database
        // lives on.
        ensure_disk_space(self.staging, size, self.min_free_disk_bytes)?;
        let staged = TempPath::new(
            self.staging
                .join(format!("inbox-{}.tmp", uuid::Uuid::new_v4().simple())),
        );
        let (copied, actual) = copy_hashed(source, staged.path(), size).await?;
        if actual != expected {
            // Not the hash itself: whatever the file is, the report says only
            // that it is not what the checksum file promised.
            return Err(Error::BadRequest(format!(
                "checksum mismatch: {name} does not have the SHA-256 in {marker}"
            )));
        }
        let target = Target {
            storage: self.storage,
            db: self.db,
            feed,
        };
        // Already attached with exactly these bytes counts as done too.
        let (Attached::New(_) | Attached::Same(_)) = attach_to(
            &target,
            id,
            v,
            name,
            staged.path().to_path_buf(),
            &actual,
            copied,
        )
        .await?;
        let _ = dir.remove(name);
        let _ = dir.remove(&marker);
        tracing::info!(%feed, %id, version = %v.normalized(), %name, size = copied, "imported file from the inbox");
        Ok(())
    }
}

/// Open `name` in `dir` for reading, refusing anything but a regular file.
fn open_regular(dir: &dir::InboxDir, name: &str) -> Result<std::fs::File> {
    let not_a_file = || Error::BadRequest(format!("{name} is not a regular file"));
    let file = dir.open_file(name).map_err(|e| match dir.kind(name) {
        dir::Kind::Missing => Error::BadRequest(format!("{name} is gone")),
        dir::Kind::File => Error::Io(e),
        dir::Kind::Dir | dir::Kind::Other => not_a_file(),
    })?;
    if file.metadata()?.file_type().is_file() {
        Ok(file)
    } else {
        Err(not_a_file())
    }
}

/// Leave `{name}.error` next to a file that could not be imported.
///
/// Created fresh, never written through: whatever stands at that name and is
/// not a report is unlinked first (unlinking a link leaves its target alone),
/// and the report is created exclusively and without following links, so if
/// the uploader puts a link back in between, creating fails and nothing is
/// written anywhere.
fn write_report(dir: &dir::InboxDir, report: &str, e: &Error) {
    if dir.kind(report) == dir::Kind::Other {
        let _ = dir.remove(report);
    }
    // The uploader reads this: what they did wrong, verbatim, but never a
    // server fault's text, which can carry paths and SQL.
    let why = if e.status().is_server_error() {
        "the server could not import it; its log says why".to_string()
    } else {
        e.to_string()
    };
    let text = format!(
        "{}: not imported: {why}\nRemove this file to try again.\n",
        chrono::Utc::now().to_rfc3339()
    );
    let written = dir
        .create_new(report)
        .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()));
    if let Err(e) = written {
        tracing::warn!(report, error = %e, "could not write an inbox error report");
    }
}

/// Refuse a copy of `incoming` bytes that would leave `dir`'s volume with
/// less than `reserve` free. Unmeasurable free space lets the import through,
/// as it does a push: a guard that fails closed would stop every import.
fn ensure_disk_space(dir: &Path, incoming: u64, reserve: u64) -> Result<()> {
    if reserve == 0 {
        return Ok(());
    }
    let available = match fs4::available_space(dir) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "could not measure free disk space");
            return Ok(());
        }
    };
    let needed = incoming.saturating_add(reserve);
    if available < needed {
        return Err(Error::InsufficientStorage(format!(
            "{available} bytes free on the storage volume, {needed} needed \
             (the file plus the configured reserve)"
        )));
    }
    Ok(())
}

/// The SHA-256 a checksum file names: its first word, 64 hex digits, as
/// `sha256sum` writes it (`<hex>  <name>`).
fn read_marker(file: std::fs::File) -> Result<String> {
    let mut raw = String::new();
    file.take(4096)
        .read_to_string(&mut raw)
        .map_err(|_| Error::BadRequest("the checksum file is not text".into()))?;
    parse_marker(&raw)
}

fn parse_marker(raw: &str) -> Result<String> {
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

/// Copy the first `len` bytes of `from` to a new file at `to`, hashing them on
/// the way, and sync it. `len` is the size the file had when it was opened: an
/// uploader still appending cannot keep the scanner copying forever. Returns
/// how many bytes were copied and their SHA-256.
async fn copy_hashed(from: std::fs::File, to: &Path, len: u64) -> Result<(u64, String)> {
    let mut src = tokio::fs::File::from_std(from).take(len);
    let mut dst = tokio::fs::File::create_new(to).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut copied = 0u64;
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dst.write_all(&buf[..n]).await?;
        copied += n as u64;
    }
    dst.sync_all().await?;
    Ok((copied, hex::encode(hasher.finalize())))
}

/// Directories of the inbox, reached without following links.
mod dir {
    use std::io;
    use std::path::Path;
    #[cfg(not(unix))]
    use std::path::PathBuf;

    /// What stands at a name, looked at without following a link.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Kind {
        File,
        Dir,
        /// A link, a pipe, a device or a socket.
        Other,
        Missing,
    }

    /// An open directory. Every name below is one path component, resolved
    /// relative to this handle.
    pub(super) struct InboxDir {
        #[cfg(unix)]
        fd: std::os::fd::OwnedFd,
        #[cfg(not(unix))]
        path: PathBuf,
    }

    #[cfg(unix)]
    impl InboxDir {
        /// The inbox itself: the operator's path, taken as configured.
        pub(super) fn open_root(path: &Path) -> io::Result<Self> {
            use rustix::fs::{openat, Mode, OFlags, CWD};
            let fd = openat(
                CWD,
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            Ok(Self { fd })
        }

        /// A subdirectory; a link to one is refused.
        pub(super) fn open_dir(&self, name: &str) -> io::Result<Self> {
            use rustix::fs::{openat, Mode, OFlags};
            let fd = openat(
                &self.fd,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            Ok(Self { fd })
        }

        /// The names in this directory, sorted. Unreadable ones yield none.
        pub(super) fn names(&self) -> Vec<String> {
            let Ok(entries) = rustix::fs::Dir::read_from(&self.fd) else {
                return Vec::new();
            };
            let mut out: Vec<String> = entries
                .map_while(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().ok().map(str::to_string))
                .filter(|n| n != "." && n != "..")
                .collect();
            out.sort();
            out
        }

        pub(super) fn kind(&self, name: &str) -> Kind {
            use rustix::fs::{statat, AtFlags, FileType};
            match statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(st) => match FileType::from_raw_mode(st.st_mode as _) {
                    FileType::RegularFile => Kind::File,
                    FileType::Directory => Kind::Dir,
                    _ => Kind::Other,
                },
                Err(_) => Kind::Missing,
            }
        }

        /// Open a file for reading. A link is refused, and opening a pipe
        /// does not wait for a writer.
        pub(super) fn open_file(&self, name: &str) -> io::Result<std::fs::File> {
            use rustix::fs::{openat, Mode, OFlags};
            let fd = openat(
                &self.fd,
                name,
                OFlags::RDONLY
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::NOCTTY
                    | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            Ok(fd.into())
        }

        /// Create a new file; fails if anything, a link included, is there.
        pub(super) fn create_new(&self, name: &str) -> io::Result<std::fs::File> {
            use rustix::fs::{openat, Mode, OFlags};
            let fd = openat(
                &self.fd,
                name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o644),
            )?;
            Ok(fd.into())
        }

        /// Remove a name. A link is removed, not what it points at.
        pub(super) fn remove(&self, name: &str) -> io::Result<()> {
            rustix::fs::unlinkat(&self.fd, name, rustix::fs::AtFlags::empty())?;
            Ok(())
        }
    }

    /// Elsewhere, by path, with each step checked for a link first. The
    /// inbox's threat model — an upload-only account in an `sshd` chroot,
    /// planting links in the server's namespace — is a Unix one.
    #[cfg(not(unix))]
    impl InboxDir {
        pub(super) fn open_root(path: &Path) -> io::Result<Self> {
            if std::fs::metadata(path)?.is_dir() {
                Ok(Self {
                    path: path.to_path_buf(),
                })
            } else {
                Err(io::Error::other("not a directory"))
            }
        }

        pub(super) fn open_dir(&self, name: &str) -> io::Result<Self> {
            if self.kind(name) == Kind::Dir {
                Ok(Self {
                    path: self.path.join(name),
                })
            } else {
                Err(io::Error::other("not a directory"))
            }
        }

        pub(super) fn names(&self) -> Vec<String> {
            let Ok(rd) = std::fs::read_dir(&self.path) else {
                return Vec::new();
            };
            let mut out: Vec<String> = rd
                .map_while(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect();
            out.sort();
            out
        }

        pub(super) fn kind(&self, name: &str) -> Kind {
            match std::fs::symlink_metadata(self.path.join(name)) {
                Ok(m) if m.file_type().is_file() => Kind::File,
                Ok(m) if m.file_type().is_dir() => Kind::Dir,
                Ok(_) => Kind::Other,
                Err(_) => Kind::Missing,
            }
        }

        pub(super) fn open_file(&self, name: &str) -> io::Result<std::fs::File> {
            match self.kind(name) {
                Kind::File => std::fs::File::open(self.path.join(name)),
                Kind::Missing => Err(io::ErrorKind::NotFound.into()),
                _ => Err(io::Error::other("not a regular file")),
            }
        }

        pub(super) fn create_new(&self, name: &str) -> io::Result<std::fs::File> {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.path.join(name))
        }

        pub(super) fn remove(&self, name: &str) -> io::Result<()> {
            std::fs::remove_file(self.path.join(name))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checksum_file_is_sha256sum_output() {
        let hex = "ab".repeat(32);
        let line = format!("{}  base.wim\n", hex.to_uppercase());
        assert_eq!(parse_marker(&line).unwrap(), hex);
        assert!(parse_marker("not a hash").is_err());
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

    #[cfg(unix)]
    #[test]
    fn links_are_never_followed() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let inbox = tmp.path().join("inbox");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(inbox.join("real")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"server's own").unwrap();
        symlink(&outside, inbox.join("linked-dir")).unwrap();
        symlink(outside.join("secret"), inbox.join("linked-file")).unwrap();
        symlink(outside.join("new"), inbox.join("dangling")).unwrap();

        let root = dir::InboxDir::open_root(&inbox).unwrap();
        assert!(root.open_dir("real").is_ok());
        assert!(root.open_dir("linked-dir").is_err());
        assert!(root.open_file("linked-file").is_err());
        assert_eq!(root.kind("linked-file"), dir::Kind::Other);
        assert_eq!(root.kind("dangling"), dir::Kind::Other);
        assert_eq!(root.kind("nothing"), dir::Kind::Missing);
        // Creating over a dangling link does not create its target.
        assert!(root.create_new("dangling").is_err());
        assert!(!outside.join("new").exists());
        // Removing a link leaves its target alone.
        root.remove("linked-file").unwrap();
        assert!(outside.join("secret").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_is_not_waited_on() {
        let tmp = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(tmp.path().join("pipe"))
            .status();
        if !status.is_ok_and(|s| s.success()) {
            return;
        }
        let root = dir::InboxDir::open_root(tmp.path()).unwrap();
        assert_eq!(root.kind("pipe"), dir::Kind::Other);
        // Opening does not block for a writer, and it is refused as a file.
        assert!(open_regular(&root, "pipe").is_err());
    }
}
