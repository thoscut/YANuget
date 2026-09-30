//! Files attached to package versions: disk images (`.wim`) and archives a
//! package's install script fetches at install time.
//!
//! * **Download** `GET|HEAD {feed}/files/{id}/{version}/{name}` — streamed,
//!   resumable (`Range`, `If-Range`), with a strong `ETag` (the SHA-256),
//!   `Last-Modified` and `Repr-Digest`, which is everything BITS and
//!   `Invoke-WebRequest -Resume` rely on. `…/index.json` lists a version's files.
//! * **Upload in one request** `PUT {feed}/api/v2/files/{id}/{version}/{name}`.
//! * **Resumable upload**: the tus 1.0.0 protocol (core, creation, expiration
//!   and termination) under `{feed}/api/v2/uploads`, so a multi-gigabyte
//!   upload that drops continues where it stopped.
//! * **Delete** `DELETE {feed}/api/v2/files/{id}/{version}/{name}`.
//!
//! The bytes are stored once, under their SHA-256, in the blob store; a file
//! is a named reference to a blob from one version. So nothing a client sends
//! ever becomes part of a path on the server, identical images uploaded for
//! several versions take the space of one, and a file goes wherever its
//! version goes — deleted with it, pruned with it, visible in every feed that
//! holds it.
//!
//! Uploading needs the feed's push key, and a push key must be configured: a
//! feed left open for package pushes does not accept multi-gigabyte files from
//! anyone who can reach it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use axum::extract::{Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use base64::Engine;
use chrono::Utc;
use futures::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::{content_length, files, parse_version, to_io_err, AppState};
use crate::database::{PackageFile, UploadSession};
use crate::error::{Error, Result};
use crate::storage::PackageContent;
use crate::streaming;
use crate::version::NuGetVersion;

const OCTET_STREAM: &str = "application/octet-stream";
const TUS_VERSION: &str = "1.0.0";
const TUS_EXTENSIONS: &str = "creation,expiration,termination";

// ---------------------------------------------------------------------------
// Download
// ---------------------------------------------------------------------------

pub(super) async fn download(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    Path((id, version, name)): Path<(String, String, String)>,
) -> Result<Response> {
    state.require_read(&headers)?;
    if !state.config.files.enabled {
        return Err(Error::PackageNotFound);
    }
    let v = parse_version(&version)?;
    // Only what a client may fetch here: a disabled or pending version's files
    // are withheld like its package.
    if !state.db.is_servable(state.feed(), &id, &v).await? {
        return Err(Error::PackageNotFound);
    }
    if name.eq_ignore_ascii_case("index.json") {
        return index(&state, &headers, &id, &v).await;
    }
    let file = state
        .db
        .get_file(&id, &v, &name)
        .await?
        .ok_or(Error::PackageNotFound)?;
    let PackageContent::LocalPath(path) = state.storage.get_blob(&file.sha256).await?;
    if files::counts_as_download(&method, &headers) {
        let _ = state.db.increment_file_downloads(&id, &v, &name).await;
    }
    let digest = hex::decode(&file.sha256)
        .map(|d| base64::engine::general_purpose::STANDARD.encode(d))
        .ok();
    let mut response = files::serve_local_file(
        path,
        &headers,
        OCTET_STREAM,
        files::FileMeta {
            download_name: Some(&file.name),
            etag: Some(&file.sha256),
            last_modified: Some(file.uploaded),
            sha256_base64: digest.as_deref(),
        },
    )
    .await?;
    // Whatever the bytes are, a browser only ever saves them: always
    // `application/octet-stream` and `attachment`, and no policy under which
    // a hosted `.html` or `.svg` could run anything from this origin.
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'"),
    );
    Ok(response)
}

/// `…/files/{id}/{version}/index.json`: the version's files, for scripts.
async fn index(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
    v: &NuGetVersion,
) -> Result<Response> {
    let urls = state.url_builder(headers);
    let list = state.db.files_for(id, v).await?;
    let files: Vec<serde_json::Value> = list
        .iter()
        .map(|f| {
            serde_json::json!({
                "name": f.name,
                "size": f.size,
                "sha256": f.sha256,
                "uploaded": f.uploaded.to_rfc3339(),
                "url": urls.file_download(&f.lower_id, &f.normalized_version, &f.name),
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "files": files })).into_response())
}

