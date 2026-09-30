//! Small request helpers shared by the handlers: validating an id or version
//! taken from a URL, reading forwarding and length headers, and running work
//! that must outlive a disconnecting client.

use axum::http::{header, HeaderMap};

use crate::error::{Error, Result};
use crate::version::NuGetVersion;

/// The declared size of a request body, when the client sent one.
pub(super) fn content_length(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse().ok())
}

/// Refuse a package id taken from a URL unless it is one a package could
/// have, before it reaches the database, the store or a lock.
///
/// The same rule a push is held to (`validate_package_id`), so every layer
/// below sees only ASCII ids, which `database::canonical_id`, storage paths
/// and the version lock all fold identically. An id no push could create —
/// one with the Kelvin sign `\u{212A}`, say, or a Windows device name — names
/// nothing, so it is a 404 before any of them is consulted.
pub(super) fn check_id(id: &str) -> Result<()> {
    crate::validation::validate_package_id(id).map_err(|_| Error::PackageNotFound)
}

/// Parse a version from a request (a URL segment or a form field).
///
/// A request only ever *names* a version to look up — every new version comes
/// from a manifest, parsed strictly — so a version an older release stored
/// under its laxer rules (a leading-zero label, more than 64 characters) has
/// to stay addressable: restorable, and deletable by an admin. Those fall back
/// to [`NuGetVersion::parse_stored`] and simply miss if no such row exists.
/// A leading `v` stays refused; no stored normalized form carries one.
pub(super) fn parse_version(raw: &str) -> Result<NuGetVersion> {
    NuGetVersion::parse(raw)
        .or_else(|e| {
            if raw.trim_start().starts_with(['v', 'V']) {
                Err(e)
            } else {
                NuGetVersion::parse_stored(raw)
            }
        })
        .map_err(|e| Error::InvalidVersion(e.to_string()))
}

/// Parse a version read back from the database (see
/// [`NuGetVersion::parse_stored`]).
pub(super) fn parse_stored_version(raw: &str) -> Result<NuGetVersion> {
    NuGetVersion::parse_stored(raw).map_err(|e| Error::InvalidVersion(e.to_string()))
}

/// A `semVerLevel` of `2.0.0` (or higher major) enables SemVer2 results.
pub(super) fn is_semver2_level(level: Option<&str>) -> bool {
    match level {
        Some(l) => NuGetVersion::parse(l)
            .map(|v| v.core().0 >= 2)
            .unwrap_or(false),
        None => false,
    }
}

pub(super) fn forwarded(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
        .filter(|s| !s.is_empty())
}

pub(super) fn to_io_err<E: std::fmt::Display>(e: E) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// Run `work` to completion in a task of its own and wait for it.
///
/// A client that disconnects cancels its request's future at whatever await
/// it has reached. Work that is a sequence of store and database steps — an
/// indexing, an attach, a mirror fetch — must not stop between two of them,
/// leaving a payload without its row or an overwrite with its old rows gone.
/// Spawned, it finishes either way; only the answer is lost.
pub(super) async fn detached<T: Send + 'static>(
    work: impl std::future::Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    tokio::spawn(work)
        .await
        .map_err(|e| Error::Other(anyhow::anyhow!("background task failed: {e}")))?
}

pub(super) fn map_upload_err(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::InvalidData {
        Error::PayloadTooLarge(e.to_string())
    } else if e.kind() == std::io::ErrorKind::TimedOut {
        Error::UploadTimeout(e.to_string())
    } else {
        Error::Io(e)
    }
}
