//! Reading the upstream's documents: service-index resources, version
//! lists and listings, package downloads and id enumeration.

use std::collections::HashMap;
use std::path::Path;

use futures::StreamExt;
use yanuget_pkg::fetch::with_slash;
use yanuget_pkg::v3::{
    resolve_id, Catalog, PackageVersions, RegistrationIndex, SearchPage, ServiceIndex,
};

use crate::error::{Error, Result};
use crate::streaming;
use crate::version::NuGetVersion;

use super::client::{error_chain, redact_url};
use super::MirrorClient;

#[derive(Debug, Clone)]
pub(super) struct MirrorResources {
    /// `PackageBaseAddress/3.0.0`, with a trailing slash.
    package_base: String,
    /// `SearchQueryService` (any advertised version), when present. Used to
    /// enumerate the upstream's package ids for a full migration.
    search: Option<String>,
    /// `Catalog/3.0.0`, when present. Walked alongside search, which some
    /// servers cap or lack.
    catalog: Option<String>,
    /// `RegistrationsBaseUrl` (the SemVer 2.0.0 hive when advertised), with a
    /// trailing slash. Where the upstream says which versions are unlisted.
    registration: Option<String>,
}

/// The package ids an upstream exposes, and what could not be read while
/// finding them.
#[derive(Debug, Default)]
pub struct Enumeration {
    /// Ids in their original casing, de-duplicated case-insensitively.
    pub ids: Vec<String>,
    /// `(what, error)` for each part of the upstream that failed to load —
    /// a catalog page, say. Ids it held are missing from `ids`.
    pub failures: Vec<(String, String)>,
}

impl MirrorClient {
    pub(super) async fn resources(&self) -> Result<&MirrorResources> {
        self.resources
            .get_or_try_init(|| async {
                let index: ServiceIndex = self
                    .get_json(&self.upstream, "service index")
                    .await?
                    .ok_or_else(|| not_found("service index", &self.upstream))?;
                // A resource's `@id` may be relative to the index.
                let base = reqwest::Url::parse(&self.upstream).map_err(|e| {
                    Error::Other(anyhow::anyhow!(
                        "upstream {} is not a URL: {e}",
                        redact_url(&self.upstream)
                    ))
                })?;
                let package_base = index
                    .resolve(&["PackageBaseAddress/3.0.0"], &base)
                    .map(with_slash)
                    .map(String::from)
                    .ok_or_else(|| {
                        Error::Other(anyhow::anyhow!(
                            "upstream {} has no PackageBaseAddress resource",
                            redact_url(&self.upstream)
                        ))
                    })?;
                // Search service type names are versioned; accept whichever the
                // upstream advertises, newest first.
                let search = index
                    .resolve(
                        &[
                            "SearchQueryService/3.5.0",
                            "SearchQueryService/3.0.0-rc",
                            "SearchQueryService/3.0.0-beta",
                            "SearchQueryService",
                        ],
                        &base,
                    )
                    .map(String::from);
                let catalog = index.resolve(&["Catalog/3.0.0"], &base).map(String::from);
                let registration = index
                    .resolve(
                        &[
                            "RegistrationsBaseUrl/3.6.0",
                            "RegistrationsBaseUrl/3.4.0",
                            "RegistrationsBaseUrl/3.0.0-rc",
                            "RegistrationsBaseUrl/3.0.0-beta",
                            "RegistrationsBaseUrl",
                        ],
                        &base,
                    )
                    .map(with_slash)
                    .map(String::from);
                // Every one of these is a URL the *upstream* chose; vet each
                // before it is ever fetched. A bad optional resource is dropped
                // rather than fatal, so one odd entry cannot disable mirroring.
                self.check_url(&package_base, "PackageBaseAddress")?;
                Ok(MirrorResources {
                    package_base,
                    search: search.filter(|u| self.log_check(u, "SearchQueryService")),
                    catalog: catalog.filter(|u| self.log_check(u, "Catalog")),
                    registration: registration
                        .filter(|u| self.log_check(u, "RegistrationsBaseUrl")),
                })
            })
            .await
    }