// ---------------------------------------------------------------------------
// Attaching and detaching (shared by every way in)
// ---------------------------------------------------------------------------

/// Uploading needs the files feature, a configured push key, and that key.
fn authorize_upload(state: &AppState, headers: &HeaderMap) -> Result<()> {
    if !state.config.files.enabled {
        return Err(Error::PackageNotFound);
    }
    if !state.feed.auth.is_enabled() {
        return Err(Error::Forbidden(
            "attached files need a push key: set api_key for this feed".into(),
        ));
    }
    if !state.feed.auth.check_headers(headers) {
        return Err(Error::Unauthorized);
    }
    Ok(())
}

/// The version a file is being attached to, which this feed must hold.
async fn version_in_feed(state: &AppState, id: &str, v: &NuGetVersion) -> Result<()> {
    if state.db.exists(state.feed(), id, v).await? {
        Ok(())
    } else {
        Err(Error::PackageNotFound)
    }
}

/// How an attach went.
pub(crate) enum Attached {
    /// A new file.
    New(PackageFile),
    /// The version already had exactly this file; nothing changed.
    Same(PackageFile),
}

/// Attach the finished file at `temp` (SHA-256 `sha256`, `size` bytes) to a
/// version under `name`, storing its bytes as a blob.
///
/// Under the version lock, like a push or a purge, so the version cannot be
/// deleted between the check that it exists and the row that references it.
/// A file of the same name with the same content is a no-op; with different
/// content it is refused: a file's URL is cached as immutable, so it must not
/// start serving different bytes.
pub(crate) async fn attach(
    state: &AppState,
    id: &str,
    v: &NuGetVersion,
    name: &str,
    temp: PathBuf,
    sha256: &str,
    size: u64,
) -> Result<Attached> {
    let target = Target {
        storage: state.storage.as_ref(),
        db: state.db.as_ref(),
        feed: state.feed(),
    };
    attach_to(&target, id, v, name, temp, sha256, size).await
}

/// Where a file is attached: a feed, and the store behind it.
pub(crate) struct Target<'a> {
    pub storage: &'a dyn crate::storage::PackageStorage,
    pub db: &'a dyn crate::database::PackageDatabase,
    pub feed: &'a str,
}

/// [`attach`] without the web layer, for the inbox importer. `temp` is
/// consumed either way: moved into the blob store, or removed.
pub(crate) async fn attach_to(
    target: &Target<'_>,
    id: &str,
    v: &NuGetVersion,
    name: &str,
    temp: PathBuf,
    sha256: &str,
    size: u64,
) -> Result<Attached> {
    let result = attach_locked(target, id, v, name, &temp, sha256, size).await;
    if !matches!(result, Ok(Attached::New(_))) {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}

async fn attach_locked(
    target: &Target<'_>,
    id: &str,
    v: &NuGetVersion,
    name: &str,
    temp: &std::path::Path,
    sha256: &str,
    size: u64,
) -> Result<Attached> {
    let Target { storage, db, feed } = *target;
    let _guard = crate::locks::lock_version(id, &v.normalized()).await;
    if !db.exists(feed, id, v).await? || !db.package_data_exists(id, v).await? {
        return Err(Error::PackageNotFound);
    }
    if let Some(existing) = db.get_file(id, v, name).await? {
        return if existing.sha256 == sha256 {
            Ok(Attached::Same(existing))
        } else {
            Err(Error::Conflict(format!(
                "{id} {} already has a different file named {name}; delete it first",
                v.normalized()
            )))
        };
    }
    // The blob may be shared with other versions, whose locks are not ours:
    // from finding it stored to the row that references it, no detach or
    // purge elsewhere may count it unused and delete it.
    let _blob = crate::locks::lock_blob(sha256).await;
    storage.store_blob(sha256, temp.to_path_buf()).await?;
    let file = PackageFile {
        lower_id: id.to_lowercase(),
        normalized_version: v.normalized(),
        name: name.to_string(),
        sha256: sha256.to_string(),
        size,
        uploaded: Utc::now(),
        downloads: 0,
    };
    if let Err(e) = db.add_file(&file).await {
        // Nothing references the blob if this was its first use.
        if db.blob_references(sha256).await.unwrap_or(1) == 0 {
            let _ = storage.delete_blob(sha256).await;
        }
        return Err(match e {
            Error::PackageAlreadyExists => {
                Error::Conflict(format!("{name} was attached by another upload just now"))
            }
            e => e,
        });
    }
    tracing::info!(%feed, %id, version = %v.normalized(), %name, size, %sha256, "attached file");
    Ok(Attached::New(file))
}

/// Detach a file from a version, deleting its blob when nothing else uses it.
pub(crate) async fn detach(state: &AppState, id: &str, v: &NuGetVersion, name: &str) -> Result<()> {
    let _guard = crate::locks::lock_version(id, &v.normalized()).await;
    version_in_feed(state, id, v).await?;
    let file = state
        .db
        .delete_file(id, v, name)
        .await?
        .ok_or(Error::PackageNotFound)?;
    // Counted and deleted as one step against attaches to other versions.
    let _blob = crate::locks::lock_blob(&file.sha256).await;
    if state.db.blob_references(&file.sha256).await? == 0 {
        state.storage.delete_blob(&file.sha256).await?;
    }
    tracing::info!(feed = %state.feed(), %id, version = %v.normalized(), %name, "detached file");
    Ok(())
}

/// The JSON a successful upload answers with.
fn attached_json(state: &AppState, headers: &HeaderMap, file: &PackageFile) -> serde_json::Value {
    let urls = state.url_builder(headers);
    serde_json::json!({
        "name": file.name,
        "size": file.size,
        "sha256": file.sha256,
        "url": urls.file_download(&file.lower_id, &file.normalized_version, &file.name),
    })
}

/// A `sha256` value from a client: 64 hex digits, any case.
fn parse_sha256(raw: &str) -> Result<String> {
    let hex = raw.trim().to_ascii_lowercase();
    if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(hex)
    } else {
        Err(Error::BadRequest(format!(
            "not a SHA-256 in hex: {:?}",
            raw.chars().take(80).collect::<String>()
        )))
    }
}

