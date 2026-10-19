//! Publishing to the target feed (`INT-20`): rewrite the package with its
//! report, push it, attach its files, and check that the feed withholds it.
//!
//! The tool holds a push key and nothing more. It cannot see pending
//! versions, so a push is how it learns whether the feed has the version
//! already: `409` means it does, and the run takes the version as
//! internalized. Attaching is idempotent (the same file again is `200`), so
//! a run that stopped between the push and the last attach is completed by
//! the next one, as long as the version is still pending. Files are never
//! attached to a version the feed serves already, nor to one it cannot tell
//! about: an attach is not reviewed, and an approved version's files go to
//! clients at once.

use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::{Body, Method, StatusCode, Url};
use serde_json::Value;
use yanuget_pkg::report::{Report, REPORT_PATH};
use yanuget_pkg::rewrite::{self, Rewrite, Source};
use yanuget_pkg::NuGetVersion;

use crate::assess::Attachment;
use crate::config::TargetConfig;
use crate::target::Target;

/// The header a push key travels in.
const API_KEY_HEADER: &str = "X-NuGet-ApiKey";

/// The first server version that replaces a pending version
/// (`allow_overwrite = "pending-only"`).
const REPLACE_PENDING_SINCE: &str = "0.8.0";

/// How often a failed attach is tried in all. Attaching is idempotent, so
/// trying again is safe.
const ATTACH_ATTEMPTS: u32 = 3;

/// The most of an error answer that is read.
const MAX_ERROR_BYTES: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    /// The feed refused, or could not be reached.
    #[error("{0}")]
    Feed(String),
    #[error("cannot rewrite the package: {0}")]
    Rewrite(#[from] yanuget_pkg::Error),
    #[error("work directory: {0}")]
    Io(#[from] std::io::Error),
}

/// One package to publish.
#[derive(Debug, Clone)]
pub struct Release<'a> {
    /// The package as it came from upstream.
    pub nupkg: &'a Path,
    /// Its report, which goes inside it as `internalizer/report.json`.
    pub report: &'a Report,
    /// Entries to add or replace besides the report: the prelude and the
    /// install script that loads it (`INT-19`).
    pub entries: Vec<(String, Source)>,
    pub files: &'a [Attachment],
}

/// What the push found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// The feed took the package (or replaced a pending build with it).
    New,
    /// The feed holds the version already and kept it.
    AlreadyThere,
}

/// Whether the feed serves the version to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Not listed: pending approval, as an intake feed should hold it.
    Withheld,
    /// Listed: the feed does not gate, or the version was approved.
    Served,
    /// The feed is read-gated, and a push key cannot read it.
    Unknown,
}

/// How one attach went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attached {
    New,
    /// The version had this file already.
    Same,
}

/// What publishing one package did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub pushed: Pushed,
    pub visibility: Visibility,
    /// Each file attached, by name.
    pub attached: Vec<(String, Attached)>,
    /// Files the feed refused, with the reason; they fail the package.
    pub errors: Vec<String>,
    /// What a reviewer should know: the feed serves the version at once,
    /// files were left out.
    pub warnings: Vec<String>,
}

/// A client for the target feed's push and attach endpoints.
#[derive(Debug)]
pub struct Publisher {
    client: reqwest::Client,
    /// The feed's URL, ending in `/`.
    feed: Url,
    key: String,
    timeout: Duration,
    target: Target,
}

impl Publisher {
    /// A publisher for `config`, which must name a feed and a push key.
    pub fn new(config: &TargetConfig) -> Result<Self, PublishError> {
        let (Some(url), Some(key)) = (&config.url, &config.api_key) else {
            return Err(PublishError::Feed(
                "publishing needs target.url and target.api_key".into(),
            ));
        };
        let mut feed =
            Url::parse(url).map_err(|e| PublishError::Feed(format!("target.url: {e}")))?;
        if !feed.path().ends_with('/') {
            feed.set_path(&format!("{}/", feed.path()));
        }
        let client = reqwest::Client::builder()
            .user_agent(crate::USER_AGENT)
            .connect_timeout(Duration::from_secs(config.timeout_secs.min(60)))
            // A push key goes to the feed and nowhere else.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| PublishError::Feed(format!("cannot build an HTTP client: {e}")))?;
        let target = Target::new(config)
            .map_err(|e| PublishError::Feed(e.to_string()))?
            .expect("target.url is set");
        Ok(Publisher {
            client,
            feed,
            key: key.expose().to_string(),
            timeout: Duration::from_secs(config.timeout_secs),
            target,
        })
    }

