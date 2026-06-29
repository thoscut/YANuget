//! Upstream mirroring: read-through caching of a public NuGet feed.
//!
//! When a feed has a `[feeds.mirror]` upstream configured and a client asks for
//! a package the feed does not have yet, the server fetches that package's
//! versions from the upstream V3 feed, streams each `.nupkg` to disk and indexes
//! it into the local feed — after which every later request is served locally.
//!
//! Mirrored versions go through the same [`crate::indexing`] pipeline as a push,
//! so the streaming/large-package guarantees and the feed's license policy
//! apply. When the feed `requires_approval`, mirrored versions land **pending**
//! and are withheld from clients until an admin approves them in `/admin` —
//! turning the mirror into a curated, approval-gated cache.

use std::path::Path;

use futures::StreamExt;
use tokio::sync::OnceCell;

use crate::config::{LicensePolicyConfig, MirrorAuthConfig, MirrorConfig};
use crate::database::PackageDatabase;
use crate::error::{Error, Result};
use crate::indexing::{self, IndexOptions};
use crate::storage::PackageStorage;
use crate::streaming;
use crate::version::NuGetVersion;

/// A client for one upstream V3 feed, with its service-index resources resolved
/// lazily on first use and cached thereafter.
#[derive(Debug)]
pub struct MirrorClient {
    client: reqwest::Client,
    upstream: String,
    resources: OnceCell<MirrorResources>,
}

#[derive(Debug, Clone)]
struct MirrorResources {
    /// `PackageBaseAddress/3.0.0`, with a trailing slash.
    package_base: String,
    /// `SearchQueryService` (any advertised version), when present. Used to
    /// enumerate the upstream's package ids for a full migration.
    search: Option<String>,
    /// `Catalog/3.0.0`, when present. The enumeration fallback for feeds whose
    /// search service is capped or absent.
    catalog: Option<String>,
}