/// Refuse a declared size over the file limit before reading a byte of it.
fn check_size(state: &AppState, declared: Option<u64>) -> Result<()> {
    if let (Some(size), Some(limit)) = (declared, state.config.max_file_size_bytes()) {
        if size > limit {
            return Err(Error::PayloadTooLarge(format!(
                "{size} bytes is over the {limit}-byte limit for files"
            )));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Upload in one request
// ---------------------------------------------------------------------------

pub(super) async fn put(
    State(state): State<AppState>,
    Path((id, version, name)): Path<(String, String, String)>,
    request: Request,
) -> Result<Response> {
    let headers = request.headers().clone();
    authorize_upload(&state, &headers)?;
    let v = parse_version(&version)?;
    crate::validation::validate_file_name(&name, &state.config.files)?;
    version_in_feed(&state, &id, &v).await?;
    let expected = headers
        .get("x-checksum-sha256")
        .and_then(|h| h.to_str().ok())
        .map(parse_sha256)
        .transpose()?;
    let declared = content_length(&headers);
    check_size(&state, declared)?;
    state.ensure_disk_space(declared)?;

    let (temp, mut file) = state.create_temp().await?;
    let stream = Box::pin(
        request
            .into_body()
            .into_data_stream()
            .map(|r| r.map_err(to_io_err)),
    );
    let streamed = streaming::stream_to_writer_sha256(
        stream,
        &mut file,
        state.config.max_file_size_bytes(),
        state.config.upload_idle_timeout(),
    )
    .await;
    let (size, sha256) = match streamed {
        Ok(done) => done,
        Err(e) => {
            let _ = tokio::fs::remove_file(&temp).await;
            return Err(super::map_upload_err(e));
        }
    };
    if let Err(e) = file.sync_all().await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(Error::Io(e));
    }
    drop(file);
    if expected.as_deref().is_some_and(|want| want != sha256) {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(Error::BadRequest(format!(
            "checksum mismatch: the body's SHA-256 is {sha256}"
        )));
    }
    let (status, file) = match attach(&state, &id, &v, &name, temp, &sha256, size).await? {
        Attached::New(f) => (StatusCode::CREATED, f),
        Attached::Same(f) => (StatusCode::OK, f),
    };
    Ok((status, Json(attached_json(&state, &headers, &file))).into_response())
}

pub(super) async fn delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version, name)): Path<(String, String, String)>,
) -> Result<StatusCode> {
    authorize_upload(&state, &headers)?;
    let v = parse_version(&version)?;
    detach(&state, &id, &v, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Resumable upload: tus 1.0.0
// ---------------------------------------------------------------------------

/// The hashing state of an upload being written, kept between its requests:
/// the SHA-256 of the first `received` bytes.
struct Hashing {
    received: u64,
    hasher: Sha256,
}

/// One lock per upload in progress. Holding it is writing to the upload, so
/// two requests never append to one file at once. The state inside is the
/// running hash; it is lost on restart, and rebuilt from the partial file by
/// the first request after it.
type Slot = Arc<tokio::sync::Mutex<Option<Hashing>>>;
static SLOTS: LazyLock<std::sync::Mutex<HashMap<String, Slot>>> = LazyLock::new(Default::default);

fn slot(upload: &str) -> Slot {
    SLOTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(upload.to_string())
        .or_default()
        .clone()
}

fn forget_slot(upload: &str) {
    SLOTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(upload);
}

/// Where an upload's bytes collect until it is finished. `.part`, not `.tmp`:
/// the startup sweep of abandoned temp files must leave these alone.
fn part_path(state: &AppState, upload: &str) -> PathBuf {
    state.temp_dir.join(format!("{upload}.part"))
}

/// A tus response: every one carries the protocol version.
fn tus_response(status: StatusCode) -> axum::http::response::Builder {
    Response::builder()
        .status(status)
        .header("tus-resumable", TUS_VERSION)
        .header(header::CACHE_CONTROL, "no-store")
}

fn built(b: axum::http::response::Builder) -> Result<Response> {
    b.body(axum::body::Body::empty())
        .map_err(|e| Error::Other(anyhow::anyhow!("failed to build response: {e}")))
}

/// Requests other than `OPTIONS` must say they speak tus 1.0.0: the answer
/// to one that does not, or `None` to go on.
fn wrong_tus_version(headers: &HeaderMap) -> Option<Response> {
    match headers.get("tus-resumable").and_then(|v| v.to_str().ok()) {
        Some(TUS_VERSION) => None,
        _ => Some(
            tus_response(StatusCode::PRECONDITION_FAILED)
                .header("tus-version", TUS_VERSION)
                .body(axum::body::Body::from("this server speaks tus 1.0.0"))
                .unwrap_or_default(),
        ),
    }
}

fn header_u64(headers: &HeaderMap, name: &str) -> Result<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
        .ok_or_else(|| Error::BadRequest(format!("missing or invalid {name} header")))
}

/// `Upload-Metadata`: comma-separated `key base64value` pairs.
fn parse_metadata(raw: &str) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for pair in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once(' ').unwrap_or((pair, ""));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(value.trim())
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
            .ok_or_else(|| {
                Error::BadRequest(format!("Upload-Metadata {key:?} is not base64 UTF-8"))
            })?;
        out.insert(key.to_string(), decoded);
    }
    Ok(out)
}

