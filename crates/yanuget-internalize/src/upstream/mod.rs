//! The upstream feed packages come from: the Chocolatey Community Repository
//! over NuGet v2 (OData), or any v3 feed through its service index.
//!
//! Downloads land in a local cache keyed by hash. Each is checked against the
//! hash the feed states for it, and its manifest against the id and version
//! asked for, before it enters the cache; a feed that states no hash (most v3
//! feeds) is cached under the package's identity and reported as unverified.

pub mod http;
pub mod v2;
pub mod v3;

use std::io::Read as _;
use std::path::{Path, PathBuf};

use base64::Engine;
use futures_util::StreamExt;
use reqwest::{StatusCode, Url};
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt;
use tokio::sync::OnceCell;
use yanuget_pkg::NuGetVersion;

use crate::config::UpstreamConfig;
use http::{redact, HttpClient};

/// The largest metadata document taken from the upstream: a page of a v2
/// feed, a registration page, a service index.
pub const MAX_DOCUMENT_BYTES: u64 = 32 * 1024 * 1024;

/// The most pages one version list may take.
pub const MAX_PAGES: usize = 200;

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    /// The upstream could not be reached, or the transfer broke off.
    #[error("{0}")]
    Http(String),
    /// The address policy or a downgrade stopped the request.
    #[error("{0}")]
    Refused(String),
    /// The upstream answered something unusable.
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Status(String),
    #[error("{0}")]
    NotFound(String),
    /// A download is not what the feed said it would be.
    #[error("{0}")]
    Integrity(String),
    #[error("cache: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HashAlgorithm {
    Sha256,
    Sha512,
}

impl HashAlgorithm {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "SHA256" | "SHA-256" => Some(HashAlgorithm::Sha256),
            "SHA512" | "SHA-512" => Some(HashAlgorithm::Sha512),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HashAlgorithm::Sha256 => "sha256",
            HashAlgorithm::Sha512 => "sha512",
        }
    }

    fn len(self) -> usize {
        match self {
            HashAlgorithm::Sha256 => 32,
            HashAlgorithm::Sha512 => 64,
        }
    }
}

/// The hash a feed states for a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageHash {
    pub algorithm: HashAlgorithm,
    pub digest: Vec<u8>,
}

impl PackageHash {
    /// From the base64 feeds use. A digest of the wrong length is no hash.
    pub fn from_base64(algorithm: HashAlgorithm, b64: &str) -> Option<Self> {
        let digest = base64::engine::general_purpose::STANDARD
            .decode(b64.trim())
            .ok()?;
        (digest.len() == algorithm.len()).then_some(PackageHash { algorithm, digest })
    }

    pub fn hex(&self) -> String {
        hex::encode(&self.digest)
    }
}

/// A dependency as the feed lists it; the range is NuGet interval notation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyRef {
    pub id: String,
    pub range: Option<String>,
}

/// One version of a package, as the feed describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageInfo {
    /// The id as the feed spells it.
    pub id: String,
    pub version: NuGetVersion,
    pub listed: bool,
    pub hash: Option<PackageHash>,
    pub size: Option<u64>,
    pub dependencies: Vec<DependencyRef>,
    pub download_url: Url,
}

/// A package in the cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPackage {
    pub path: PathBuf,
    pub size: u64,
    /// Hex SHA-512 of the file, the hash NuGet itself uses.
    pub sha512: String,
    /// Whether it matched a hash the feed stated.
    pub verified: bool,
}

enum Protocol {
    V2 { base: Url },
    V3 { index: Url },
}

/// A client for one upstream feed.
pub struct Upstream {
    http: HttpClient,
    protocol: Protocol,
    /// A v3 feed's resources, read from its service index once.
    v3_resources: OnceCell<v3::Resources>,
    source: String,
    cache: PathBuf,
    max_package_bytes: u64,
}

impl std::fmt::Debug for Upstream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upstream")
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