impl MirrorClient {
    /// Build a client from a feed's [`MirrorConfig`]. Returns `None` when
    /// mirroring is disabled for the feed.
    pub fn from_config(config: &MirrorConfig) -> Option<Self> {
        if !config.enabled {
            return None;
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs.max(1)))
            .user_agent(concat!("yanuget/", env!("CARGO_PKG_VERSION")))
            .default_headers(auth_headers(&config.auth))
            .build()
            .ok()?;
        Some(Self {
            client,
            upstream: config.upstream.clone(),
            resources: OnceCell::new(),
        })
    }

    /// The upstream service-index URL this client mirrors from.
    pub fn upstream(&self) -> &str {
        &self.upstream
    }

    async fn resources(&self) -> Result<&MirrorResources> {
        self.resources
            .get_or_try_init(|| async {
                let index: serde_json::Value = self
                    .client
                    .get(&self.upstream)
                    .send()
                    .await
                    .map_err(mirror_err)?
                    .error_for_status()
                    .map_err(mirror_err)?
                    .json()
                    .await
                    .map_err(mirror_err)?;
                let package_base =
                    find_resource(&index, "PackageBaseAddress/3.0.0").ok_or_else(|| {
                        Error::Other(anyhow::anyhow!(
                            "upstream {} has no PackageBaseAddress resource",
                            self.upstream
                        ))
                    })?;
                // Search service type names are versioned; accept whichever the
                // upstream advertises, newest first.
                let search = find_first_resource(
                    &index,
                    &[
                        "SearchQueryService/3.5.0",
                        "SearchQueryService/3.0.0-rc",
                        "SearchQueryService/3.0.0-beta",
                        "SearchQueryService",
                    ],
                );
                let catalog = find_resource(&index, "Catalog/3.0.0");
                Ok(MirrorResources {
                    package_base: ensure_trailing_slash(&package_base),
                    search,
                    catalog,
                })
            })
            .await
    }

    /// Fetch the upstream's list of version strings for `lower_id`. An absent
    /// package (404) yields an empty list rather than an error.
    pub async fn upstream_versions(&self, lower_id: &str) -> Result<Vec<String>> {
        let base = &self.resources().await?.package_base;
        let url = format!("{base}{lower_id}/index.json");
        let resp = self.client.get(&url).send().await.map_err(mirror_err)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let resp = resp.error_for_status().map_err(mirror_err)?;
        let doc: serde_json::Value = resp.json().await.map_err(mirror_err)?;
        Ok(doc
            .get("versions")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
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
        let base = &self.resources().await?.package_base;
        let url = format!("{base}{lower_id}/{version}/{lower_id}.{version}.nupkg");
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(mirror_err)?
            .error_for_status()
            .map_err(mirror_err)?;
        let mut file = tokio::fs::File::create(dest).await?;
        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(|e| std::io::Error::other(e.to_string())));
        let summary = streaming::stream_to_writer(Box::pin(stream), &mut file)
            .await
            .map_err(Error::Io)?;
        Ok(summary)
    }

    /// Discover every package id the upstream exposes.
    ///
    /// Prefers the `SearchQueryService`, paging through it with an empty query;
    /// falls back to walking the `Catalog/3.0.0` resource when the upstream has
    /// no search service (or search returns nothing while a catalog exists).
    /// Ids are returned in their original casing, de-duplicated
    /// case-insensitively.
    pub async fn enumerate_package_ids(&self) -> Result<Vec<String>> {
        let res = self.resources().await?;
        if let Some(search) = &res.search {
            let ids = self.enumerate_via_search(search).await?;
            if !ids.is_empty() {
                return Ok(ids);
            }
            // A non-empty search service that returns nothing: either a truly
            // empty feed, or one whose contents only the catalog can reveal.
            if res.catalog.is_none() {
                return Ok(ids);
            }
        }
        if let Some(catalog) = &res.catalog {
            return self.enumerate_via_catalog(catalog).await;
        }
        Err(Error::Other(anyhow::anyhow!(
            "upstream {} exposes neither SearchQueryService nor Catalog/3.0.0; cannot enumerate packages",
            self.upstream
        )))
    }

    /// Page through the upstream `SearchQueryService` with an empty query,
    /// collecting every package id.
    async fn enumerate_via_search(&self, search: &str) -> Result<Vec<String>> {
        const PAGE: i64 = 100;
        let mut ids = DedupIds::new();
        let mut skip: i64 = 0;
        loop {
            let sep = if search.contains('?') { '&' } else { '?' };
            let url = format!(
                "{search}{sep}q=&skip={skip}&take={PAGE}&prerelease=true&semVerLevel=2.0.0"
            );
            let doc: serde_json::Value = self
                .client
                .get(&url)
                .send()
                .await
                .map_err(mirror_err)?
                .error_for_status()
                .map_err(mirror_err)?
                .json()
                .await
                .map_err(mirror_err)?;

            let page_len = doc
                .get("data")
                .and_then(|d| d.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            for id in page_ids(&doc) {
                ids.push(&id);
            }

            skip += PAGE;
            // A short page means we have reached the end.
            if page_len < PAGE as usize {
                break;
            }
            // Stop once we've paged past the upstream's reported total.
            if let Some(total) = doc.get("totalHits").and_then(|t| t.as_i64()) {
                if skip >= total {
                    break;
                }
            }
            // Safety valve against an upstream that never returns a short page.
            if skip > 5_000_000 {
                break;
            }
        }
        Ok(ids.into_vec())
    }

    /// Walk the upstream `Catalog/3.0.0` (index → pages → items), collecting
    /// every package id. Best-effort: a page that fails to load is skipped.
    async fn enumerate_via_catalog(&self, catalog: &str) -> Result<Vec<String>> {
        let index: serde_json::Value = self
            .client
            .get(catalog)
            .send()
            .await
            .map_err(mirror_err)?
            .error_for_status()
            .map_err(mirror_err)?
            .json()
            .await
            .map_err(mirror_err)?;

        let pages: Vec<String> = index
            .get("items")
            .and_then(|i| i.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.get("@id").and_then(|u| u.as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        let mut ids = DedupIds::new();
        for page_url in pages {
            let page: serde_json::Value = match self
                .client
                .get(&page_url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(resp) => match resp.json().await {
                    Ok(json) => json,
                    Err(e) => {
                        tracing::warn!(page = %page_url, error = %e, "catalog page parse failed");
                        continue;
                    }
                },
                Err(e) => {
                    tracing::warn!(page = %page_url, error = %e, "catalog page fetch failed");
                    continue;
                }
            };
            if let Some(items) = page.get("items").and_then(|i| i.as_array()) {
                for item in items {
                    if let Some(id) = item.get("nuget:id").and_then(|i| i.as_str()) {
                        ids.push(id);
                    }
                }
            }
        }
        Ok(ids.into_vec())
    }
}

/// Extract the package ids from one `SearchQueryService` response page.
fn page_ids(doc: &serde_json::Value) -> Vec<String> {
    doc.get("data")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|i| i.get("id").and_then(|v| v.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
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

    fn into_vec(self) -> Vec<String> {
        self.ids
    }
}

/// Options governing how mirrored versions are ingested.
#[derive(Debug, Clone, Default)]
pub struct MirrorOptions {
    /// Mirrored versions land pending (require admin approval before serving).
    pub requires_approval: bool,
    /// The feed's license policy, applied to each mirrored version.
    pub license_policy: LicensePolicyConfig,
}

/// Ensure every upstream version of `id` is present in `feed`, fetching and
/// indexing any that are missing. Returns how many new versions were mirrored.
///
/// Best-effort: a failure for one version is logged and skipped so a single bad
/// package never blocks the rest. Versions already in the feed are left as-is.
pub async fn ensure_package(
    client: &MirrorClient,
    storage: &dyn PackageStorage,
    db: &dyn PackageDatabase,
    feed: &str,
    temp_dir: &Path,
    id: &str,
    options: &MirrorOptions,
) -> Result<usize> {
    // Only mirror well-formed package ids. This rejects anything (path
    // traversal, slashes, control characters) that could escape the upstream's
    // PackageBaseAddress path when interpolated into the request URL.
    if crate::validation::validate_package_id(id).is_err() {
        return Ok(0);
    }
    let lower_id = id.to_lowercase();
    let versions = client.upstream_versions(&lower_id).await?;
    let mut mirrored = 0;

    for raw in versions {
        let Ok(version) = NuGetVersion::parse(&raw) else {
            continue;
        };
        // Skip versions the feed already exposes.
        if db.exists(feed, &lower_id, &version).await.unwrap_or(false) {
            continue;
        }

        let normalized = version.normalized().to_lowercase();
        let temp_path = temp_dir.join(format!("mirror-{}.tmp", uuid::Uuid::new_v4()));
        let summary = match client
            .download_nupkg(&lower_id, &normalized, &temp_path)
            .await
        {
            Ok(summary) => summary,
            Err(e) => {
                tracing::warn!(%feed, id = %lower_id, version = %normalized, error = %e, "mirror download failed");
                let _ = tokio::fs::remove_file(&temp_path).await;
                continue;
            }
        };

        let opts = IndexOptions {
            overwrite: crate::config::OverwriteMode::Disabled,
            pending: options.requires_approval,
            license_policy: options.license_policy.clone(),
        };
        match indexing::index_package(storage, db, feed, temp_path, summary, &opts).await {
            Ok(_) => {
                mirrored += 1;
                tracing::info!(%feed, id = %lower_id, version = %normalized, pending = options.requires_approval, "mirrored package");
            }
            // A concurrent request may have mirrored it first — not an error.
            Err(Error::PackageAlreadyExists) => {}
            Err(e) => {
                tracing::warn!(%feed, id = %lower_id, version = %normalized, error = %e, "mirror index failed")
            }
        }
    }
    Ok(mirrored)
}

fn find_resource(index: &serde_json::Value, ty: &str) -> Option<String> {
    index
        .get("resources")?
        .as_array()?
        .iter()
        .find(|r| r.get("@type").and_then(|t| t.as_str()) == Some(ty))
        .and_then(|r| r.get("@id"))
        .and_then(|i| i.as_str())
        .map(str::to_string)
}

/// Return the first resource matching any of `types`, in the given priority
/// order (used for the versioned `SearchQueryService` type names).
fn find_first_resource(index: &serde_json::Value, types: &[&str]) -> Option<String> {
    types.iter().find_map(|ty| find_resource(index, ty))
}

fn ensure_trailing_slash(s: &str) -> String {
    if s.ends_with('/') {
        s.to_string()
    } else {
        format!("{s}/")
    }
}

fn mirror_err(e: reqwest::Error) -> Error {
    Error::Other(anyhow::anyhow!("upstream request failed: {e}"))
}

/// Build the default header map a mirror client sends on every upstream request
/// from its configured credentials. Basic and Bearer both populate
/// `Authorization` (Basic wins if both are set); custom headers are added as-is.
/// Malformed header names/values are skipped with a warning rather than failing
/// the whole client.
fn auth_headers(auth: &MirrorAuthConfig) -> reqwest::header::HeaderMap {
    use base64::Engine;
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};

    let mut headers = HeaderMap::new();
    if let Some(user) = &auth.username {
        let pass = auth.password.as_deref().unwrap_or("");
        let raw = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        if let Ok(mut value) = HeaderValue::from_str(&format!("Basic {raw}")) {
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
    } else if let Some(token) = &auth.token {
        if let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {token}")) {
            value.set_sensitive(true);
            headers.insert(AUTHORIZATION, value);
        }
    }
    for (name, value) in &auth.headers {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(n), Ok(mut v)) => {
                v.set_sensitive(true);
                headers.insert(n, v);
            }
            _ => tracing::warn!(header = %name, "ignoring invalid mirror auth header"),
        }
    }
    headers
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_headers_basic_bearer_and_custom() {
        use reqwest::header::AUTHORIZATION;

        // No credentials → no headers.
        assert!(auth_headers(&MirrorAuthConfig::default()).is_empty());

        // Basic auth populates Authorization.
        let basic = MirrorAuthConfig {
            username: Some("user".into()),
            password: Some("pass".into()),
            ..Default::default()
        };
        let h = auth_headers(&basic);
        assert!(h
            .get(AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("Basic "));

        // Bearer token plus a custom header.
        let mut headers = std::collections::BTreeMap::new();
        headers.insert("X-Feed-Key".to_string(), "abc".to_string());
        let bearer = MirrorAuthConfig {
            token: Some("tok".into()),
            headers,
            ..Default::default()
        };
        let h = auth_headers(&bearer);
        assert_eq!(
            h.get(AUTHORIZATION).unwrap().to_str().unwrap(),
            "Bearer tok"
        );
        assert_eq!(h.get("X-Feed-Key").unwrap().to_str().unwrap(), "abc");
    }

    #[test]
    fn page_ids_extracts_and_dedup_collects() {
        let page = serde_json::json!({
            "totalHits": 3,
            "data": [
                {"id": "Alpha", "version": "1.0.0"},
                {"id": "Beta", "version": "2.0.0"},
                {"version": "9.9.9"} // no id — skipped
            ]
        });
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

    #[test]
    fn finds_first_resource_in_priority_order() {
        let index = serde_json::json!({
            "resources": [
                {"@id": "https://up/search-rc", "@type": "SearchQueryService/3.0.0-rc"},
                {"@id": "https://up/search", "@type": "SearchQueryService"}
            ]
        });
        // 3.5.0 is absent, so the next candidate (3.0.0-rc) wins.
        assert_eq!(
            find_first_resource(
                &index,
                &[
                    "SearchQueryService/3.5.0",
                    "SearchQueryService/3.0.0-rc",
                    "SearchQueryService"
                ]
            )
            .as_deref(),
            Some("https://up/search-rc")
        );
        assert!(find_first_resource(&index, &["Catalog/3.0.0"]).is_none());
    }

    #[test]
    fn finds_resource_by_type() {
        let index = serde_json::json!({
            "resources": [
                {"@id": "https://up/flat/", "@type": "PackageBaseAddress/3.0.0"},
                {"@id": "https://up/query", "@type": "SearchQueryService"}
            ]
        });
        assert_eq!(
            find_resource(&index, "PackageBaseAddress/3.0.0").as_deref(),
            Some("https://up/flat/")
        );
        assert!(find_resource(&index, "Nope").is_none());
    }

    #[test]
    fn trailing_slash_is_normalized() {
        assert_eq!(ensure_trailing_slash("https://x/flat"), "https://x/flat/");
        assert_eq!(ensure_trailing_slash("https://x/flat/"), "https://x/flat/");
    }

    #[test]
    fn disabled_mirror_builds_no_client() {
        assert!(MirrorClient::from_config(&MirrorConfig::default()).is_none());
        let enabled = MirrorConfig {
            enabled: true,
            ..Default::default()
        };
        assert!(MirrorClient::from_config(&enabled).is_some());
    }
}