    /// Check up front that the server can replace a pending version, where
    /// its `/health/status` says which version it runs. A server that does
    /// not say (monitoring off, a token, a proxy in front) passes; a refused
    /// replace names the version it needs then.
    pub async fn check_replace_pending(&self) -> Result<(), PublishError> {
        let Ok(url) = self.feed.join("/health/status") else {
            return Ok(());
        };
        let Ok(resp) = self
            .client
            .get(url)
            .timeout(self.timeout.min(Duration::from_secs(30)))
            .send()
            .await
        else {
            return Ok(());
        };
        if resp.status() != StatusCode::OK {
            return Ok(());
        }
        let Some(running) = read_capped(resp)
            .await
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["version"].as_str().map(str::to_string))
        else {
            return Ok(());
        };
        let (Ok(have), Ok(need)) = (
            NuGetVersion::parse(&running),
            NuGetVersion::parse(REPLACE_PENDING_SINCE),
        ) else {
            return Ok(());
        };
        if have < need {
            return Err(PublishError::Feed(format!(
                "--replace-pending needs YANuget {REPLACE_PENDING_SINCE} or later, and the target runs {running}"
            )));
        }
        Ok(())
    }

    /// Rewrite the package into `work_dir` with its report and entries, push
    /// it, and attach its files where that is safe.
    ///
    /// With `replace_pending`, a version the feed keeps is an error: the
    /// feed refused to replace it. Without, it is internalized already.
    pub async fn publish(
        &self,
        release: Release<'_>,
        work_dir: &Path,
        replace_pending: bool,
    ) -> Result<Published, PublishError> {
        let package = &release.report.package;
        tokio::fs::create_dir_all(work_dir).await?;
        let rewritten = rewritten_path(work_dir, &package.id, &package.version);
        let mut plan = Rewrite::new().put(REPORT_PATH, Source::Bytes(release.report.to_vec()));
        for (name, source) in release.entries {
            plan = plan.put(name, source);
        }
        rewrite::rewrite(release.nupkg, &rewritten, plan).await?;
        let pushed = self.push(&rewritten).await;
        let _ = tokio::fs::remove_file(&rewritten).await;
        let pushed = pushed?;
        if pushed == Pushed::AlreadyThere && replace_pending {
            return Err(PublishError::Feed(format!(
                "{} {} is in the feed already and was not replaced: it is approved, or the \
                 feed does not set allow_overwrite = \"pending-only\" (YANuget \
                 {REPLACE_PENDING_SINCE} or later)",
                package.id, package.version
            )));
        }

        let mut published = Published {
            pushed,
            visibility: self.visibility(&package.id, &package.version).await,
            attached: Vec::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
        };
        let attach = match (pushed, published.visibility) {
            (Pushed::New, Visibility::Served) => {
                published.warnings.push(
                    "the feed served the version at once: it does not require approval".into(),
                );
                true
            }
            (Pushed::New, _) | (Pushed::AlreadyThere, Visibility::Withheld) => true,
            (Pushed::AlreadyThere, Visibility::Served) => {
                if !release.files.is_empty() {
                    published.warnings.push(
                        "the feed serves the version already; nothing was attached to it".into(),
                    );
                }
                false
            }
            (Pushed::AlreadyThere, Visibility::Unknown) => {
                if !release.files.is_empty() {
                    published.warnings.push(
                        "the feed holds the version already and is read-gated, so the tool \
                         cannot tell whether it is approved; nothing was attached to it"
                            .into(),
                    );
                }
                false
            }
        };
        if attach {
            for file in release.files {
                match self.attach(&package.id, &package.version, file).await {
                    Ok(how) => published.attached.push((file.name.clone(), how)),
                    Err(e) => published.errors.push(e.to_string()),
                }
            }
        }
        Ok(published)
    }

    /// `PUT {feed}api/v2/package` with the package as the body.
    async fn push(&self, nupkg: &Path) -> Result<Pushed, PublishError> {
        let url = self.endpoint("api/v2/package")?;
        let size = tokio::fs::metadata(nupkg).await?.len();
        let resp = self
            .send(Method::PUT, url, nupkg, size, None)
            .await
            .map_err(|e| PublishError::Feed(format!("push: {e}")))?;
        match resp.status() {
            StatusCode::CREATED | StatusCode::OK | StatusCode::ACCEPTED => Ok(Pushed::New),
            StatusCode::CONFLICT => Ok(Pushed::AlreadyThere),
            status => Err(PublishError::Feed(format!(
                "push: {}",
                refusal(status, resp).await
            ))),
        }
    }

    /// `PUT {feed}api/v2/files/{id}/{version}/{name}`, tried again on a
    /// broken connection or a server error.
    async fn attach(
        &self,
        id: &str,
        version: &str,
        file: &Attachment,
    ) -> Result<Attached, PublishError> {
        let url = self.endpoint(&format!("api/v2/files/{id}/{version}/{}", file.name))?;
        let fail = |m: String| PublishError::Feed(format!("attach {}: {m}", file.name));
        let mut attempt = 0;
        loop {
            attempt += 1;
            let last = attempt >= ATTACH_ATTEMPTS;
            match self
                .send(
                    Method::PUT,
                    url.clone(),
                    &file.path,
                    file.size,
                    Some(&file.sha256),
                )
                .await
            {
                Ok(resp) => match resp.status() {
                    StatusCode::CREATED => return Ok(Attached::New),
                    StatusCode::OK => return Ok(Attached::Same),
                    s if (s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS) && !last => {
                        tracing::warn!(file = %file.name, status = %s, "attach failed; trying again");
                    }
                    s => return Err(fail(refusal(s, resp).await)),
                },
                Err(e) if !last => {
                    tracing::warn!(file = %file.name, error = %e, "attach failed; trying again");
                }
                Err(e) => return Err(fail(e.to_string())),
            }
            tokio::time::sleep(Duration::from_secs(1 << attempt)).await;
        }
    }

    /// One request with a file as its body. The time it may take grows by a
    /// second for every MiB, so a large file is not cut off for its size.
    async fn send(
        &self,
        method: Method,
        url: Url,
        path: &Path,
        size: u64,
        sha256: Option<&str>,
    ) -> Result<reqwest::Response, String> {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        let mut req = self
            .client
            .request(method, url)
            .header(API_KEY_HEADER, &self.key)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(reqwest::header::CONTENT_LENGTH, size)
            .timeout(self.timeout + Duration::from_secs(size / (1024 * 1024)))
            .body(file_body(file));
        if let Some(sha) = sha256 {
            req = req.header("x-checksum-sha256", sha);
        }
        req.send().await.map_err(|e| without_url(&e))
    }

    /// Whether the feed serves `id` `version`, from its flat container, which
    /// lists approved versions only.
    async fn visibility(&self, id: &str, version: &str) -> Visibility {
        let Ok(version) = NuGetVersion::parse(version) else {
            return Visibility::Unknown;
        };
        match self.target.versions(id).await {
            Ok(Some(versions)) if versions.contains(&version) => Visibility::Served,
            Ok(Some(_)) => Visibility::Withheld,
            Ok(None) => Visibility::Unknown,
            Err(e) => {
                tracing::warn!(%id, error = %e, "cannot tell whether the feed serves the version");
                Visibility::Unknown
            }
        }
    }

    fn endpoint(&self, path: &str) -> Result<Url, PublishError> {
        self.feed
            .join(path)
            .map_err(|e| PublishError::Feed(format!("target URL: {e}")))
    }
}