impl Upstream {
    /// A client for `config.url`: v3 when it names a `.json` service index,
    /// v2 otherwise. Packages are cached under `cache_dir/packages`.
    pub fn new(config: &UpstreamConfig, cache_dir: &Path) -> Result<Self, UpstreamError> {
        let url = Url::parse(&config.url)
            .map_err(|e| UpstreamError::Invalid(format!("upstream.url: {e}")))?;
        let protocol = if url.path().ends_with(".json") {
            Protocol::V3 { index: url }
        } else {
            Protocol::V2 { base: url }
        };
        Ok(Upstream {
            http: HttpClient::new(config)?,
            protocol,
            v3_resources: OnceCell::new(),
            source: config.url.clone(),
            cache: cache_dir.join("packages"),
            max_package_bytes: config.max_package_bytes,
        })
    }

    pub fn http(&self) -> &HttpClient {
        &self.http
    }

    /// The HTTP client, to tune its retries in tests.
    pub fn http_mut(&mut self) -> &mut HttpClient {
        &mut self.http
    }

    /// The feed's URL, for the report.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the feed answers: a v2 feed its service document, a v3 feed
    /// its service index with the resources the client needs.
    pub async fn probe(&self) -> Result<(), UpstreamError> {
        match &self.protocol {
            Protocol::V2 { base } => match self.document(base, "service document").await? {
                Some(_) => Ok(()),
                None => Err(UpstreamError::NotFound(format!(
                    "upstream {} answers 404",
                    redact(base)
                ))),
            },
            Protocol::V3 { .. } => self.v3_resources().await.map(|_| ()),
        }
    }

    async fn v3_resources(&self) -> Result<&v3::Resources, UpstreamError> {
        let Protocol::V3 { index } = &self.protocol else {
            unreachable!("only called for a v3 feed")
        };
        self.v3_resources
            .get_or_try_init(|| async {
                let doc = self.json(index, "service index").await?.ok_or_else(|| {
                    UpstreamError::NotFound(format!(
                        "upstream service index {} not found",
                        redact(index)
                    ))
                })?;
                v3::resources(&doc, index)
            })
            .await
    }

    /// Every version of `id` the feed has, oldest first. Unknown ids give an
    /// empty list.
    pub async fn versions(&self, id: &str) -> Result<Vec<PackageInfo>, UpstreamError> {
        let mut all = match &self.protocol {
            Protocol::V2 { base } => self.v2_versions(base, id).await?,
            Protocol::V3 { .. } => {
                let resources = self.v3_resources().await?;
                self.v3_versions(resources, id).await?
            }
        };
        all.sort_by(|a, b| a.version.cmp(&b.version));
        all.dedup_by(|a, b| a.version == b.version);
        Ok(all)
    }

