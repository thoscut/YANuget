//! The downloader: fetches each file a captured call names into
//! `cache_dir/files`, under its SHA-256.
//!
//! A file is streamed to disk while it is hashed with SHA-256 and with the
//! algorithm the script declares. A declared checksum that does not match
//! fails the file; a match on SHA-256 or SHA-512 verifies it; MD5, SHA-1 or no
//! checksum at all leaves it trust-on-first-use, pinned by its SHA-256 from
//! then on. The URLs come from untrusted scripts, so the mirror's address
//! policy applies to each of them and every redirect: loopback, private and
//! link-local addresses are refused unless `downloads.allow_private_hosts`
//! names the host. Without that, a script could make the tool fetch from an
//! internal host and publish the answer into the feed. Transfers that break
//! off are resumed where the host allows it and restarted where it does not.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use md5::Md5;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT_RANGES, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE,
};
use reqwest::{StatusCode, Url};
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt;
use yanuget_pkg::report::{Checksum as ReportChecksum, ChecksumStatus};

use crate::capture::{Checksum, ChecksumType};
use crate::config::DownloadsConfig;
use crate::upstream::http::{redact, HttpClient, Options};
use crate::upstream::UpstreamError;

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// Refused by the address policy, unreachable, or broken off for good.
    #[error(transparent)]
    Http(#[from] UpstreamError),
    #[error("{url}: the file's {algorithm} is {actual}, and the script declares {declared}")]
    ChecksumMismatch {
        url: String,
        algorithm: &'static str,
        declared: String,
        actual: String,
    },
    #[error("{0}")]
    TooLarge(String),
    /// The host answered something other than the file.
    #[error("{0}")]
    Status(String),
    #[error("download cache: {0}")]
    Io(#[from] std::io::Error),
}

/// A file in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Downloaded {
    pub path: PathBuf,
    pub size: u64,
    /// Lower-case hex.
    pub sha256: String,
    /// What the script's checksum vouched for, as the report says it.
    pub checksum: ReportChecksum,
    /// Where the file came from in the end, after redirects; `None` when it
    /// was in the cache already.
    pub final_url: Option<Url>,
    /// The host's `Content-Disposition` and `Content-Type`, for naming the
    /// file.
    pub content_disposition: Option<String>,
    pub content_type: Option<String>,
}

/// Fetches files under the download policy.
#[derive(Debug)]
pub struct Downloader {
    http: HttpClient,
    dir: PathBuf,
    max_bytes: u64,
    resume_attempts: u32,
}

/// Tells partial files of one process apart.
static PARTIAL: AtomicU64 = AtomicU64::new(0);

impl Downloader {
    /// Files go to `cache_dir/files`.
    pub fn new(config: &DownloadsConfig, cache_dir: &Path) -> Result<Self, DownloadError> {
        let http = HttpClient::with_options(Options {
            label: "download",
            origin: None,
            credentials: HeaderMap::new(),
            allow_private: false,
            allowed_private_hosts: config.allow_private_hosts.clone(),
            proxy: config.proxy.clone(),
            timeout: Duration::from_secs(config.timeout_secs.max(1)),
            concurrency: config.concurrency,
            hint: "add the host to downloads.allow_private_hosts to permit it",
            watch_health: false,
        })?;
        Ok(Downloader {
            http,
            dir: cache_dir.join("files"),
            max_bytes: config.max_file_bytes,
            resume_attempts: config.resume_attempts,
        })
    }

    /// The HTTP client, to tune its retries in tests.
    pub fn http_mut(&mut self) -> &mut HttpClient {
        &mut self.http
    }

    /// The file at `url`, from the cache when a declared SHA-256 names one
    /// there, else downloaded.
    pub async fn fetch(
        &self,
        url: &Url,
        declared: Option<&Checksum>,
    ) -> Result<Downloaded, DownloadError> {
        tokio::fs::create_dir_all(&self.dir).await?;
        if let Some(found) = self.cached(declared).await? {
            return Ok(found);
        }
        let partial = self.dir.join(format!(
            "partial-{}-{}",
            std::process::id(),
            PARTIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let result = self.download(url, declared, &partial).await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&partial).await;
        }
        result
    }

