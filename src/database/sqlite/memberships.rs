//! A feed's own state for a version: `feed_packages`.

use chrono::Utc;
use sqlx::{Row, SqliteConnection};

use crate::database::{canonical_id, Membership, MembershipChange, StoredFeed};
use crate::error::{Error, Result};
use crate::version::NuGetVersion;

use super::{is_unique_violation, SqliteDatabase};

pub(super) async fn add_membership(db: &SqliteDatabase, m: &Membership) -> Result<()> {
    let mut conn = db.pool.acquire().await?;
    insert_membership(&mut conn, m).await
}

pub(super) async fn remove_membership(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let result = sqlx::query(
        "DELETE FROM feed_packages WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .execute(&db.pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// [`remove_membership`] and, when a row was removed, its tombstone, in one
/// transaction: a failure leaves the membership in place rather than gone
/// without the tombstone that keeps a mirror from fetching it back.
pub(super) async fn remove_membership_for_good(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let mut tx = db.write_tx().await?;
    let removed = sqlx::query(
        "DELETE FROM feed_packages WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .execute(&mut *tx)
    .await?
    .rows_affected()
        > 0;
    if removed {
        super::tombstones::insert_tombstone(&mut tx, feed, id, version).await?;
    }
    tx.commit().await?;
    Ok(removed)
}

pub(super) async fn get_membership(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<Option<Membership>> {
    let row = sqlx::query(
        "SELECT * FROM feed_packages WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .fetch_optional(&db.pool)
    .await?;
    Ok(row.map(|r| Membership {
        feed: r.get("feed"),
        lower_id: r.get("lower_id"),
        normalized_version: r.get("normalized_version"),
        listed: r.get::<i64, _>("listed") != 0,
        enabled: r.get::<i64, _>("enabled") != 0,
        pending: r.get::<i64, _>("pending") != 0,
        flagged: r.get::<i64, _>("flagged") != 0,
        flag_reason: r.get("flag_reason"),
        pinned: r.get::<i64, _>("pinned") != 0,
    }))
}

pub(super) async fn approve_membership(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE feed_packages SET pending = 0 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .execute(&db.pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub(super) async fn exists(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let row = sqlx::query(
        "SELECT 1 FROM feed_packages WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3 LIMIT 1",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .fetch_optional(&db.pool)
    .await?;
    Ok(row.is_some())
}

pub(super) async fn set_listed(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
    listed: bool,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE feed_packages SET listed = ?4, listing_set = 1 \
         WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .bind(i64::from(listed))
    .execute(&db.pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub(super) async fn follow_upstream_listing(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
    listed: bool,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE feed_packages SET listed = ?4 \
         WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3 \
           AND listing_set = 0 AND listed <> ?4",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .bind(i64::from(listed))
    .execute(&db.pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub(super) async fn set_enabled(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
    enabled: bool,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE feed_packages SET enabled = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .bind(i64::from(enabled))
    .execute(&db.pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub(super) async fn set_pinned(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
    pinned: bool,
) -> Result<bool> {
    let result = sqlx::query(
        "UPDATE feed_packages SET pinned = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .bind(i64::from(pinned))
    .execute(&db.pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub(super) async fn update_memberships(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    versions: &[NuGetVersion],
    change: MembershipChange,
) -> Result<u64> {
    let (sql, value) = match change {
        MembershipChange::Enabled(on) => (
            "UPDATE feed_packages SET enabled = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
            i64::from(on),
        ),
        MembershipChange::Pinned(on) => (
            "UPDATE feed_packages SET pinned = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
            i64::from(on),
        ),
        MembershipChange::Approve => (
            "UPDATE feed_packages SET pending = ?4 WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
            0,
        ),
    };
    // Dropping the transaction without committing rolls it back, so an
    // early return below leaves every row as it was.
    let mut tx = db.write_tx().await?;
    let mut updated = 0;
    for version in versions {
        let result = sqlx::query(sql)
            .bind(feed)
            .bind(canonical_id(id))
            .bind(version.normalized())
            .bind(value)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() == 0 {
            return Err(Error::PackageNotFound);
        }
        updated += result.rows_affected();
    }
    tx.commit().await?;
    Ok(updated)
}

pub(super) async fn is_servable(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<bool> {
    let row = sqlx::query(
        "SELECT 1 FROM feed_packages
         WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3
           AND enabled = 1 AND pending = 0 LIMIT 1",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .fetch_optional(&db.pool)
    .await?;
    Ok(row.is_some())
}

pub(super) async fn increment_downloads(
    db: &SqliteDatabase,
    feed: &str,
    id: &str,
    version: &NuGetVersion,
) -> Result<()> {
    sqlx::query(
        "UPDATE feed_packages SET downloads = downloads + 1
         WHERE feed = ?1 AND lower_id = ?2 AND normalized_version = ?3",
    )
    .bind(feed)
    .bind(canonical_id(id))
    .bind(version.normalized())
    .execute(&db.pool)
    .await?;
    Ok(())
}

/// Insert a membership; [`Error::PackageAlreadyExists`] when the version is
/// already in the feed.
pub(super) async fn insert_membership(conn: &mut SqliteConnection, m: &Membership) -> Result<()> {
    let result = sqlx::query(
        r#"INSERT INTO feed_packages
               (feed, lower_id, normalized_version, listed, enabled, pending,
                flagged, flag_reason, added, downloads, pinned)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)"#,
    )
    .bind(&m.feed)
    .bind(canonical_id(&m.lower_id))
    .bind(&m.normalized_version)
    .bind(i64::from(m.listed))
    .bind(i64::from(m.enabled))
    .bind(i64::from(m.pending))
    .bind(i64::from(m.flagged))
    .bind(&m.flag_reason)
    .bind(Utc::now().to_rfc3339())
    .bind(i64::from(m.pinned))
    .execute(&mut *conn)
    .await;
    match result {
        Ok(_) => Ok(()),
        Err(e) if is_unique_violation(&e) => Err(Error::PackageAlreadyExists),
        Err(e) => Err(Error::Database(e)),
    }
}

pub(super) async fn stored_feeds(db: &SqliteDatabase) -> Result<Vec<StoredFeed>> {
    let rows = sqlx::query(
        "SELECT name, SUM(versions) AS versions, SUM(exclusive) AS exclusive, \
                SUM(tombstones) AS tombstones \
         FROM ( \
             SELECT fp.feed AS name, COUNT(*) AS versions, \
                    SUM(NOT EXISTS (SELECT 1 FROM feed_packages o \
                        WHERE o.lower_id = fp.lower_id \
                          AND o.normalized_version = fp.normalized_version \
                          AND o.feed <> fp.feed)) AS exclusive, \
                    0 AS tombstones \
             FROM feed_packages fp GROUP BY fp.feed \
             UNION ALL \
             SELECT feed, 0, 0, COUNT(*) FROM tombstones GROUP BY feed \
         ) GROUP BY name ORDER BY name",
    )
    .fetch_all(&db.pool)
    .await?;
    rows.iter()
        .map(|r| {
            Ok(StoredFeed {
                name: r.try_get("name")?,
                versions: r.try_get("versions")?,
                exclusive: r.try_get("exclusive")?,
                tombstones: r.try_get("tombstones")?,
            })
        })
        .collect()
}

pub(super) async fn shared_versions(conn: &mut SqliteConnection, a: &str, b: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM feed_packages x JOIN feed_packages y \
           ON y.lower_id = x.lower_id AND y.normalized_version = x.normalized_version \
         WHERE x.feed = ?1 AND y.feed = ?2",
    )
    .bind(a)
    .bind(b)
    .fetch_one(&mut *conn)
    .await?)
}

pub(super) async fn rename_feed(db: &SqliteDatabase, from: &str, to: &str) -> Result<u64> {
    let mut tx = db.write_tx().await?;
    // Checked inside the transaction, so nothing lands in between.
    let shared = shared_versions(&mut tx, from, to).await?;
    if shared > 0 {
        return Err(Error::BadRequest(format!(
            "{shared} version(s) are in both {from:?} and {to:?}; remove them from one first"
        )));
    }
    let moved = sqlx::query("UPDATE feed_packages SET feed = ?2 WHERE feed = ?1")
        .bind(from)
        .bind(to)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    // A version deleted from both keeps one tombstone. One deleted only from
    // the new name but held under the old is held now, which a membership
    // and a tombstone together never say.
    sqlx::query("UPDATE OR IGNORE tombstones SET feed = ?2 WHERE feed = ?1")
        .bind(from)
        .bind(to)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM tombstones WHERE feed = ?1")
        .bind(from)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM tombstones WHERE feed = ?1 AND EXISTS (SELECT 1 FROM feed_packages fp \
           WHERE fp.feed = ?1 AND fp.lower_id = tombstones.lower_id \
             AND fp.normalized_version = tombstones.normalized_version)",
    )
    .bind(to)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE uploads SET feed = ?2 WHERE feed = ?1")
        .bind(from)
        .bind(to)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(moved)
}

pub(super) async fn feed_members(
    db: &SqliteDatabase,
    feed: &str,
) -> Result<Vec<(String, NuGetVersion)>> {
    let rows = sqlx::query(
        "SELECT lower_id, normalized_version FROM feed_packages WHERE feed = ?1 \
         ORDER BY lower_id, normalized_version",
    )
    .bind(feed)
    .fetch_all(&db.pool)
    .await?;
    rows.iter()
        .map(|r| {
            let version: String = r.try_get("normalized_version")?;
            let version = NuGetVersion::parse_stored(&version)
                .map_err(|e| Error::InvalidVersion(e.to_string()))?;
            Ok((r.try_get("lower_id")?, version))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;

    #[tokio::test]
    async fn a_removal_for_good_takes_the_membership_and_writes_the_tombstone_together() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let (a, b) = (sample("Gone.Pkg", "1.0.0"), sample("Gone.Pkg", "2.0.0"));
        db.add_to_feed(FEED, &a).await.unwrap();
        db.add_to_feed(FEED, &b).await.unwrap();

        assert!(db
            .remove_membership_for_good(FEED, "gone.pkg", &a.version)
            .await
            .unwrap());
        assert!(db
            .get_membership(FEED, "gone.pkg", &a.version)
            .await
            .unwrap()
            .is_none());
        assert!(db
            .is_tombstoned(FEED, "gone.pkg", &a.version)
            .await
            .unwrap());
        // Nothing to remove: no tombstone either.
        let absent = NuGetVersion::parse("3.0.0").unwrap();
        assert!(!db
            .remove_membership_for_good(FEED, "gone.pkg", &absent)
            .await
            .unwrap());
        assert!(!db.is_tombstoned(FEED, "gone.pkg", &absent).await.unwrap());

        // The tombstone cannot be written: the membership stays.
        sqlx::query(
            "CREATE TRIGGER refuse_tombstones BEFORE INSERT ON tombstones \
             BEGIN SELECT RAISE(ABORT, 'refused'); END",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        db.remove_membership_for_good(FEED, "gone.pkg", &b.version)
            .await
            .unwrap_err();
        assert!(db
            .get_membership(FEED, "gone.pkg", &b.version)
            .await
            .unwrap()
            .is_some());
        assert!(!db
            .is_tombstoned(FEED, "gone.pkg", &b.version)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_batch_membership_update_is_all_or_nothing() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let (a, b) = (sample("Batch.Pkg", "1.0.0"), sample("Batch.Pkg", "2.0.0"));
        db.add_to_feed(FEED, &a).await.unwrap();
        db.add_to_feed(FEED, &b).await.unwrap();
        let missing = NuGetVersion::parse("3.0.0").unwrap();

        // One version the feed does not hold: nothing changes.
        let err = db
            .update_memberships(
                FEED,
                "batch.pkg",
                &[a.version.clone(), missing, b.version.clone()],
                MembershipChange::Enabled(false),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, Error::PackageNotFound));
        assert!(db.is_servable(FEED, "batch.pkg", &a.version).await.unwrap());

        let n = db
            .update_memberships(
                FEED,
                "Batch.Pkg",
                &[a.version.clone(), b.version.clone()],
                MembershipChange::Enabled(false),
            )
            .await
            .unwrap();
        assert_eq!(n, 2);
        assert!(!db.is_servable(FEED, "batch.pkg", &a.version).await.unwrap());
        assert!(!db.is_servable(FEED, "batch.pkg", &b.version).await.unwrap());
    }

    #[tokio::test]
    async fn pending_membership_is_withheld_until_approved() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let p = sample("Ring.Pkg", "1.0.0");
        db.upsert_package_data(&p).await.unwrap();
        db.add_membership(&Membership {
            pending: true,
            ..Membership::active("stable", &p)
        })
        .await
        .unwrap();

        // Pending: present, but not servable and hidden from listings/search.
        assert!(db.exists("stable", "ring.pkg", &p.version).await.unwrap());
        assert!(!db
            .is_servable("stable", "ring.pkg", &p.version)
            .await
            .unwrap());
        assert!(db
            .find("stable", "ring.pkg", &p.version)
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            db.search("stable", &SearchRequest::default())
                .await
                .unwrap()
                .total_hits,
            0
        );
        // Admin still sees it (as pending).
        let all = db.find_all_versions("stable", "ring.pkg").await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].pending);

        // Approving clears the gate.
        assert!(db
            .approve_membership("stable", "ring.pkg", &p.version)
            .await
            .unwrap());
        assert!(db
            .is_servable("stable", "ring.pkg", &p.version)
            .await
            .unwrap());
        assert_eq!(
            db.search("stable", &SearchRequest::default())
                .await
                .unwrap()
                .total_hits,
            1
        );
    }

    #[tokio::test]
    async fn disabled_versions_are_hidden_but_present() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        db.add_to_feed(FEED, &sample("Pkg", "1.0.0")).await.unwrap();
        db.add_to_feed(FEED, &sample("Pkg", "2.0.0")).await.unwrap();
        let v1 = NuGetVersion::parse("1.0.0").unwrap();

        // Disable 1.0.0 — hidden from public listings, search and serving.
        assert!(db.set_enabled(FEED, "pkg", &v1, false).await.unwrap());
        assert!(!db.is_servable(FEED, "pkg", &v1).await.unwrap());
        assert!(db
            .is_servable(FEED, "pkg", &NuGetVersion::parse("2.0.0").unwrap())
            .await
            .unwrap());
        assert!(db.find(FEED, "pkg", &v1).await.unwrap().is_none());

        let listed = db.find_versions(FEED, "pkg", true).await.unwrap();
        assert_eq!(listed.len(), 1); // only 2.0.0
        let page = db.search(FEED, &SearchRequest::default()).await.unwrap();
        assert_eq!(page.groups[0].packages.len(), 1);

        // But it still exists and admin listing shows it.
        assert!(db.exists(FEED, "pkg", &v1).await.unwrap());
        let all = db.find_all_versions(FEED, "pkg").await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|p| !p.package.enabled));

        // Re-enable restores visibility.
        assert!(db.set_enabled(FEED, "pkg", &v1, true).await.unwrap());
        assert!(db.is_servable(FEED, "pkg", &v1).await.unwrap());
        assert_eq!(db.find_versions(FEED, "pkg", true).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_rename_moves_memberships_tombstones_and_uploads() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let (a, b) = (sample("Moved.Pkg", "1.0.0"), sample("Moved.Pkg", "2.0.0"));
        db.add_to_feed("old", &a).await.unwrap();
        db.set_enabled("old", "moved.pkg", &a.version, false)
            .await
            .unwrap();
        db.add_tombstone("old", "moved.pkg", &b.version)
            .await
            .unwrap();
        // Deleted from the new name, but still held under the old one.
        db.add_tombstone("new", "moved.pkg", &a.version)
            .await
            .unwrap();
        db.create_upload(&UploadSession {
            id: "up".into(),
            feed: "old".into(),
            lower_id: "moved.pkg".into(),
            normalized_version: "1.0.0".into(),
            name: "f.iso".into(),
            length: 1,
            received: 0,
            expected_sha256: None,
            expires: Utc::now(),
        })
        .await
        .unwrap();

        assert_eq!(db.rename_feed("old", "new").await.unwrap(), 1);
        // The state comes along.
        let moved = db.get_membership("new", "moved.pkg", &a.version).await;
        assert!(!moved.unwrap().unwrap().enabled);
        assert!(!db.exists("old", "moved.pkg", &a.version).await.unwrap());
        assert!(db
            .is_tombstoned("new", "moved.pkg", &b.version)
            .await
            .unwrap());
        assert!(!db
            .is_tombstoned("new", "moved.pkg", &a.version)
            .await
            .unwrap());
        assert!(!db
            .is_tombstoned("old", "moved.pkg", &b.version)
            .await
            .unwrap());
        assert_eq!(db.get_upload("up").await.unwrap().unwrap().feed, "new");
        assert!(db
            .stored_feeds()
            .await
            .unwrap()
            .iter()
            .all(|f| f.name == "new"));
    }

    #[tokio::test]
    async fn a_rename_onto_a_feed_holding_the_same_version_is_refused() {
        let db = SqliteDatabase::in_memory().await.unwrap();
        let p = sample("Both.Pkg", "1.0.0");
        db.add_to_feed("old", &p).await.unwrap();
        db.add_to_feed("new", &p).await.unwrap();
        assert_eq!(db.shared_versions("old", "new").await.unwrap(), 1);
        assert!(matches!(
            db.rename_feed("old", "new").await,
            Err(Error::BadRequest(_))
        ));
        assert!(db.exists("old", "both.pkg", &p.version).await.unwrap());
    }
}