fn http_date(when: chrono::DateTime<Utc>) -> String {
    when.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// `OPTIONS …/uploads`: what this server supports.
pub(super) async fn tus_options(State(state): State<AppState>) -> Result<Response> {
    let mut b = tus_response(StatusCode::NO_CONTENT)
        .header("tus-version", TUS_VERSION)
        .header("tus-extension", TUS_EXTENSIONS);
    if let Some(max) = state.config.max_file_size_bytes() {
        b = b.header("tus-max-size", max);
    }
    built(b)
}

/// `POST …/uploads`: start an upload. `Upload-Length` is the file's size;
/// `Upload-Metadata` names the `id`, `version` and `filename`, and may give
/// the `sha256` the finished file must have.
pub(super) async fn tus_create(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response> {
    if let Some(r) = wrong_tus_version(&headers) {
        return Ok(r);
    }
    authorize_upload(&state, &headers)?;
    let length = header_u64(&headers, "upload-length")?;
    let meta = parse_metadata(
        headers
            .get("upload-metadata")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
    )?;
    let field = |k: &str| {
        meta.get(k)
            .map(String::as_str)
            .ok_or_else(|| Error::BadRequest(format!("Upload-Metadata needs {k:?}")))
    };
    let id = field("id")?;
    let v = parse_version(field("version")?)?;
    let name = field("filename")?;
    crate::validation::validate_file_name(name, &state.config.files)?;
    let expected = meta.get("sha256").map(|s| parse_sha256(s)).transpose()?;
    version_in_feed(&state, id, &v).await?;
    if state.db.get_file(id, &v, name).await?.is_some() {
        return Err(Error::Conflict(format!(
            "{id} {} already has a file named {name}",
            v.normalized()
        )));
    }
    check_size(&state, Some(length))?;
    state.ensure_disk_space(Some(length))?;

    let upload = uuid::Uuid::new_v4().simple().to_string();
    tokio::fs::File::create(part_path(&state, &upload)).await?;
    let expires = Utc::now()
        + chrono::Duration::hours(state.config.files.upload_expiry_hours.min(24 * 365) as i64);
    state
        .db
        .create_upload(&UploadSession {
            id: upload.clone(),
            feed: state.feed().to_string(),
            lower_id: id.to_lowercase(),
            normalized_version: v.normalized(),
            name: name.to_string(),
            length,
            received: 0,
            expected_sha256: expected,
            expires,
        })
        .await?;
    let location = state.url_builder(&headers).upload(&upload);
    built(
        tus_response(StatusCode::CREATED)
            .header(header::LOCATION, location)
            .header("upload-expires", http_date(expires)),
    )
}

/// The upload `upload` of this feed, still current.
async fn session(state: &AppState, upload: &str) -> Result<UploadSession> {
    match state.db.get_upload(upload).await? {
        Some(s) if s.feed == state.feed() && s.expires > Utc::now() => Ok(s),
        _ => Err(Error::PackageNotFound),
    }
}

/// Bring `hashing` to the SHA-256 of the first `received` bytes of the
/// partial file — after a restart, by reading them once.
async fn resume_hash(
    hashing: &mut Option<Hashing>,
    part: &std::path::Path,
    received: u64,
) -> Result<()> {
    if hashing.as_ref().is_some_and(|h| h.received == received) {
        return Ok(());
    }
    let mut hasher = Sha256::new();
    let mut file = tokio::fs::File::open(part).await?;
    let mut left = received;
    let mut buf = vec![0u8; 256 * 1024];
    while left > 0 {
        let want = buf.len().min(left as usize);
        let n = file.read(&mut buf[..want]).await?;
        if n == 0 {
            return Err(Error::Other(anyhow::anyhow!(
                "partial upload is shorter than recorded"
            )));
        }
        hasher.update(&buf[..n]);
        left -= n as u64;
    }
    *hashing = Some(Hashing { received, hasher });
    Ok(())
}

/// `HEAD …/uploads/{id}`: how much has arrived.
pub(super) async fn tus_head(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(upload): Path<String>,
) -> Result<Response> {
    if let Some(r) = wrong_tus_version(&headers) {
        return Ok(r);
    }
    authorize_upload(&state, &headers)?;
    let s = session(&state, &upload).await?;
    built(
        tus_response(StatusCode::OK)
            .header("upload-offset", s.received)
            .header("upload-length", s.length)
            .header("upload-expires", http_date(s.expires)),
    )
}

/// `PATCH …/uploads/{id}`: append bytes at `Upload-Offset`, which must be
/// exactly what has arrived so far. The request that brings the last byte
/// verifies the whole file and attaches it.
pub(super) async fn tus_patch(
    State(state): State<AppState>,
    Path(upload): Path<String>,
    request: Request,
) -> Result<Response> {
    let headers = request.headers().clone();
    if let Some(r) = wrong_tus_version(&headers) {
        return Ok(r);
    }
    authorize_upload(&state, &headers)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.starts_with("application/offset+octet-stream") {
        return Ok(tus_response(StatusCode::UNSUPPORTED_MEDIA_TYPE)
            .body(axum::body::Body::from(
                "PATCH bodies are application/offset+octet-stream",
            ))
            .unwrap_or_default());
    }
    let offset = header_u64(&headers, "upload-offset")?;
    let s = session(&state, &upload).await?;

    // One writer at a time; a second request is turned away, not queued.
    let slot = slot(&upload);
    let Ok(mut hashing) = slot.try_lock() else {
        return Err(Error::Conflict(
            "another request is writing this upload".into(),
        ));
    };
    if offset != s.received {
        return built(tus_response(StatusCode::CONFLICT).header("upload-offset", s.received));
    }
    let part = part_path(&state, &upload);
    resume_hash(&mut hashing, &part, s.received).await?;
    let h = hashing.as_mut().expect("resume_hash sets the state");

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&part)
        .await?;
    // Anything past the recorded length is a write that never got counted.
    file.set_len(s.received).await?;
    file.seek(std::io::SeekFrom::Start(s.received)).await?;
    let mut body = Box::pin(
        request
            .into_body()
            .into_data_stream()
            .map(|r| r.map_err(to_io_err)),
    );
    let (written, streamed) = streaming::append_hashed(
        &mut body,
        &mut file,
        &mut h.hasher,
        s.length - s.received,
        state.config.upload_idle_timeout(),
    )
    .await;
    // What arrived counts even when the connection then dropped: that is the
    // point of a resumable upload.
    let received = s.received + written;
    h.received = received;
    let _ = file.flush().await;
    state.db.set_upload_received(&upload, received).await?;
    if let Err(e) = streamed {
        return Err(super::map_upload_err(e));
    }

    if received == s.length {
        file.sync_all().await?;
        drop(file);
        let sha256 = hex::encode(h.hasher.clone().finalize());
        *hashing = None;
        drop(hashing);
        forget_slot(&upload);
        state.db.delete_upload(&upload).await?;
        if s.expected_sha256
            .as_deref()
            .is_some_and(|want| want != sha256)
        {
            let _ = tokio::fs::remove_file(&part).await;
            return Err(Error::BadRequest(format!(
                "checksum mismatch: the upload's SHA-256 is {sha256}; it was discarded"
            )));
        }
        let v = parse_version(&s.normalized_version)?;
        attach(&state, &s.lower_id, &v, &s.name, part, &sha256, received).await?;
    }
    built(
        tus_response(StatusCode::NO_CONTENT)
            .header("upload-offset", received)
            .header("upload-expires", http_date(s.expires)),
    )
}

/// `DELETE …/uploads/{id}`: abandon an upload.
pub(super) async fn tus_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(upload): Path<String>,
) -> Result<Response> {
    if let Some(r) = wrong_tus_version(&headers) {
        return Ok(r);
    }
    authorize_upload(&state, &headers)?;
    session(&state, &upload).await?;
    let slot = slot(&upload);
    let Ok(_writing) = slot.try_lock() else {
        return Err(Error::Conflict("the upload is being written".into()));
    };
    let _ = tokio::fs::remove_file(part_path(&state, &upload)).await;
    state.db.delete_upload(&upload).await?;
    forget_slot(&upload);
    built(tus_response(StatusCode::NO_CONTENT))
}

