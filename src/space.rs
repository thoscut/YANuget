//! Free space on the storage volume, and the space promised to what is still
//! being written to it.
//!
//! A full disk does not fail cleanly: the database, the temp file and every
//! other writer on the volume run out together, mid-write. So everything that
//! writes a payload into the staging directory — a push, an attached file, a
//! resumable upload, a mirror download, an inbox import — is admitted by one
//! [`SpaceGuard`] first, which says no up front when the volume would be left
//! with less than `min_free_disk_bytes`, and then watches the bytes going in
//! through [`DiskReserve`].
//!
//! One guard is built per staging directory and shared by every feed, the
//! mirror clients and the inbox, so all of them see what the others promised.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;

use crate::config::Config;
use crate::database::PackageDatabase;
use crate::error::{Error, Result};

/// How free space is measured: `fs4::available_space` (one `statvfs`) unless
/// a test put something else in.
pub type Measure = Arc<dyn Fn(&Path) -> io::Result<u64> + Send + Sync>;

/// How many bytes [`DiskReserve`] lets through between two measurements.
pub const RESERVE_CHECK_INTERVAL: u64 = 64 * 1024 * 1024;

/// The admission of writes into one staging directory: its volume's free
/// space, the reserve to keep, and the bytes promised to writes in progress.
pub struct SpaceGuard {
    dir: PathBuf,
    reserve: u64,
    pool: Arc<Reservations>,
    /// Where unfinished resumable uploads are recorded, whose rest counts as
    /// promised too.
    resumable: Option<Arc<dyn PackageDatabase>>,
    measure: Measure,
}

impl std::fmt::Debug for SpaceGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpaceGuard")
            .field("dir", &self.dir)
            .field("reserve", &self.reserve)
            .field("promised", &self.pool.promised())
            .finish_non_exhaustive()
    }
}

impl SpaceGuard {
    /// A guard for writes into `dir`, keeping `reserve` bytes free on its
    /// volume; a reserve of 0 checks nothing.
    pub fn new(dir: impl Into<PathBuf>, reserve: u64) -> Self {
        Self {
            dir: dir.into(),
            reserve,
            pool: Arc::default(),
            resumable: None,
            measure: Arc::new(|dir: &Path| fs4::available_space(dir)),
        }
    }

    /// The server's guard: the package store's staging directory
    /// (`<storage>/.uploads`), held to `min_free_disk_bytes`, counting the
    /// resumable uploads `db` records.
    pub fn for_store(config: &Config, db: Arc<dyn PackageDatabase>) -> Self {
        Self::new(
            config.storage_path().join(".uploads"),
            config.min_free_disk_bytes,
        )
        .with_resumable(db)
    }

    /// Count the rest of the unfinished resumable uploads in `db` as promised.
    pub fn with_resumable(mut self, db: Arc<dyn PackageDatabase>) -> Self {
        self.resumable = Some(db);
        self
    }

    /// Measure free space with `measure` instead of `statvfs`: for tests,
    /// which cannot rely on the free space of a volume others write to.
    pub fn with_measure(
        mut self,
        measure: impl Fn(&Path) -> io::Result<u64> + Send + Sync + 'static,
    ) -> Self {
        self.measure = Arc::new(measure);
        self
    }

    /// The staging directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The bytes promised now to writes in progress through this guard (not
    /// counting resumable uploads between two requests).
    pub fn promised(&self) -> i64 {
        self.pool.promised()
    }

    /// Admit a write of `incoming` bytes (when its size is known), or refuse
    /// it with [`Error::InsufficientStorage`] when it would leave the volume
    /// with less than the reserve. The returned reservation holds the bytes
    /// until they are written through [`Self::guard`] or it is dropped.
    ///
    /// What is promised counts as used: the bytes other writes in progress
    /// declared and have not written, and what the resumable uploads still
    /// open have yet to receive. With no known size only the rest is checked;
    /// the size limit and [`Self::guard`] bound the body. When free space
    /// cannot be measured the write is let through, since a guard that fails
    /// closed would take the server down with it.
    pub async fn admit(&self, incoming: Option<u64>) -> Result<Reservation> {
        let reservation = self.pool.reserve(incoming.unwrap_or(0));
        self.check(incoming.unwrap_or(0)).await?;
        Ok(reservation)
    }

