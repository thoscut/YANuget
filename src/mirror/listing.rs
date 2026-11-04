//! Fetching a package's upstream version list and following the upstream's
//! listing.

use std::sync::Arc;
use std::time::Instant;

use crate::error::Result;
use crate::version::NuGetVersion;

use super::target::{Fetched, MirrorTarget};
use super::tracker::Listing;
use super::{MirrorClient, MIRROR_BUDGET};

impl MirrorTarget<'_> {
    pub(super) async fn fetch_listing(&self, lower_id: &str) -> Result<usize> {
        let client = self.client;
        let feed = self.feed;
        // This request holds the fetch, so only the list's age matters.
        if self.backing_off(lower_id) || !client.list_is_stale(feed, lower_id, false) {
            return Ok(0);
        }
        self.resources_ready().await?;
        let listed = client.upstream_versions(lower_id).await;
        self.record_upstream(lower_id, listed.is_ok());
        let mut versions: Vec<NuGetVersion> = listed?
            .iter()
            .filter_map(|raw| match NuGetVersion::parse(raw) {
                Ok(version) => Some(version),
                // A version NuGet itself would refuse cannot be mirrored, but
                // it must not cost the package its other versions either.
                Err(e) => {
                    tracing::warn!(id = %lower_id, version = %raw, error = %e, "skipping an upstream version that does not parse");
                    None
                }
            })
            .collect();
        // Newest first, so a bounded fetch keeps the versions clients actually want.
        versions.sort_by(|a, b| b.cmp(a));
        let considered = versions.len();
        if let Some(max) = client.max_versions_per_package() {
            versions.truncate(max);
        }
        if versions.len() < considered {
            tracing::debug!(
                %feed, id = %lower_id, considered, fetching = versions.len(),
                "limiting mirrored versions (mirror.max_versions_per_package)"
            );
        }
        let mut mirrored = 0;
        let deadline = Instant::now() + MIRROR_BUDGET;
        let mut complete = true;
        // Read once, when the first version is fetched, so each one is
        // indexed with the upstream's listing from the start, and shared with
        // the reconciliation after the loop.
        let mut listing: Option<Option<Listing>> = None;

        for version in versions {
            if !self.wanted(lower_id, &version).await {
                continue;
            }
            if Instant::now() >= deadline {
                tracing::info!(
                    %feed,
                    id = %lower_id,
                    mirrored,
                    "mirror budget spent; the rest of this package's versions will \
                     be fetched on a later request"
                );
                complete = false;
                break;
            }
            if listing.is_none() {
                listing = Some(self.upstream_listing(lower_id, true).await);
            }
            let upstream = listing.as_ref().and_then(|l| l.as_deref());
            match self.fetch_one(lower_id, &version, upstream).await {
                Fetched::Mirrored => mirrored += 1,
                Fetched::NotMirrored => {}
                Fetched::DownloadFailed(e) => {
                    tracing::warn!(%feed, id = %lower_id, version = %version.normalized(), error = %e, "mirror download failed");
                }
            }
        }
        // The versions held from before; the ones just mirrored were indexed
        // with the upstream's listing already.
        self.reconcile_listing(lower_id, listing).await;
        if complete {
            client.tracker.with(
                &MirrorClient::key(feed, lower_id),
                client.refresh,
                |state| {
                    state.listed_at = Some(Instant::now());
                },
            );
        }
        Ok(mirrored)
    }

    /// Bring the listing of every version of `lower_id` the feed holds in
    /// line with the upstream's, on each refresh: a version unlisted there
    /// after it was mirrored is unlisted here, and one relisted there is
    /// listed again. Not where the listing was set by hand, which wins (see
    /// [`PackageDatabase::follow_upstream_listing`](crate::database::PackageDatabase::follow_upstream_listing)), nor for a version the
    /// upstream's registration does not mention. `listing` is the one this
    /// refresh read already, if it did. Best-effort: a registration that
    /// cannot be read changes nothing, and the next refresh tries again.
    async fn reconcile_listing(&self, lower_id: &str, listing: Option<Option<Listing>>) {
        let feed = self.feed;
        let held = match self.db.upstream_listed(feed, lower_id).await {
            Ok(held) if !held.is_empty() => held,
            Ok(_) => return,
            Err(e) => {
                tracing::warn!(%feed, id = %lower_id, error = %e, "could not read the versions to follow the upstream's listing for");
                return;
            }
        };
        let listing = match listing {
            Some(listing) => listing,
            None => self.upstream_listing(lower_id, true).await,
        };
        let Some(listing) = listing else {
            return;
        };
        for (normalized, listed_here) in held {
            let Some(&listed) = listing.get(&normalized.to_lowercase()) else {
                continue;
            };
            if listed == listed_here {
                continue;
            }
            let Ok(version) = NuGetVersion::parse(&normalized) else {
                continue;
            };
            match self
                .db
                .follow_upstream_listing(feed, lower_id, &version, listed)
                .await
            {
                Ok(true) => {
                    tracing::info!(%feed, id = %lower_id, version = %normalized, listed, "followed the upstream's listing");
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(%feed, id = %lower_id, error = %e, "could not follow the upstream's listing");
                }
            }
        }
    }

    /// The upstream's listing of `lower_id`, or `None`, logged, when its
    /// registration cannot be read. Read anew with `fresh`; otherwise the one
    /// read within `refresh_secs` serves, if there is one.
    pub(super) async fn upstream_listing(&self, lower_id: &str, fresh: bool) -> Option<Listing> {
        let client = self.client;
        let key = MirrorClient::key(self.feed, lower_id);
        if !fresh {
            let cached = client.tracker.with(&key, client.refresh, |state| {
                state
                    .listing
                    .as_ref()
                    .filter(|(at, _)| at.elapsed() < client.refresh)
                    .map(|(_, listing)| listing.clone())
            });
            if cached.is_some() {
                return cached;
            }
        }
        match client.upstream_listing(lower_id).await {
            Ok(listing) => {
                let listing = Arc::new(listing);
                client.tracker.with(&key, client.refresh, |state| {
                    state.listing = Some((Instant::now(), listing.clone()));
                });
                Some(listing)
            }
            Err(e) => {
                tracing::warn!(feed = %self.feed, id = %lower_id, error = %e, "could not read which versions the upstream unlisted");
                None
            }
        }
    }
}
