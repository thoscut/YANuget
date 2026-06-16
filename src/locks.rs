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
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

type Registry = Mutex<HashMap<String, Arc<AsyncMutex<()>>>>;

fn registry() -> &'static Registry {
    static LOCKS: OnceLock<Registry> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Acquire the lock for one package version. The returned guard releases on
/// drop. Keys are normalized so callers using different casing agree.
pub async fn lock_version(id: &str, normalized_version: &str) -> OwnedMutexGuard<()> {
    let key = format!(
        "{}/{}",
        id.to_ascii_lowercase(),
        normalized_version.to_ascii_lowercase()
    );
    let mutex = {
        let mut map = registry().lock().expect("version-lock registry poisoned");
        map.entry(key)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    };
    mutex.lock_owned().await
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
    async fn different_keys_do_not_block() {
        let _a = lock_version("A", "1.0.0").await;
        // A different key must be acquirable while `_a` is held.
        let _b = lock_version("B", "1.0.0").await;
    }
}