    /// A file a declared SHA-256 names, if the cache holds it and it still
    /// hashes to that.
    async fn cached(
        &self,
        declared: Option<&Checksum>,
    ) -> Result<Option<Downloaded>, DownloadError> {
        let Some(sum) = declared.filter(|c| c.kind == ChecksumType::Sha256) else {
            return Ok(None);
        };
        let path = self.dir.join(format!("sha256-{}", sum.hex));
        let Ok(bytes) = tokio::fs::metadata(&path).await.map(|m| m.len()) else {
            return Ok(None);
        };
        let mut file = tokio::fs::File::open(&path).await?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = tokio::io::AsyncReadExt::read(&mut file, &mut buf).await?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        if hex::encode(hasher.finalize()) != sum.hex {
            tracing::warn!(path = %path.display(), "a cached file no longer matches its hash; fetching it again");
            tokio::fs::remove_file(&path).await?;
            return Ok(None);
        }
        Ok(Some(Downloaded {
            path,
            size: bytes,
            sha256: sum.hex.clone(),
            checksum: report_checksum(Some(sum)),
            final_url: None,
            content_disposition: None,
            content_type: None,
        }))
    }

    async fn download(
        &self,
        url: &Url,
        declared: Option<&Checksum>,
        partial: &Path,
    ) -> Result<Downloaded, DownloadError> {
        let shown = redact(url);
        let too_large = || {
            DownloadError::TooLarge(format!(
                "{shown} is larger than downloads.max_file_bytes ({} bytes)",
                self.max_bytes
            ))
        };
        let mut file = tokio::fs::File::create(partial).await?;
        let mut hashes = Hashes::new(declared.map(|c| c.kind));
        let mut written: u64 = 0;
        // What a resume needs: the host takes ranges, and a validator proves
        // the rest belongs to the same file.
        let mut validator: Option<HeaderValue> = None;
        let mut final_url = url.clone();
        let mut content_disposition = None;
        let mut content_type = None;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let mut headers = HeaderMap::new();
            if written > 0 {
                if let Some(v) = &validator {
                    headers.insert(
                        RANGE,
                        HeaderValue::from_str(&format!("bytes={written}-"))
                            .expect("a range is a header value"),
                    );
                    headers.insert(IF_RANGE, v.clone());
                }
            }
            let fetched = self.http.get_with(url, "file", &headers).await?;
            let resp = fetched.response;
            let status = resp.status();
            let resumed = written > 0
                && status == StatusCode::PARTIAL_CONTENT
                && starts_at(resp.headers(), written);
            if !resumed {
                if status != StatusCode::OK {
                    return Err(DownloadError::Status(format!("{shown} answered {status}")));
                }
                if written > 0 {
                    // The host sent the whole file again: start over.
                    tracing::info!(url = %shown, "restarting a download the host cannot resume");
                    file.set_len(0).await?;
                    file = tokio::fs::OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(partial)
                        .await?;
                    hashes = Hashes::new(declared.map(|c| c.kind));
                    written = 0;
                }
                if resp.content_length().is_some_and(|n| n > self.max_bytes) {
                    return Err(too_large());
                }
                let h = resp.headers();
                let ranges = h
                    .get(ACCEPT_RANGES)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.eq_ignore_ascii_case("bytes"));
                // A strong ETag, else Last-Modified, as `If-Range` takes them.
                validator = ranges
                    .then(|| {
                        h.get(ETAG)
                            .filter(|e| !e.as_bytes().starts_with(b"W/"))
                            .or_else(|| h.get(LAST_MODIFIED))
                            .cloned()
                    })
                    .flatten();
                final_url = resp.url().clone();
                content_disposition = header(h, reqwest::header::CONTENT_DISPOSITION);
                content_type = header(h, reqwest::header::CONTENT_TYPE);
            }
            let mut resp = resp;
            let broke = loop {
                match resp.chunk().await {
                    Ok(Some(chunk)) => {
                        if written + chunk.len() as u64 > self.max_bytes {
                            return Err(too_large());
                        }
                        hashes.update(&chunk);
                        file.write_all(&chunk).await?;
                        written += chunk.len() as u64;
                    }
                    Ok(None) => break None,
                    Err(e) => break Some(e),
                }
            };
            let Some(e) = broke else { break };
            if attempt > self.resume_attempts {
                return Err(UpstreamError::Http(format!(
                    "{shown} broke off after {written} bytes, {attempt} times: {e}"
                ))
                .into());
            }
            tracing::info!(url = %shown, written, error = %e, "a download broke off; resuming");
            tokio::time::sleep(self.http.backoff(attempt)).await;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        let (sha256, extra) = hashes.finish();
        if let Some(sum) = declared {
            let actual = extra.unwrap_or_else(|| sha256.clone());
            if actual != sum.hex {
                return Err(DownloadError::ChecksumMismatch {
                    url: shown,
                    algorithm: sum.kind.as_str(),
                    declared: sum.hex.clone(),
                    actual,
                });
            }
        }
        let path = self.dir.join(format!("sha256-{sha256}"));
        tokio::fs::rename(partial, &path).await?;
        Ok(Downloaded {
            path,
            size: written,
            sha256,
            checksum: report_checksum(declared),
            final_url: Some(final_url),
            content_disposition,
            content_type,
        })
    }
}