    /// Fetch the upstream's list of version strings for `lower_id`. An absent
    /// package (404) yields an empty list rather than an error.
    pub async fn upstream_versions(&self, lower_id: &str) -> Result<Vec<String>> {
        let base = &self.resources().await?.package_base;
        let url = format!("{base}{lower_id}/index.json");
        let Some(doc) = self
            .get_json::<PackageVersions>(&url, "version list")
            .await?
        else {
            return Ok(Vec::new());
        };
        Ok(doc.versions.unwrap_or_default())
    }

    /// The versions of `lower_id` the upstream has unlisted, normalized and
    /// lower-cased, from its registration. Empty when the upstream has no
    /// registration resource or no registration for the package.
    ///
    /// The flat container a mirror lists versions from says nothing about
    /// listing, so without this every copied version was listed: a version
    /// the upstream had deliberately hidden came back into search and
    /// "latest".
    pub async fn upstream_unlisted(
        &self,
        lower_id: &str,
    ) -> Result<std::collections::HashSet<String>> {
        Ok(self
            .upstream_listing(lower_id)
            .await?
            .into_iter()
            .filter_map(|(version, listed)| (!listed).then_some(version))
            .collect())
    }

    /// Whether the upstream lists each version of `lower_id` its registration
    /// has, by version normalized and lower-cased. A version missing from it
    /// is one the upstream says nothing about; the map is empty when the
    /// upstream has no registration resource or no registration for the
    /// package.
    pub async fn upstream_listing(&self, lower_id: &str) -> Result<HashMap<String, bool>> {
        let mut listing = HashMap::new();
        let Some(base) = &self.resources().await?.registration else {
            return Ok(listing);
        };
        let url = format!("{base}{lower_id}/index.json");
        let Some((index, index_url)) = self
            .get_json_at::<RegistrationIndex>(&url, "registration")
            .await?
        else {
            return Ok(listing);
        };
        for page in index.items.into_iter().flatten() {
            // Small packages inline their leaves; large ones page them out,
            // at an `@id` that may be relative to where the index came from.
            let page = if page.items.is_some() {
                page
            } else {
                let Some(page_url) = page.id.as_deref().map(|id| {
                    resolve_id(&index_url, id).map_or_else(|| id.to_string(), String::from)
                }) else {
                    continue;
                };
                self.get_json(&page_url, "registration page")
                    .await?
                    .ok_or_else(|| not_found("registration page", &page_url))?
            };
            for leaf in page.items.iter().flatten() {
                let entry = leaf.entry();
                let version = entry
                    .version
                    .as_deref()
                    .and_then(|v| NuGetVersion::parse(v).ok());
                if let Some(version) = version {
                    listing.insert(version.normalized().to_lowercase(), entry.is_listed());
                }
            }
        }
        Ok(listing)
    }

    /// Stream the upstream's `.nupkg` for `lower_id`/`version` to `dest`,
    /// returning the [`StreamSummary`](streaming::StreamSummary) (size and
    /// SHA-512) computed while streaming — so the caller never re-reads the file
    /// just to hash it.
    pub async fn download_nupkg(
        &self,
        lower_id: &str,
        version: &str,
        dest: &Path,
    ) -> Result<streaming::StreamSummary> {
        self.download_nupkg_with_progress(lower_id, version, dest, &|_| {})
            .await
    }