    /// The check of [`Self::admit`] for more bytes of a resumable upload,
    /// which its row already promises: an [offset](Reservations::offset) for
    /// the request that writes them.
    pub async fn resume(&self) -> Result<Reservation> {
        let reservation = self.pool.offset();
        self.check(0).await?;
        Ok(reservation)
    }

    /// The free space of the volume, measured off the async worker (a
    /// `statvfs` on a network filesystem can hang). `None` when it cannot be
    /// measured.
    pub async fn available(&self) -> Option<u64> {
        let (measure, dir) = (Arc::clone(&self.measure), self.dir.clone());
        match tokio::task::spawn_blocking(move || measure(&dir)).await {
            Ok(Ok(n)) => Some(n),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not measure free disk space");
                None
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not measure free disk space");
                None
            }
        }
    }

    /// Whether the volume has room for everything promised, `incoming` (part
    /// of it) included, and the reserve.
    async fn check(&self, incoming: u64) -> Result<()> {
        if self.reserve == 0 {
            return Ok(());
        }
        let Some(available) = self.available().await else {
            return Ok(());
        };
        let resumable = match &self.resumable {
            Some(db) => db.outstanding_upload_bytes(chrono::Utc::now()).await?,
            None => 0,
        };
        let promised = (self.pool.promised() as i128 + resumable as i128).max(0) as u64;
        let others = promised.saturating_sub(incoming);
        let needed = promised.max(incoming).saturating_add(self.reserve);
        if available < needed {
            return Err(Error::InsufficientStorage(format!(
                "{available} bytes free on the storage volume, {needed} needed \
                 ({incoming} incoming, {others} promised to other uploads in progress, \
                 and the configured reserve)"
            )));
        }
        Ok(())
    }

    /// `writer`, made to fail part-way once the volume drops below the
    /// reserve (see [`DiskReserve`]): the check [`Self::admit`] makes up
    /// front cannot see a body of unknown size, or one longer than it said.
    /// What goes through is taken off `reservation`.
    pub fn guard<'a, W>(
        &'a self,
        writer: &'a mut W,
        reservation: &'a Reservation,
    ) -> DiskReserve<'a, W> {
        DiskReserve {
            inner: writer,
            space: self,
            unchecked: 0,
            measuring: None,
            reservation: Some(reservation),
        }
    }

    /// `writer` held to the reserve, with nothing promised for it.
    pub fn unreserved<'a, W>(&'a self, writer: &'a mut W) -> DiskReserve<'a, W> {
        DiskReserve {
            inner: writer,
            space: self,
            unchecked: 0,
            measuring: None,
            reservation: None,
        }
    }
}

/// Bytes promised to writes still coming in to one volume: what each has
/// declared and not yet written.
///
/// A free-space check alone does not reserve anything. N uploads that arrive
/// together each see the same free space, each pass, and together write N
/// times what was checked. Admitting an upload adds its size here first, and
/// the check counts what is promised to the others too.
#[derive(Debug, Default)]
pub struct Reservations {
    promised: AtomicI64,
}

impl Reservations {
    /// The bytes promised now, over every [`Reservation`] still held. Below
    /// zero while [offsets](Self::offset) hold more than is promised here:
    /// the bytes they took off are still counted where they were promised.
    pub fn promised(&self) -> i64 {
        self.promised.load(Ordering::SeqCst)
    }

    /// Promise `bytes` to an upload. Each byte written through the
    /// reservation (see [`SpaceGuard::guard`]) is taken off again, since the
    /// volume's free space shows it from then on; what is left goes when the
    /// reservation is dropped, finished or not.
    pub fn reserve(self: &Arc<Self>, bytes: u64) -> Reservation {
        let bytes = bytes.min(i64::MAX as u64) as i64;
        self.promised.fetch_add(bytes, Ordering::SeqCst);
        Reservation {
            pool: Arc::clone(self),
            held: AtomicI64::new(bytes),
            bounded: true,
        }
    }