    async fn v2_versions(&self, base: &Url, id: &str) -> Result<Vec<PackageInfo>, UpstreamError> {
        let mut out = Vec::new();
        let mut next = Some(v2::find_url(base, id)?);
        let mut pages = 0;
        while let Some(url) = next.take() {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(UpstreamError::Invalid(format!(
                    "upstream lists {id} over more than {MAX_PAGES} pages"
                )));
            }
            let Some(body) = self.document(&url, "version list").await? else {
                break;
            };
            let xml = String::from_utf8(body)
                .map_err(|_| UpstreamError::Invalid("upstream v2 feed is not UTF-8".into()))?;
            let (entries, more) = v2::parse_page(&xml, base, id)?;
            out.extend(entries);
            next = more;
        }
        Ok(out)
    }

    async fn v3_versions(
        &self,
        resources: &v3::Resources,
        id: &str,
    ) -> Result<Vec<PackageInfo>, UpstreamError> {
        let flat = resources.flat_container.as_ref();
        let Some(registration) = &resources.registration else {
            let flat = flat.expect("a service index without either is refused");
            let url = v3::flat_versions_url(flat, id)?;
            return match self.json(&url, "version list").await? {
                Some(doc) => v3::flat_versions(&doc, id, flat),
                None => Ok(Vec::new()),
            };
        };
        let url = v3::registration_url(registration, id)?;
        let Some(index) = self.json(&url, "registration").await? else {
            return Ok(Vec::new());
        };
        let pages = v3::pages(index, &url)?;
        if pages.len() > MAX_PAGES {
            return Err(UpstreamError::Invalid(format!(
                "upstream lists {id} over more than {MAX_PAGES} pages"
            )));
        }
        let mut out = Vec::new();
        for page in pages {
            let (leaves, base) = match page {
                v3::Page::Inline(leaves) => (leaves, url.clone()),
                v3::Page::Remote(page_url) => {
                    let doc = self
                        .json(&page_url, "registration page")
                        .await?
                        .ok_or_else(|| {
                            UpstreamError::Invalid(format!(
                                "upstream registration page {} not found",
                                redact(&page_url)
                            ))
                        })?;
                    (v3::page_leaves(doc)?, page_url)
                }
            };
            for l in &leaves {
                if let Some(info) = v3::leaf(l, id, &base, flat)? {
                    out.push(info);
                }
            }
        }
        Ok(out)
    }

    /// What `info` depends on. A v2 feed and a v3 registration list it with
    /// the version; a v3 feed with only a flat container does not, so its
    /// manifest is read from there.
    pub async fn dependencies(
        &self,
        info: &PackageInfo,
    ) -> Result<Vec<DependencyRef>, UpstreamError> {
        let flat = match &self.protocol {
            Protocol::V2 { .. } => return Ok(info.dependencies.clone()),
            Protocol::V3 { .. } => match self.v3_resources().await? {
                v3::Resources {
                    registration: None,
                    flat_container: Some(flat),
                } => flat,
                _ => return Ok(info.dependencies.clone()),
            },
        };
        let url = v3::flat_nuspec_url(flat, &info.id, &info.version.normalized())?;
        let what = format!("manifest of {} {}", info.id, info.version.normalized());
        let body = self
            .document(&url, &what)
            .await?
            .ok_or_else(|| UpstreamError::NotFound(format!("upstream {what}: not found")))?;
        let xml = String::from_utf8(body)
            .map_err(|_| UpstreamError::Invalid(format!("upstream {what} is not UTF-8")))?;
        let nuspec = yanuget_pkg::nuspec::parse_nuspec(&xml)
            .map_err(|e| UpstreamError::Invalid(format!("upstream {what}: {e}")))?;
        let mut out: Vec<DependencyRef> = Vec::new();
        for dep in nuspec
            .dependency_groups
            .into_iter()
            .flat_map(|g| g.dependencies)
        {
            let dep = DependencyRef {
                id: dep.id,
                range: dep.version_range.filter(|r| !r.trim().is_empty()),
            };
            if !out.contains(&dep) {
                out.push(dep);
            }
        }
        Ok(out)
    }

    /// The version to take of `id`: `version` when given, else the newest
    /// listed stable version, as `choco install` picks it.
    pub async fn resolve(
        &self,
        id: &str,
        version: Option<&NuGetVersion>,
    ) -> Result<PackageInfo, UpstreamError> {
        let versions = self.versions(id).await?;
        if versions.is_empty() {
            return Err(UpstreamError::NotFound(format!(
                "{id} is not on the upstream {}",
                self.source
            )));
        }
        let found = match version {
            Some(v) => versions.into_iter().find(|i| &i.version == v),
            None => versions
                .into_iter()
                .rev()
                .find(|i| i.listed && !i.version.is_prerelease()),
        };
        found.ok_or_else(|| {
            UpstreamError::NotFound(match version {
                Some(v) => format!("{id} {} is not on the upstream", v.normalized()),
                None => {
                    format!("{id} has no listed stable version upstream; name one as {id}@version")
                }
            })
        })
    }

    /// The package, from the cache or downloaded into it.
    pub async fn fetch(&self, info: &PackageInfo) -> Result<CachedPackage, UpstreamError> {
        tokio::fs::create_dir_all(&self.cache).await?;
        let final_path = self.cache.join(self.cache_name(info));
        if let Ok(meta) = tokio::fs::metadata(&final_path).await {
            let (sha512, other) =
                hash_file(&final_path, info.hash.as_ref().map(|h| h.algorithm)).await?;
            match &info.hash {
                Some(h) if digest_for(h.algorithm, &sha512, &other) != h.digest => {
                    tracing::warn!(path = %final_path.display(), "cached package does not match its hash; fetching it again");
                    tokio::fs::remove_file(&final_path).await?;
                }
                _ => {
                    return Ok(CachedPackage {
                        path: final_path,
                        size: meta.len(),
                        sha512: hex::encode(sha512),
                        verified: info.hash.is_some(),
                    })
                }
            }
        }

        // Unique per process and per fetch, so two fetches of one package,
        // here or in another process sharing the cache, never write one file.
        static PARTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let part = self.cache.join(format!(
            ".{}.{}-{}.part",
            self.cache_name(info),
            std::process::id(),
            PARTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let result = self.download(info, &part).await;
        let downloaded = match result {
            Ok(d) => d,
            Err(e) => {
                let _ = tokio::fs::remove_file(&part).await;
                return Err(e);
            }
        };
        if let Err(e) = check_identity(&part, info).await {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(e);
        }
        tokio::fs::rename(&part, &final_path).await?;
        Ok(CachedPackage {
            path: final_path,
            ..downloaded
        })
    }

    /// The cache's file name: the feed's hash when it states one, else a
    /// hash of where the package came from and what it is.
    fn cache_name(&self, info: &PackageInfo) -> String {
        match &info.hash {
            Some(h) => format!("{}-{}.nupkg", h.algorithm.as_str(), h.hex()),
            None => {
                let mut key = Sha256::new();
                key.update(self.source.as_bytes());
                key.update(b"\n");
                key.update(info.id.to_ascii_lowercase().as_bytes());
                key.update(b"\n");
                key.update(info.version.normalized().to_ascii_lowercase().as_bytes());
                format!("unverified-{}.nupkg", hex::encode(key.finalize()))
            }
        }
    }

    async fn download(
        &self,
        info: &PackageInfo,
        part: &Path,
    ) -> Result<CachedPackage, UpstreamError> {
        let what = format!("package {} {}", info.id, info.version.normalized());
        let fetched = self.http.get(&info.download_url, &what).await?;
        let resp = ok_status(fetched.response, &what)?;
        let limit = self.max_package_bytes;
        if resp.content_length().is_some_and(|n| n > limit) {
            return Err(UpstreamError::Integrity(format!(
                "{what} is larger than upstream.max_package_bytes ({limit})"
            )));
        }
        let mut file = tokio::fs::File::create(part).await?;
        let mut sha512 = Sha512::new();
        let mut sha256 = Sha256::new();
        let mut size: u64 = 0;
        let mut body = resp.bytes_stream();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| UpstreamError::Http(format!("{what}: {e}")))?;
            size += chunk.len() as u64;
            if size > limit {
                return Err(UpstreamError::Integrity(format!(
                    "{what} is larger than upstream.max_package_bytes ({limit})"
                )));
            }
            sha512.update(&chunk);
            sha256.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        let sha512: [u8; 64] = sha512.finalize().into();
        let sha256: [u8; 32] = sha256.finalize().into();
        if let Some(expected) = info.size {
            if expected != size {
                return Err(UpstreamError::Integrity(format!(
                    "{what} is {size} bytes, but the feed says {expected}"
                )));
            }
        }
        if let Some(h) = &info.hash {
            let got = digest_for(h.algorithm, &sha512, &sha256);
            if got != h.digest {
                return Err(UpstreamError::Integrity(format!(
                    "{what} does not match the feed's {} hash",
                    h.algorithm.as_str()
                )));
            }
        }
        Ok(CachedPackage {
            path: part.to_path_buf(),
            size,
            sha512: hex::encode(sha512),
            verified: info.hash.is_some(),
        })
    }

    /// A metadata document; `None` is a `404`. Gzip-encoded bodies (Azure
    /// blob storage serves registrations that way) are decoded, under the
    /// same cap.
    async fn document(&self, url: &Url, what: &str) -> Result<Option<Vec<u8>>, UpstreamError> {
        let fetched = self.http.get(url, what).await?;
        let resp = fetched.response;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = ok_status(resp, &format!("{what} {}", redact(url)))?;
        let gzip = resp
            .headers()
            .get(reqwest::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("gzip"));
        let too_large = || {
            UpstreamError::Invalid(format!(
                "upstream {what} {} is larger than {MAX_DOCUMENT_BYTES} bytes",
                redact(url)
            ))
        };
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_DOCUMENT_BYTES)
        {
            return Err(too_large());
        }
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        let read = async {
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|e| UpstreamError::Http(format!("upstream {what}: {e}")))?;
                if body.len() as u64 + chunk.len() as u64 > MAX_DOCUMENT_BYTES {
                    return Err(too_large());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(())
        };
        tokio::time::timeout(self.http.timeout(), read)
            .await
            .map_err(|_| {
                UpstreamError::Http(format!("upstream {what} {} timed out", redact(url)))
            })??;
        if gzip {
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(body.as_slice())
                .take(MAX_DOCUMENT_BYTES + 1)
                .read_to_end(&mut decoded)
                .map_err(|e| UpstreamError::Invalid(format!("upstream {what}: bad gzip: {e}")))?;
            if decoded.len() as u64 > MAX_DOCUMENT_BYTES {
                return Err(too_large());
            }
            body = decoded;
        }
        Ok(Some(body))
    }

    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        url: &Url,
        what: &str,
    ) -> Result<Option<T>, UpstreamError> {
        match self.document(url, what).await? {
            Some(body) => serde_json::from_slice(&body).map(Some).map_err(|e| {
                UpstreamError::Invalid(format!("upstream {what} {} is not JSON: {e}", redact(url)))
            }),
            None => Ok(None),
        }
    }
}

