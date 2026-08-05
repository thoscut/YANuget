//! A process-global keyed async lock, serializing the multi-step operations
//! that touch one package version's shared storage and database rows.
//!
//! Indexing (store payload → upsert metadata → add membership) and purging
//! (remove membership → GC the payload when the last feed drops it) are each a
//! sequence of independent storage and SQLite operations with no enclosing
//! transaction. Because the payload and metadata are *shared* across feeds, a
//! push into one feed can race a purge from another feed for the **same**
//! version and either lose the payload or leave a membership pointing at
//! nothing. Holding [`lock_version`] for the version's `{id}/{version}` key
//! across each sequence makes them mutually exclusive per version, while leaving
//! operations on *different* versions fully concurrent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// The keyed registry of per-version locks. Entries hold a [`Weak`] so a lock is
/// reclaimable once no caller holds or waits on it; dead entries are swept
/// opportunistically so the map stays bounded over a long uptime instead of
/// retaining one entry for every version the process ever touched.
#[derive(Default)]
struct Registry {
    map: HashMap<String, Weak<AsyncMutex<()>>>,
    /// Sweep dead entries once the map grows to at least this size.
    sweep_at: usize,
}

fn registry() -> &'static Mutex<Registry> {
    static LOCKS: OnceLock<Mutex<Registry>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(Registry::default()))
}

/// Return the lock for `key`, creating it if no live one exists. Before
/// inserting, dead `Weak`s are swept once the map crosses a growing threshold,
/// which amortises the cleanup to O(1) per call while bounding the map at
/// roughly twice the set of in-flight versions.
fn acquire(reg: &mut Registry, key: String) -> Arc<AsyncMutex<()>> {
    if reg.map.len() >= reg.sweep_at {
        reg.map.retain(|_, weak| weak.strong_count() > 0);
        reg.sweep_at = reg.map.len() * 2 + 16;
    }
    match reg.map.get(&key).and_then(Weak::upgrade) {
        Some(existing) => existing,
        None => {
            let created = Arc::new(AsyncMutex::new(()));
            reg.map.insert(key, Arc::downgrade(&created));
            created
        }
    }
}

/// Acquire the lock for one package version. The returned guard releases on
/// drop. Keys are normalized so callers using different casing agree.
pub async fn lock_version(id: &str, normalized_version: &str) -> OwnedMutexGuard<()> {
    mutex_for(id, normalized_version).lock_owned().await
}

/// Take the lock only if it is free right now, otherwise return `None`.
///
/// For work that another task is already doing and that nobody needs done
/// twice — a read-through mirror fetch, say — waiting is worse than declining:
/// the waiter would hold a request open for as long as the winner takes, while
/// its result will be there for the next request anyway.
pub fn try_lock_version(id: &str, normalized_version: &str) -> Option<OwnedMutexGuard<()>> {
    mutex_for(id, normalized_version).try_lock_owned().ok()
}

fn mutex_for(id: &str, normalized_version: &str) -> Arc<AsyncMutex<()>> {
    let key = format!(
        "{}/{}",
        id.to_ascii_lowercase(),
        normalized_version.to_ascii_lowercase()
    );
    // See the note in `ratelimit`: a poisoned process-global mutex on a hot
    // path is a permanent outage, and the guarded map degrades gracefully.
    let mut reg = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    acquire(&mut reg, key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn same_key_is_mutually_exclusive() {
        let counter = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let counter = counter.clone();
            let max = max.clone();
            handles.push(tokio::spawn(async move {
                let _g = lock_version("Pkg", "1.0.0").await;
                let now = counter.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                counter.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // Never more than one holder of the same key at a time.
        assert_eq!(max.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn try_lock_declines_instead_of_waiting() {
        let held = lock_version("Busy", "1.0.0").await;
        // A second caller must not block behind the holder.
        assert!(try_lock_version("busy", "1.0.0").is_none());
        // A different version is unaffected.
        assert!(try_lock_version("busy", "2.0.0").is_some());
        drop(held);
        assert!(try_lock_version("Busy", "1.0.0").is_some());
    }

    #[tokio::test]
    async fn different_keys_do_not_block() {
        let _a = lock_version("A", "1.0.0").await;
        // A different key must be acquirable while `_a` is held.
        let _b = lock_version("B", "1.0.0").await;
    }

    #[test]
    fn registry_returns_same_lock_for_a_live_key() {
        let mut reg = Registry::default();
        let a = acquire(&mut reg, "pkg/1.0.0".into());
        let b = acquire(&mut reg, "pkg/1.0.0".into());
        // While at least one strong ref is alive, the same lock is returned.
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn registry_reclaims_dropped_entries() {
        let mut reg = Registry::default();
        // Touch many distinct versions, each released immediately.
        for i in 0..1000 {
            let _m = acquire(&mut reg, format!("pkg/1.0.{i}"));
        }
        // Despite 1000 distinct versions, the map stays bounded and holds no
        // live locks, because dead Weaks are swept as the threshold is crossed.
        assert!(reg.map.len() <= reg.sweep_at);
        let live = reg.map.values().filter(|w| w.strong_count() > 0).count();
        assert_eq!(live, 0);
    }
}
