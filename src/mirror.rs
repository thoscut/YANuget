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

use std::path::{Path, PathBuf};

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
                Ok(MirrorResources {
                    package_base: ensure_trailing_slash(&package_base),
                })
            })
            .await
    }

    /// Fetch the upstream's list of version strings for `lower_id`. An absent
    /// package (404) yields an empty list rather than an error.
    async fn upstream_versions(&self, lower_id: &str) -> Result<Vec<String>> {
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

    async fn download_nupkg(&self, lower_id: &str, version: &str, dest: &Path) -> Result<()> {
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
        streaming::stream_to_writer(Box::pin(stream), &mut file)
            .await
            .map_err(Error::Io)?;
        Ok(())
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
        if let Err(e) = client
            .download_nupkg(&lower_id, &normalized, &temp_path)
            .await
        {
            tracing::warn!(%feed, id = %lower_id, version = %normalized, error = %e, "mirror download failed");
            let _ = tokio::fs::remove_file(&temp_path).await;
            continue;
        }

        let summary = match summarize(&temp_path).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(%feed, id = %lower_id, version = %normalized, error = %e, "mirror hash failed");
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

/// Re-hash a downloaded file to produce the [`StreamSummary`](streaming::StreamSummary)
/// the indexing pipeline needs.
async fn summarize(path: &PathBuf) -> Result<streaming::StreamSummary> {
    use tokio_util::io::ReaderStream;
    let file = tokio::fs::File::open(path).await?;
    let stream = ReaderStream::new(file);
    let mut sink = tokio::io::sink();
    streaming::stream_to_writer(stream, &mut sink)
        .await
        .map_err(Error::Io)
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