fn header(h: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Whether a `206`'s `Content-Range` starts where the file left off.
fn starts_at(h: &HeaderMap, offset: u64) -> bool {
    h.get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes "))
        .and_then(|v| v.split('-').next())
        .and_then(|start| start.trim().parse::<u64>().ok())
        == Some(offset)
}

/// The report's view of a declared checksum that matched.
fn report_checksum(declared: Option<&Checksum>) -> ReportChecksum {
    ReportChecksum {
        status: match declared {
            Some(c) if c.kind.is_strong() => ChecksumStatus::Verified,
            _ => ChecksumStatus::TrustOnFirstUse,
        },
        algorithm: declared.map(|c| c.kind.as_str().to_string()),
    }
}

/// SHA-256 always, and the declared algorithm when it is another.
struct Hashes {
    sha256: Sha256,
    extra: Extra,
}

enum Extra {
    None,
    Md5(Md5),
    Sha1(Sha1),
    Sha512(Sha512),
}

impl Hashes {
    fn new(declared: Option<ChecksumType>) -> Self {
        Hashes {
            sha256: Sha256::new(),
            extra: match declared {
                None | Some(ChecksumType::Sha256) => Extra::None,
                Some(ChecksumType::Md5) => Extra::Md5(Md5::new()),
                Some(ChecksumType::Sha1) => Extra::Sha1(Sha1::new()),
                Some(ChecksumType::Sha512) => Extra::Sha512(Sha512::new()),
            },
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.sha256.update(bytes);
        match &mut self.extra {
            Extra::None => {}
            Extra::Md5(h) => h.update(bytes),
            Extra::Sha1(h) => h.update(bytes),
            Extra::Sha512(h) => h.update(bytes),
        }
    }

    /// SHA-256 and the declared algorithm's digest, lower-case hex.
    fn finish(self) -> (String, Option<String>) {
        let extra = match self.extra {
            Extra::None => None,
            Extra::Md5(h) => Some(hex::encode(h.finalize())),
            Extra::Sha1(h) => Some(hex::encode(h.finalize())),
            Extra::Sha512(h) => Some(hex::encode(h.finalize())),
        };
        (hex::encode(self.sha256.finalize()), extra)
    }
}
