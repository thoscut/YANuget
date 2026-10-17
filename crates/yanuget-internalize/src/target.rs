//! The feed the tool pushes to, as far as a plan needs it: which versions it
//! already serves.
//!
//! The tool holds a push key and nothing more, so a read-gated feed cannot
//! tell; nor are pending versions listed. A push of a version the feed holds
//! answers `409`, which a run takes as "already internalized" (`INT-20`).

use std::time::Duration;

use reqwest::{StatusCode, Url};
use serde_json::Value;
use tokio::sync::OnceCell;
use yanuget_pkg::NuGetVersion;

use crate::config::TargetConfig;

/// The largest document taken from the target feed.
const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct TargetError(String);

/// The target feed's package listing.
#[derive(Debug)]
pub struct Target {
    client: reqwest::Client,
    index: Url,
    /// The flat container, or `None` when the feed is read-gated.
    flat: OnceCell<Option<Url>>,
}

impl Target {
    /// A client for `config.url`, or `None` when no target is set.
    pub fn new(config: &TargetConfig) -> Result<Option<Self>, TargetError> {
        let Some(url) = &config.url else {
            return Ok(None);
        };
        let index = Url::parse(url)
            .and_then(|u| u.join("v3/index.json"))
            .map_err(|e| TargetError(format!("target.url: {e}")))?;
        let client = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .timeout(Duration::from_secs(config.timeout_secs.min(60)))
            .build()
            .map_err(|e| TargetError(format!("cannot build an HTTP client: {e}")))?;
        Ok(Some(Target {
            client,
            index,
            flat: OnceCell::new(),
        }))
    }

    /// The versions of `id` the feed serves, unlisted ones included; `None`
    /// when it does not say, because it is read-gated.
    pub async fn versions(&self, id: &str) -> Result<Option<Vec<NuGetVersion>>, TargetError> {
        let Some(flat) = self
            .flat
            .get_or_try_init(|| async {
                let Some(index) = self.json(&self.index, "service index").await? else {
                    return Ok::<_, TargetError>(None);
                };
                let flat = index["resources"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|r| r["@type"].as_str() == Some("PackageBaseAddress/3.0.0"))
                    .and_then(|r| r["@id"].as_str())
                    .and_then(|id| self.index.join(id).ok())
                    .ok_or_else(|| {
                        TargetError(format!(
                            "target feed {} lists no flat container",
                            self.index
                        ))
                    })?;
                Ok(Some(flat))
            })
            .await?
        else {
            return Ok(None);
        };
        let mut url = flat.clone();
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        let url = url
            .join(&format!("{}/index.json", id.to_ascii_lowercase()))
            .map_err(|e| TargetError(format!("target URL: {e}")))?;
        let Some(doc) = self.json(&url, "version list").await? else {
            return Ok(None);
        };
        Ok(Some(
            doc["versions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(|v| NuGetVersion::parse(v).ok())
                .collect(),
        ))
    }

    /// A JSON document: `None` on `401`/`403`, an empty object on `404`.
    async fn json(&self, url: &Url, what: &str) -> Result<Option<Value>, TargetError> {
        let fail = |m: String| TargetError(format!("target feed {what} {url}: {m}"));
        let mut resp = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| fail(e.to_string()))?;
        match resp.status() {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => return Ok(None),
            StatusCode::NOT_FOUND => return Ok(Some(Value::Object(Default::default()))),
            s if !s.is_success() => return Err(fail(format!("answers {s}"))),
            _ => {}
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| fail(e.to_string()))? {
            if body.len() + chunk.len() > MAX_DOCUMENT_BYTES {
                return Err(fail(format!("larger than {MAX_DOCUMENT_BYTES} bytes")));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|e| fail(format!("not JSON: {e}")))
    }
}