fn ok_status(resp: reqwest::Response, what: &str) -> Result<reqwest::Response, UpstreamError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    if status == StatusCode::NOT_FOUND {
        return Err(UpstreamError::NotFound(format!(
            "upstream {what}: not found"
        )));
    }
    let hint = match status.as_u16() {
        401 | 403 => {
            "; check the upstream credentials (they are sent to the upstream's own origin only)"
        }
        _ => "",
    };
    Err(UpstreamError::Status(format!(
        "upstream {what} answered {status}{hint}"
    )))
}

fn digest_for(algorithm: HashAlgorithm, sha512: &[u8], sha256: &[u8]) -> Vec<u8> {
    match algorithm {
        HashAlgorithm::Sha512 => sha512.to_vec(),
        HashAlgorithm::Sha256 => sha256.to_vec(),
    }
}

/// SHA-512 of a file, and SHA-256 too when `also` asks for it.
async fn hash_file(
    path: &Path,
    also: Option<HashAlgorithm>,
) -> Result<([u8; 64], Vec<u8>), UpstreamError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut file = std::fs::File::open(&path)?;
        let mut sha512 = Sha512::new();
        let mut sha256 = Sha256::new();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            sha512.update(&buf[..n]);
            if also == Some(HashAlgorithm::Sha256) {
                sha256.update(&buf[..n]);
            }
        }
        Ok((sha512.finalize().into(), sha256.finalize().to_vec()))
    })
    .await
    .map_err(|e| UpstreamError::Io(std::io::Error::other(e.to_string())))?
}

/// The package's manifest must name the id and version that were asked for:
/// a feed that serves another package under a URL is caught here, hash or no
/// hash.
async fn check_identity(path: &Path, info: &PackageInfo) -> Result<(), UpstreamError> {
    let what = format!("package {} {}", info.id, info.version.normalized());
    let bad = |m: String| UpstreamError::Integrity(format!("{what}: {m}"));
    let archive = yanuget_pkg::nupkg::read_archive(path)
        .await
        .map_err(|e| bad(e.to_string()))?;
    let nuspec =
        yanuget_pkg::nuspec::parse_nuspec(&archive.nuspec_xml).map_err(|e| bad(e.to_string()))?;
    let version = NuGetVersion::parse(&nuspec.version)
        .map_err(|e| bad(format!("its manifest's version does not parse: {e}")))?;
    if !nuspec.id.eq_ignore_ascii_case(&info.id) || version != info.version {
        return Err(bad(format!(
            "its manifest says {} {}",
            nuspec.id,
            version.normalized()
        )));
    }
    Ok(())
}