    /// A reservation for bytes promised somewhere else — a resumable upload's
    /// row, which says how much of it is still to come. It starts at nothing,
    /// and each byte written through it is taken off until it is dropped:
    /// written, the byte shows in the free space while the row still counts
    /// it, and would otherwise be counted twice.
    pub fn offset(self: &Arc<Self>) -> Reservation {
        Reservation {
            pool: Arc::clone(self),
            held: AtomicI64::new(0),
            bounded: false,
        }
    }
}

/// One upload's share of [`Reservations`], released when dropped.
#[derive(Debug)]
pub struct Reservation {
    pool: Arc<Reservations>,
    held: AtomicI64,
    /// Whether the share stops at zero: true for a promise of its own, false
    /// for an [`offset`](Reservations::offset), which goes below.
    bounded: bool,
}

impl Reservation {
    /// `n` bytes went to disk through this upload.
    fn consumed(&self, n: u64) {
        let n = n.min(i64::MAX as u64) as i64;
        // One writer at a time per reservation, so no other update races this.
        let held = self.held.load(Ordering::SeqCst);
        let take = if self.bounded { n.min(held.max(0)) } else { n };
        self.held.store(held - take, Ordering::SeqCst);
        self.pool.promised.fetch_sub(take, Ordering::SeqCst);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let held = self.held.load(Ordering::SeqCst);
        self.pool.promised.fetch_sub(held, Ordering::SeqCst);
    }
}

/// A writer that keeps measuring the free space of a [`SpaceGuard`]'s volume
/// while bytes go through it, and fails with [`io::ErrorKind::StorageFull`]
/// once less than the reserve is left.
///
/// Free space is measured every [`RESERVE_CHECK_INTERVAL`] bytes, on the
/// blocking pool: the write waits for the measurement without holding up the
/// async worker. A reserve of zero turns it off, and a volume that cannot be
/// measured is let through, as by the check up front.
pub struct DiskReserve<'a, W> {
    inner: &'a mut W,
    space: &'a SpaceGuard,
    unchecked: u64,
    measuring: Option<tokio::task::JoinHandle<io::Result<u64>>>,
    reservation: Option<&'a Reservation>,
}