/// A file as a streamed request body.
fn file_body(file: tokio::fs::File) -> Body {
    let chunks = futures_util::stream::unfold(file, |mut file| async move {
        let mut buf = vec![0u8; 256 * 1024];
        match tokio::io::AsyncReadExt::read(&mut file, &mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(buf), file))
            }
            Err(e) => Some((Err(e), file)),
        }
    });
    Body::wrap_stream(chunks)
}

/// A refused request as the feed explains it: the status and its `error`.
async fn refusal(status: StatusCode, resp: reqwest::Response) -> String {
    let why = read_capped(resp)
        .await
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["error"].as_str().map(str::to_string));
    let hint = match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            " (check target.api_key: it must be the feed's push key)"
        }
        _ => "",
    };
    match why {
        Some(why) => format!("the feed answered {status}: {why}{hint}"),
        None => format!("the feed answered {status}{hint}"),
    }
}

async fn read_capped(mut resp: reqwest::Response) -> Result<Vec<u8>, reqwest::Error> {
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_ERROR_BYTES {
            body.truncate(MAX_ERROR_BYTES);
            break;
        }
    }
    Ok(body)
}

/// A transport error without the URL reqwest puts in it; the caller says
/// what it was doing.
fn without_url(e: &reqwest::Error) -> String {
    let mut source = std::error::Error::source(e);
    let mut detail = None;
    while let Some(s) = source {
        detail = Some(s.to_string());
        source = s.source();
    }
    match detail {
        Some(d) if e.is_timeout() => format!("timed out ({d})"),
        Some(d) => d,
        None if e.is_timeout() => "timed out".into(),
        None => e.to_string(),
    }
}

/// Where the rewritten package of `id` `version` waits for its push.
fn rewritten_path(work_dir: &Path, id: &str, version: &str) -> PathBuf {
    work_dir.join(format!("{}.{version}.nupkg", id.to_ascii_lowercase()))
}
