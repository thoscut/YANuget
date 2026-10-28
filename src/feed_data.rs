//! What the database still holds for feeds the configuration no longer has.
//!
//! Memberships are recorded under the feed's name. Taking a feed out of the
//! configuration, or renaming it, leaves its memberships behind under the old
//! name: nothing serves them, but they still count as a feed holding each
//! version, so the last feed never lets go of its payload, an overwrite of the
//! version elsewhere is refused with `409`, and a renamed feed starts out
//! empty. The server warns about them at startup and `yanuget config check`
//! reports them; `yanuget feeds rename` gives them to the new name and
//! `yanuget feeds remove` drops them.

use crate::config::ResolvedFeed;
use crate::database::{PackageDatabase, StoredFeed};
use crate::error::Result;
use crate::storage::PackageStorage;

/// The stored feeds no configured feed is called: holding versions, and
/// named differently from every configured feed (exactly, as memberships are
/// recorded).
pub fn unconfigured<'a>(
    stored: &'a [StoredFeed],
    configured: &[ResolvedFeed],
) -> Vec<&'a StoredFeed> {
    stored
        .iter()
        .filter(|s| s.versions > 0 && !configured.iter().any(|f| f.name == s.name))
        .collect()
}

/// The warning for one feed of [`unconfigured`], for the log and for
/// `yanuget config check`.
pub fn warning(feed: &StoredFeed) -> String {
    let name = &feed.name;
    format!(
        "feed {name:?} is not configured, but the database holds {} version(s) for it, {} of \
         them in no other feed: nothing serves or cleans them up, and they cannot be \
         overwritten elsewhere. If the feed was renamed, run `yanuget feeds rename {name} \
         <new name>`; if it is gone, `yanuget feeds remove {name}`",
        feed.versions, feed.exclusive
    )
}

/// Log a warning for each stored feed the configuration lacks.
pub async fn warn_unconfigured(db: &dyn PackageDatabase, configured: &[ResolvedFeed]) {
    match db.stored_feeds().await {
        Ok(stored) => {
            for feed in unconfigured(&stored, configured) {
                tracing::warn!(feed = %feed.name, versions = feed.versions, "{}", warning(feed));
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not check the database for feeds no longer configured");
        }
    }
}

/// What [`remove`] removed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Removed {
    /// Memberships of the feed.
    pub versions: u64,
    /// Versions no other feed held, whose global data went with them.
    pub purged: u64,
    /// Tombstones of the feed.
    pub tombstones: u64,
}

/// Drop every membership and tombstone of `feed`, and, as retention does, the
/// global data of each version no other feed holds: its payload, attached
/// files and symbols.
///
/// One version at a time under its lock, membership first: a run that fails
/// part-way leaves at most a version no feed holds, which a rerun (or the
/// server's orphan sweep) finishes. The lock is this process's own, so the
/// server must not be running; the command refuses while it answers.
pub async fn remove(
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
) -> Result<Removed> {
    let mut removed = Removed::default();
    for (id, version) in db.feed_members(feed).await? {
        let _guard = crate::locks::lock_version(&id, &version.normalized()).await;
        if db.remove_membership(feed, &id, &version).await? {
            removed.versions += 1;
        }
        if db.feed_count(&id, &version).await? == 0 && db.package_data_exists(&id, &version).await?
        {
            crate::retention::purge_global_data(storage, db, &id, &version).await?;
            removed.purged += 1;
        }
    }
    removed.tombstones = db.clear_tombstones(feed).await?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::SqliteDatabase;
    use crate::models::Package;
    use crate::storage::FilesystemStorage;

    fn package(id: &str) -> Package {
        crate::database::sqlite::test_support::sample(id, "1.0.0")
    }

    async fn store(storage: &FilesystemStorage, db: &SqliteDatabase, feed: &str, p: &Package) {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("p.nupkg");
        tokio::fs::write(&tmp, b"payload").await.unwrap();
        storage
            .store_package(&p.id, &p.version.normalized(), tmp)
            .await
            .unwrap();
        db.add_to_feed(feed, p).await.unwrap();
    }

    #[tokio::test]
    async fn only_feeds_the_configuration_lacks_are_reported() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let (a, b) = (package("A"), package("B"));
        for (feed, p) in [("live", &a), ("old", &a), ("old", &b)] {
            db.add_to_feed(feed, p).await.unwrap();
        }
        db.add_tombstone("gone", "c", &a.version).await.unwrap();
        let config: crate::config::Config = toml::from_str("[[feeds]]\nname = \"live\"\n").unwrap();
        let stored = db.stored_feeds().await.unwrap();
        assert_eq!(stored.len(), 3);
        // `gone` has only a tombstone left: nothing held, nothing to warn of.
        let stale = unconfigured(&stored, &config.resolved_feeds().unwrap());
        assert_eq!(
            stale,
            [&StoredFeed {
                name: "old".into(),
                versions: 2,
                exclusive: 1,
                tombstones: 0
            }]
        );
        assert!(warning(stale[0]).contains("yanuget feeds rename old"));
    }

    #[tokio::test]
    async fn removing_a_feed_purges_what_only_it_held() {
        let dir = tempfile::tempdir().unwrap();
        let storage = FilesystemStorage::new(dir.path()).await.unwrap();
        let db = SqliteDatabase::in_memory().await.unwrap();
        let (shared, alone) = (package("Shared"), package("Alone"));
        store(&storage, &db, "live", &shared).await;
        store(&storage, &db, "old", &shared).await;
        store(&storage, &db, "old", &alone).await;
        db.add_tombstone("old", "gone", &alone.version)
            .await
            .unwrap();

        let removed = remove(&storage, &db, "old").await.unwrap();
        assert_eq!(
            removed,
            Removed {
                versions: 2,
                purged: 1,
                tombstones: 1
            }
        );
        assert!(db.feed_members("old").await.unwrap().is_empty());
        assert!(!db
            .package_data_exists("alone", &alone.version)
            .await
            .unwrap());
        assert!(!storage.package_exists("alone", "1.0.0").await);
        // Still held by a configured feed: untouched.
        assert!(db.exists("live", "shared", &shared.version).await.unwrap());
        assert!(storage.package_exists("shared", "1.0.0").await);
        assert!(db
            .stored_feeds()
            .await
            .unwrap()
            .iter()
            .all(|f| f.name != "old"));
    }
}