impl<W> DiskReserve<'_, W> {
    /// Ready once the free space allows another write: starts a measurement
    /// when one is due and waits for it.
    fn poll_check(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let reserve = self.space.reserve;
        if reserve == 0 {
            return Poll::Ready(Ok(()));
        }
        if self.measuring.is_none() {
            if self.unchecked < RESERVE_CHECK_INTERVAL {
                return Poll::Ready(Ok(()));
            }
            self.unchecked = 0;
            let (measure, dir) = (Arc::clone(&self.space.measure), self.space.dir.clone());
            self.measuring = Some(tokio::task::spawn_blocking(move || measure(&dir)));
        }
        let handle = self.measuring.as_mut().expect("set above");
        let measured = std::task::ready!(Pin::new(handle).poll(cx));
        self.measuring = None;
        match measured {
            Ok(Ok(available)) if available < reserve => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::StorageFull,
                format!(
                    "{available} bytes free on the storage volume, {reserve} needed (the \
                     configured reserve)"
                ),
            ))),
            Ok(Ok(_)) => Poll::Ready(Ok(())),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "could not measure free disk space");
                Poll::Ready(Ok(()))
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not measure free disk space");
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for DiskReserve<'_, W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        std::task::ready!(this.poll_check(cx))?;
        let poll = Pin::new(&mut *this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = &poll {
            this.unchecked += *n as u64;
            if let Some(reservation) = this.reservation {
                reservation.consumed(*n as u64);
            }
        }
        poll
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.get_mut().inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tokio::io::AsyncWriteExt;

    /// A guard whose volume always has `free` bytes.
    fn fixed(reserve: u64, free: u64) -> SpaceGuard {
        SpaceGuard::new("/nowhere", reserve).with_measure(move |_| Ok(free))
    }

    #[tokio::test]
    async fn an_unmeetable_reserve_stops_a_stream_part_way() {
        let space = fixed(10, 5);
        let chunks = (0..3).map(|_| Ok(Bytes::from(vec![0u8; RESERVE_CHECK_INTERVAL as usize])));
        let mut sink = tokio::io::sink();
        let mut guarded = space.unreserved(&mut sink);
        let err = crate::streaming::stream_to_writer(futures::stream::iter(chunks), &mut guarded)
            .await
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::StorageFull);
        // The first interval goes through unmeasured; the up-front check
        // covers it.
        assert_eq!(guarded.unchecked, 0);

        // A reserve of zero never measures.
        let space = fixed(0, 0);
        let chunks = (0..3).map(|_| Ok(Bytes::from(vec![0u8; RESERVE_CHECK_INTERVAL as usize])));
        let mut off = space.unreserved(&mut sink);
        let summary = crate::streaming::stream_to_writer(futures::stream::iter(chunks), &mut off)
            .await
            .unwrap();
        assert_eq!(summary.size, 3 * RESERVE_CHECK_INTERVAL);
    }

    #[tokio::test]
    async fn a_reservation_holds_what_is_still_to_come_until_dropped() {
        let space = fixed(0, 0);
        let pool = &space.pool;
        let first = pool.reserve(1000);
        let second = pool.reserve(500);
        assert_eq!(pool.promised(), 1500);

        // Written bytes leave the promise; more than was promised never takes
        // off another upload's share.
        let mut sink = tokio::io::sink();
        let mut guarded = space.guard(&mut sink, &first);
        guarded.write_all(&[0u8; 400]).await.unwrap();
        assert_eq!(pool.promised(), 1100);
        guarded.write_all(&[0u8; 1000]).await.unwrap();
        assert_eq!(pool.promised(), 500);
        drop(first);
        assert_eq!(pool.promised(), 500);
        // An upload that stops part-way gives back the rest.
        drop(second);
        assert_eq!(pool.promised(), 0);

        // An offset takes written bytes off what others promised for it, and
        // puts them back when it goes.
        let row = pool.reserve(1000);
        let offset = pool.offset();
        let mut guarded = space.guard(&mut sink, &offset);
        guarded.write_all(&[0u8; 300]).await.unwrap();
        assert_eq!(pool.promised(), 700);
        drop(offset);
        assert_eq!(pool.promised(), 1000);
        drop(row);
        assert_eq!(pool.promised(), 0);
    }

    #[tokio::test]
    async fn admission_counts_what_is_promised_to_others() {
        let space = fixed(100, 1000);
        let first = space.admit(Some(600)).await.unwrap();
        // 600 promised, 600 more and the reserve do not fit in 1000.
        let err = space.admit(Some(600)).await.unwrap_err();
        assert!(matches!(err, Error::InsufficientStorage(_)), "{err}");
        assert!(err.to_string().contains("600 promised to other"), "{err}");
        // A refusal promises nothing.
        assert_eq!(space.promised(), 600);
        drop(first);
        let _second = space.admit(Some(600)).await.unwrap();
        // Unknown size: only the reserve and what is promised.
        let _chunked = space.admit(None).await.unwrap();
        assert!(fixed(100, 50).admit(None).await.is_err());
        // Off, nothing is checked.
        assert!(fixed(0, 0).admit(Some(u64::MAX)).await.is_ok());
    }

    #[tokio::test]
    async fn an_unmeasurable_volume_lets_writes_through() {
        let space = SpaceGuard::new("/nowhere", 100)
            .with_measure(|_| Err(io::Error::other("no statvfs here")));
        assert!(space.admit(Some(u64::MAX / 2)).await.is_ok());
    }
}