/// Drop uploads whose expiry has passed, with their partial files. Returns how
/// many went.
pub async fn sweep_expired_uploads(
    db: &dyn crate::database::PackageDatabase,
    temp_dir: &std::path::Path,
) -> usize {
    let expired = match db.expired_uploads(Utc::now()).await {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(error = %e, "could not list expired uploads");
            return 0;
        }
    };
    let mut removed = 0;
    for upload in expired {
        // Skip one being written right now; the next sweep gets it.
        let slot = slot(&upload.id);
        let Ok(_writing) = slot.try_lock() else {
            continue;
        };
        let _ = tokio::fs::remove_file(temp_dir.join(format!("{}.part", upload.id))).await;
        if db.delete_upload(&upload.id).await.is_ok() {
            removed += 1;
        }
        forget_slot(&upload.id);
    }
    if removed > 0 {
        tracing::info!(removed, "dropped expired resumable uploads");
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_metadata_is_base64_pairs() {
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let raw = format!(
            "id {},version {}, filename {},empty",
            b64("Contoso.Images"),
            b64("1.2.0"),
            b64("base.wim")
        );
        let meta = parse_metadata(&raw).unwrap();
        assert_eq!(meta["id"], "Contoso.Images");
        assert_eq!(meta["version"], "1.2.0");
        assert_eq!(meta["filename"], "base.wim");
        assert_eq!(meta["empty"], "");
        assert!(parse_metadata("id !!notbase64!!").is_err());
    }

    #[test]
    fn a_checksum_is_64_hex_digits() {
        let hex = "AB".repeat(32);
        assert_eq!(parse_sha256(&hex).unwrap(), "ab".repeat(32));
        assert!(parse_sha256("abc").is_err());
        assert!(parse_sha256(&"zz".repeat(32)).is_err());
    }
}