    /// [`Self::download_nupkg`], calling `progress` with the size of each
    /// chunk as it arrives.
    pub async fn download_nupkg_with_progress(
        &self,
        lower_id: &str,
        version: &str,
        dest: &Path,
        progress: &(dyn Fn(u64) + Send + Sync),
    ) -> Result<streaming::StreamSummary> {
        let base = &self.resources().await?.package_base;
        let url = format!("{base}{lower_id}/{version}/{lower_id}.{version}.nupkg");
        let resp = self.send(&url, "package", self.download_deadline).await?;
        let resp = self.check_status(resp, "package")?;
        // Reject an over-sized package before a single byte hits the disk when
        // the upstream is honest about its length; the streaming cap below is
        // the real enforcement for when it is not.
        if let (Some(limit), Some(len)) = (self.max_package_size_bytes, resp.content_length()) {
            if len > limit {
                return Err(Error::PayloadTooLarge(format!(
                    "upstream package is {len} bytes, over the {limit} byte mirror limit"
                )));
            }
        }
        // The same admission as a push: the declared length is promised for
        // the rest of the download, and the reserve is watched while it runs.
        let standalone;
        let space = match &self.space {
            Some(space) => space.as_ref(),
            None => {
                standalone = crate::space::SpaceGuard::new(
                    dest.parent().unwrap_or(dest),
                    self.min_free_disk_bytes,
                );
                &standalone
            }
        };
        let reservation = space.admit(resp.content_length()).await?;
        let mut file = tokio::fs::File::create(dest).await?;
        let stream = resp.bytes_stream().map(|r| {
            if let Ok(chunk) = &r {
                progress(chunk.len() as u64);
            }
            r.map_err(|e| std::io::Error::other(error_chain(&e.without_url())))
        });
        // A mirror fetch is triggered by an ordinary (possibly anonymous) read,
        // so an unbounded copy here would let anyone fill the disk by naming
        // packages upstream happens to host. The push path is capped; so is this.
        let summary = streaming::stream_to_writer_limited(
            Box::pin(stream),
            &mut space.guard(&mut file, &reservation),
            self.max_package_size_bytes,
            // The mirror's own silence timeout already bounds a stalled upstream.
            None,
        )
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::InvalidData {
                Error::PayloadTooLarge(e.to_string())
            } else {
                Error::Io(e)
            }
        })?;
        Ok(summary)
    }

    /// Discover every package id the upstream exposes.
    ///
    /// Pages through the `SearchQueryService` with an empty query *and* walks
    /// the `Catalog/3.0.0` resource when the upstream has both, and returns
    /// the union. Either alone can come up short: a server may cap search at
    /// some number of results, and a catalog is only as complete as its
    /// pages. A search that fails outright is not fatal when there is a
    /// catalog to fall back on; a catalog page that fails is reported in
    /// [`Enumeration::failures`], because the ids on it are then missing.
    pub async fn enumerate_package_ids(&self) -> Result<Enumeration> {
        let res = self.resources().await?;
        if res.search.is_none() && res.catalog.is_none() {
            return Err(Error::Other(anyhow::anyhow!(
                "upstream {} exposes neither SearchQueryService nor Catalog/3.0.0; cannot enumerate packages",
                redact_url(&self.upstream)
            )));
        }
        let mut ids = DedupIds::new();
        let mut failures = Vec::new();
        if let Some(search) = &res.search {
            match self.enumerate_via_search(search).await {
                Ok(found) => found.iter().for_each(|id| ids.push(id)),
                Err(e) if res.catalog.is_some() => {
                    tracing::warn!(error = %e, "search enumeration failed; relying on the catalog");
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(catalog) = &res.catalog {
            match self.enumerate_via_catalog(catalog, &mut failures).await {
                Ok(found) => found.iter().for_each(|id| ids.push(id)),
                // Search already answered; a catalog that cannot even be
                // opened leaves what it found.
                Err(e) if res.search.is_some() => {
                    failures.push(("catalog".to_string(), e.to_string()));
                }
                Err(e) => return Err(e),
            }
        }
        Ok(Enumeration {
            ids: ids.into_vec(),
            failures,
        })
    }

    /// Page through the upstream `SearchQueryService` with an empty query,
    /// collecting every package id.
    ///
    /// Only an empty page ends the walk. `totalHits` cannot be trusted to:
    /// BaGetter reports the number of results on the *current page* there, so
    /// stopping once `skip >= totalHits` ended every BaGetter migration after
    /// its first page. A short page cannot either, because a server may return
    /// fewer results than `take` asked for, so `skip` advances by what the page
    /// actually held. The one extra request for the empty page at the end is
    /// the price of not depending on either.
    async fn enumerate_via_search(&self, search: &str) -> Result<Vec<String>> {
        const PAGE: usize = 100;
        // Safety valve: 50,000 pages is five million ids at a full page each,
        // far past any real feed, and stops a source that never runs dry.
        const MAX_PAGES: usize = 50_000;
        let mut ids = DedupIds::new();
        let mut skip: usize = 0;
        for _ in 0..MAX_PAGES {
            let sep = if search.contains('?') { '&' } else { '?' };
            let url = format!(
                "{search}{sep}q=&skip={skip}&take={PAGE}&prerelease=true&semVerLevel=2.0.0"
            );
            let doc: SearchPage = self
                .get_json(&url, "search page")
                .await?
                .ok_or_else(|| not_found("search page", &url))?;

            let page_len = doc.data.as_ref().map_or(0, Vec::len);
            if page_len == 0 {
                return Ok(ids.into_vec());
            }
            let known = ids.len();
            for id in page_ids(&doc) {
                ids.push(&id);
            }
            // A page of nothing but ids already seen means the source is not
            // honouring `skip` — it would answer every later page the same way.
            if ids.len() == known {
                tracing::warn!(
                    upstream = %redact_url(&self.upstream),
                    skip,
                    "search page repeated earlier results; stopping enumeration"
                );
                return Ok(ids.into_vec());
            }
            skip += page_len;
        }
        tracing::warn!(
            upstream = %redact_url(&self.upstream),
            pages = MAX_PAGES,
            "search enumeration hit its page limit; the id list may be incomplete"
        );
        Ok(ids.into_vec())
    }

    /// Walk the upstream `Catalog/3.0.0` (index → pages → items), collecting
    /// every package id. A page that fails to load is recorded in `failures`
    /// and the walk goes on.
    async fn enumerate_via_catalog(
        &self,
        catalog: &str,
        failures: &mut Vec<(String, String)>,
    ) -> Result<Vec<String>> {
        let (index, index_url): (Catalog, _) = self
            .get_json_at(catalog, "catalog index")
            .await?
            .ok_or_else(|| not_found("catalog index", catalog))?;

        // A page's `@id` may be relative to the catalog index.
        let pages: Vec<String> = index
            .items
            .into_iter()
            .flatten()
            .filter_map(|p| p.id)
            .map(|id| resolve_id(&index_url, &id).map_or(id, String::from))
            .collect();

        let mut ids = DedupIds::new();
        for page_url in pages {
            // Page URLs come from the catalog index, so `get_json` vets each
            // one like any other upstream-chosen URL.
            let page = match self.get_json::<Catalog>(&page_url, "catalog page").await {
                Ok(Some(page)) => page,
                Ok(None) => {
                    let e = not_found("catalog page", &page_url);
                    failures.push((
                        format!("catalog page {}", redact_url(&page_url)),
                        e.to_string(),
                    ));
                    continue;
                }
                Err(e) => {
                    failures.push((
                        format!("catalog page {}", redact_url(&page_url)),
                        e.to_string(),
                    ));
                    continue;
                }
            };
            for id in page
                .items
                .iter()
                .flatten()
                .filter_map(|i| i.package_id.as_deref())
            {
                ids.push(id);
            }
        }
        Ok(ids.into_vec())
    }
}

/// The error for a document the upstream said does not exist.
pub(super) fn not_found(what: &str, url: &str) -> Error {
    Error::Other(anyhow::anyhow!(
        "upstream {what} {} answered 404 Not Found",
        redact_url(url)
    ))
}

/// Extract the package ids from one `SearchQueryService` response page.
fn page_ids(doc: &SearchPage) -> Vec<String> {
    doc.data
        .iter()
        .flatten()
        .filter_map(|i| i.id.clone())
        .collect()
}

/// Accumulates package ids in first-seen order, dropping case-insensitive
/// duplicates.
#[derive(Default)]
struct DedupIds {
    seen: std::collections::HashSet<String>,
    ids: Vec<String>,
}

impl DedupIds {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, id: &str) {
        if self.seen.insert(id.to_lowercase()) {
            self.ids.push(id.to_string());
        }
    }

    fn len(&self) -> usize {
        self.ids.len()
    }

    fn into_vec(self) -> Vec<String> {
        self.ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MirrorConfig;

    #[test]
    fn page_ids_extracts_and_dedup_collects() {
        let page: SearchPage = serde_json::from_value(serde_json::json!({
            "totalHits": 3,
            "data": [
                {"id": "Alpha", "version": "1.0.0"},
                {"id": "Beta", "version": "2.0.0"},
                {"version": "9.9.9"} // no id — skipped
            ]
        }))
        .unwrap();
        assert_eq!(
            page_ids(&page),
            vec!["Alpha".to_string(), "Beta".to_string()]
        );

        // DedupIds keeps first-seen casing and drops case-insensitive repeats.
        let mut ids = DedupIds::new();
        for id in ["Alpha", "Beta", "alpha", "Gamma", "BETA"] {
            ids.push(id);
        }
        assert_eq!(
            ids.into_vec(),
            vec!["Alpha".to_string(), "Beta".to_string(), "Gamma".to_string()]
        );
    }

    #[tokio::test]
    async fn the_mirror_reads_a_list_typed_index_with_relative_ids() {
        use axum::routing::get;

        // `@type` as a list and every `@id` relative: the service index, to
        // the flat container and the registration, and the registration to
        // its page.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = axum::Router::new()
            .route(
                "/v3/index.json",
                get(|| async {
                    axum::Json(serde_json::json!({"resources": [
                        {"@id": "flat", "@type": ["PackageBaseAddress/3.0.0"]},
                        {"@id": "/v3/registration/", "@type": ["RegistrationsBaseUrl/3.6.0", "RegistrationsBaseUrl"]}
                    ]}))
                }),
            )
            .route(
                "/v3/flat/pkg/index.json",
                get(|| async { axum::Json(serde_json::json!({"versions": ["1.0.0", "2.0.0"]})) }),
            )
            .route(
                "/v3/registration/pkg/index.json",
                get(|| async { axum::Json(serde_json::json!({"items": [{"@id": "page/0.json"}]})) }),
            )
            .route(
                "/v3/registration/pkg/page/0.json",
                get(|| async {
                    axum::Json(serde_json::json!({"items": [
                        {"catalogEntry": {"id": "Pkg", "version": "1.0.0"}},
                        {"catalogEntry": {"id": "Pkg", "version": "2.0.0", "listed": false}}
                    ]}))
                }),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = MirrorClient::from_config(&MirrorConfig {
            enabled: true,
            upstream: format!("{base}/v3/index.json"),
            // The fake upstream is on loopback.
            allow_private_upstream: true,
            timeout_secs: 5,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            client.upstream_versions("pkg").await.unwrap(),
            ["1.0.0", "2.0.0"]
        );
        let listing = client.upstream_listing("pkg").await.unwrap();
        assert_eq!(listing.get("1.0.0"), Some(&true));
        assert_eq!(listing.get("2.0.0"), Some(&false));
    }

    /// How a fake upstream's search service pages.
    #[derive(Clone, Copy, Debug)]
    enum Quirk {
        /// Reports the real total in `totalHits`, as nuget.org and YANuget do.
        Honest,
        /// BaGetter: `totalHits` is the number of results on the current page.
        PageSizedTotalHits,
        /// Returns at most this many results, whatever `take` asked for.
        CapsTake(usize),
        /// Ignores `skip`: every request gets the first page again.
        IgnoresSkip,
    }

    /// Serve a service index and a search service over `total` package ids
    /// (`Pkg.000`, `Pkg.001`, …). Returns the service-index URL and a count of
    /// the search requests made.
    async fn fake_search_upstream(
        total: usize,
        quirk: Quirk,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::extract::Query;
        use axum::routing::get;
        use axum::{Json, Router};
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let index = serde_json::json!({
            "version": "3.0.0",
            "resources": [
                {"@id": format!("{base}/flat/"), "@type": "PackageBaseAddress/3.0.0"},
                {"@id": format!("{base}/query"), "@type": "SearchQueryService"}
            ]
        });
        let requests = Arc::new(AtomicUsize::new(0));
        let counter = requests.clone();
        let app = Router::new()
            .route(
                "/v3/index.json",
                get(move || {
                    let index = index.clone();
                    async move { Json(index) }
                }),
            )
            .route(
                "/query",
                get(move |Query(q): Query<HashMap<String, String>>| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let arg = |k: &str| q.get(k).and_then(|v| v.parse::<usize>().ok());
                    let mut skip = arg("skip").unwrap_or(0);
                    let mut take = arg("take").unwrap_or(20);
                    match quirk {
                        Quirk::CapsTake(cap) => take = take.min(cap),
                        Quirk::IgnoresSkip => skip = 0,
                        Quirk::Honest | Quirk::PageSizedTotalHits => {}
                    }
                    let data: Vec<serde_json::Value> = (skip..total.min(skip + take))
                        .map(|i| serde_json::json!({"id": format!("Pkg.{i:03}"), "version": "1.0.0"}))
                        .collect();
                    let total_hits = match quirk {
                        Quirk::PageSizedTotalHits => data.len(),
                        _ => total,
                    };
                    async move { Json(serde_json::json!({"totalHits": total_hits, "data": data})) }
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("{base}/v3/index.json"), requests)
    }

    async fn enumerate(upstream: String) -> Vec<String> {
        let client = MirrorClient::from_config(&MirrorConfig {
            enabled: true,
            upstream,
            // The fake upstream is on loopback.
            allow_private_upstream: true,
            ..Default::default()
        })
        .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            client.enumerate_package_ids(),
        )
        .await
        .expect("enumeration must terminate")
        .unwrap()
        .ids
    }

    #[tokio::test]
    async fn search_enumeration_reaches_every_page_whatever_totalhits_says() {
        // BaGetter's `totalHits` counts only the results on the page it
        // returns, so a walk that stops once `skip >= totalHits` ends after page
        // one: a real migration off BaGetter found 100 of its 164 package ids.
        for quirk in [Quirk::Honest, Quirk::PageSizedTotalHits] {
            let (upstream, _) = fake_search_upstream(250, quirk).await;
            let ids = enumerate(upstream).await;
            assert_eq!(ids.len(), 250, "{quirk:?}");
            assert_eq!(ids[249], "Pkg.249", "{quirk:?}");
        }
    }

    #[tokio::test]
    async fn search_enumeration_survives_a_server_that_caps_take() {
        // A server may return fewer results than `take` asked for. A short page
        // is then not the end, and `skip` has to advance by what arrived.
        let (upstream, _) = fake_search_upstream(250, Quirk::CapsTake(30)).await;
        assert_eq!(enumerate(upstream).await.len(), 250);
    }

    #[tokio::test]
    async fn search_enumeration_stops_when_the_server_ignores_skip() {
        // With nothing but an empty page to stop on, a server that answers
        // every request with its first page would be asked again until the
        // safety valve. A page that adds no new id ends the walk instead.
        let (upstream, requests) = fake_search_upstream(250, Quirk::IgnoresSkip).await;
        assert_eq!(enumerate(upstream).await.len(), 100);
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
