//! Streaming file responses with HTTP range support.
//!
//! Large packages must be downloadable without ever holding the file in memory
//! and must be *resumable*, so this serves files as a streamed body and honours
//! a single `Range: bytes=...` request, replying `206 Partial Content`.

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use chrono::{DateTime, Utc};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::error::{Error, Result};

/// How a response may be cached, and by whom.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CachePolicy {
    /// Only the requesting client may keep a copy: the content is behind a
    /// read key, and a shared cache or CDN in front of the server would
    /// otherwise hand it to anyone.
    pub private: bool,
    /// The bytes under this URL can never change, so a copy never needs
    /// revalidating. Only true where the server guarantees it (for a package,
    /// while overwriting is off); otherwise every use revalidates, which the
    /// `ETag` makes a cheap `304`.
    pub immutable: bool,
}

impl CachePolicy {
    /// The `Cache-Control` value.
    pub fn header_value(self) -> &'static str {
        match (self.private, self.immutable) {
            (false, true) => "public, max-age=31536000, immutable",
            (true, true) => "private, max-age=31536000, immutable",
            (false, false) => "public, no-cache",
            (true, false) => "private, no-cache",
        }
    }
}

/// What a served file is, beyond its bytes.
#[derive(Debug, Default, Clone, Copy)]
pub struct FileMeta<'a> {
    /// Offered as the `Content-Disposition: attachment` filename.
    pub download_name: Option<&'a str>,
    /// A strong validator (a content hash). Served as `ETag`, and answers a
    /// matching `If-None-Match` with `304`.
    pub etag: Option<&'a str>,
    /// Served as `Cache-Control`.
    pub cache: CachePolicy,
    /// When the file was published. Served as `Last-Modified`, which BITS
    /// compares between the requests of one transfer to notice a changed file.
    pub last_modified: Option<DateTime<Utc>>,
    /// The base64 SHA-256 of the whole file, served as `Repr-Digest`
    /// (RFC 9530) so a client can check what it assembled from ranges.
    pub sha256_base64: Option<&'a str>,
}

/// Serve a local file as a streamed response, honouring a `Range` header.
///
/// A `Range` is honoured only while an `If-Range` the client sent still
/// matches: a resume against a file that changed underneath it must get the
/// whole new file (`200`), not a splice of old and new bytes.
pub async fn serve_local_file(
    path: PathBuf,
    headers: &HeaderMap,
    content_type: &'static str,
    meta: FileMeta<'_>,
) -> Result<Response> {
    let file = open(&path).await?;
    serve_open_file(file, headers, content_type, meta).await
}

/// Open a stored file for serving; a missing one is a `404`.
///
/// Separate from [`serve_open_file`] so a caller can look up the file's
/// validators *after* it holds the handle: an overwrite renames new bytes into
/// place, and a tag read before the open could otherwise be served with bytes
/// opened after it (and let an `If-Range` splice the two).
pub async fn open(path: &std::path::Path) -> Result<tokio::fs::File> {
    tokio::fs::File::open(path)
        .await
        .map_err(|_| Error::PackageNotFound)
}

/// Serve an already-open file, as [`serve_local_file`] does.
pub async fn serve_open_file(
    mut file: tokio::fs::File,
    headers: &HeaderMap,
    content_type: &'static str,
    meta: FileMeta<'_>,
) -> Result<Response> {
    let total = file.metadata().await?.len();

    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some(name) = meta.download_name {
        builder = builder.header(header::CONTENT_DISPOSITION, content_disposition(name));
    }
    let last_modified = meta.last_modified.map(http_date);
    if let Some(date) = &last_modified {
        builder = builder.header(header::LAST_MODIFIED, date);
    }
    if let Some(digest) = meta.sha256_base64 {
        builder = builder.header("repr-digest", format!("sha-256=:{digest}:"));
    }
    // With a validator, a client that already holds the file pays for a
    // header exchange rather than for the bytes again — for a multi-gigabyte
    // package, the difference that matters. Whether it may skip even that is
    // the cache policy's call.
    let cache_control = meta.cache.header_value();
    builder = builder.header(header::CACHE_CONTROL, cache_control);
    let quoted = meta.etag.map(|tag| format!("\"{tag}\""));
    if let Some(quoted) = &quoted {
        if if_none_match_hits(headers, quoted) {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, quoted)
                .header(header::CACHE_CONTROL, cache_control)
                .body(Body::empty())
                .map_err(internal);
        }
        builder = builder.header(header::ETAG, quoted);
    }

    let range = if if_range_holds(headers, quoted.as_deref(), last_modified.as_deref()) {
        parse_range(headers, total)
    } else {
        RangeResult::None
    };
    match range {
        RangeResult::None => {
            let stream = ReaderStream::new(file);
            builder
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, total)
                .body(Body::from_stream(stream))
                .map_err(internal)
        }
        RangeResult::Satisfiable { start, end } => {
            file.seek(std::io::SeekFrom::Start(start))
                .await
                .map_err(Error::Io)?;
            let len = end - start + 1;
            let stream = ReaderStream::new(file.take(len));
            builder
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_LENGTH, len)
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{total}"),
                )
                .body(Body::from_stream(stream))
                .map_err(internal)
        }
        RangeResult::Unsatisfiable => builder
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::CONTENT_RANGE, format!("bytes */{total}"))
            .body(Body::empty())
            .map_err(internal),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RangeResult {
    /// No (usable) range header — serve the whole file.
    None,
    /// A satisfiable, inclusive byte range.
    Satisfiable { start: u64, end: u64 },
    /// A syntactically valid but out-of-bounds range.
    Unsatisfiable,
}

/// A range position: decimal digits only. `u64::from_str` also takes a leading
/// `+`, which RFC 9110 does not.
fn range_pos(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parse a single-range `Range: bytes=...` header against a file of `total`
/// bytes. Multi-range requests are intentionally treated as "serve whole file",
/// and so is anything that is not a valid range (RFC 9110 §14.2: a server
/// ignores a `Range` it cannot parse).
fn parse_range(headers: &HeaderMap, total: u64) -> RangeResult {
    let Some(value) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) else {
        return RangeResult::None;
    };
    let Some(spec) = value.strip_prefix("bytes=") else {
        return RangeResult::None;
    };
    // Only a single range is supported.
    if spec.contains(',') {
        return RangeResult::None;
    }
    let Some((start_s, end_s)) = spec.split_once('-') else {
        return RangeResult::None;
    };

    if total == 0 {
        return RangeResult::Unsatisfiable;
    }
    let last = total - 1;

    match (start_s.trim(), end_s.trim()) {
        // Suffix range: last N bytes.
        ("", suffix) => match range_pos(suffix) {
            Some(0) => RangeResult::Unsatisfiable,
            Some(n) => {
                let start = total.saturating_sub(n);
                RangeResult::Satisfiable { start, end: last }
            }
            None => RangeResult::None,
        },
        // Open-ended range: start to EOF.
        (start, "") => match range_pos(start) {
            Some(start) if start <= last => RangeResult::Satisfiable { start, end: last },
            Some(_) => RangeResult::Unsatisfiable,
            None => RangeResult::None,
        },
        // Explicit range. A last position before the first is not a range at
        // all, so the header is ignored rather than answered with 416.
        (start, end) => match (range_pos(start), range_pos(end)) {
            (Some(start), Some(end)) if start > end => RangeResult::None,
            (Some(start), Some(end)) if start <= last => RangeResult::Satisfiable {
                start,
                end: end.min(last),
            },
            (Some(_), Some(_)) => RangeResult::Unsatisfiable,
            _ => RangeResult::None,
        },
    }
}

/// An IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`), the form HTTP dates take.
fn http_date(when: DateTime<Utc>) -> String {
    when.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

/// Whether a `Range` may be honoured given the request's `If-Range`.
///
/// Absent, it always may. An entity tag must match the current one exactly
/// (a weak tag never matches, RFC 9110 §13.1.5), and a date must equal the
/// current `Last-Modified`. Anything else means the client holds bytes of a
/// different file, so it gets the whole current one instead.
fn if_range_holds(headers: &HeaderMap, etag: Option<&str>, last_modified: Option<&str>) -> bool {
    let Some(value) = headers.get(header::IF_RANGE).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let value = value.trim();
    if value.starts_with('"') {
        etag == Some(value)
    } else if value.starts_with("W/") {
        false
    } else {
        last_modified == Some(value)
    }
}

/// Whether a request for a file counts as one download.
///
/// A resumable client fetches one file in many ranged requests, and BITS
/// starts every transfer with a `HEAD`. Only a `GET` for the whole file, or
/// for a range from its first byte, is a download starting; the rest are the
/// same download continuing.
pub fn counts_as_download(method: &axum::http::Method, headers: &HeaderMap) -> bool {
    if method != axum::http::Method::GET {
        return false;
    }
    match headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(range) => range
            .trim()
            .strip_prefix("bytes=")
            .is_some_and(|spec| spec.trim_start().starts_with("0-")),
    }
}

/// Whether a response carries the file's bytes (`200` or `206`), as opposed to
/// a `304`, a `416` or an error.
pub fn sends_content(response: &Response) -> bool {
    matches!(
        response.status(),
        StatusCode::OK | StatusCode::PARTIAL_CONTENT
    )
}

/// Whether `If-None-Match` names `quoted` (or is the `*` wildcard).
pub fn if_none_match_hits(headers: &HeaderMap, quoted: &str) -> bool {
    let Some(raw) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    raw.split(',').any(|candidate| {
        let c = candidate.trim();
        // Weak comparison, as RFC 9110 §13.1.2 prescribes for
        // `If-None-Match`: a `W/` tag names the same entity.
        let c = c.strip_prefix("W/").unwrap_or(c);
        c == "*" || c == quoted
    })
}

/// Build a `Content-Disposition` value for `name`.
///
/// The name is derived from a package id and version, both already restricted
/// to a conservative character set — but this header ends up steering where a
/// client writes a file, so nothing outside that set is allowed through in the
/// quoted form. Anything unexpected is dropped rather than escaped, and the
/// RFC 5987 `filename*` form carries the exact name for clients that read it.
fn content_disposition(name: &str) -> String {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
        .collect();
    let safe = if safe.is_empty() {
        "package.nupkg".to_string()
    } else {
        safe
    };
    let encoded = percent_encoding::utf8_percent_encode(name, percent_encoding::NON_ALPHANUMERIC);
    format!("attachment; filename=\"{safe}\"; filename*=UTF-8''{encoded}")
}

fn internal(e: axum::http::Error) -> Error {
    Error::Other(anyhow::anyhow!("failed to build response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with_range(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn no_range_header() {
        assert_eq!(parse_range(&HeaderMap::new(), 1000), RangeResult::None);
    }

    #[test]
    fn explicit_range() {
        assert_eq!(
            parse_range(&headers_with_range("bytes=0-499"), 1000),
            RangeResult::Satisfiable { start: 0, end: 499 }
        );
        // End clamped to last byte.
        assert_eq!(
            parse_range(&headers_with_range("bytes=500-100000"), 1000),
            RangeResult::Satisfiable {
                start: 500,
                end: 999
            }
        );
    }

    #[test]
    fn open_ended_and_suffix() {
        assert_eq!(
            parse_range(&headers_with_range("bytes=900-"), 1000),
            RangeResult::Satisfiable {
                start: 900,
                end: 999
            }
        );
        assert_eq!(
            parse_range(&headers_with_range("bytes=-100"), 1000),
            RangeResult::Satisfiable {
                start: 900,
                end: 999
            }
        );
        // Suffix larger than file -> whole file.
        assert_eq!(
            parse_range(&headers_with_range("bytes=-5000"), 1000),
            RangeResult::Satisfiable { start: 0, end: 999 }
        );
    }

    #[test]
    fn unsatisfiable_range() {
        assert_eq!(
            parse_range(&headers_with_range("bytes=2000-3000"), 1000),
            RangeResult::Unsatisfiable
        );
    }

    #[test]
    fn invalid_ranges_are_ignored_not_refused() {
        // Last before first is not a range: serve the whole file (RFC 9110).
        assert_eq!(
            parse_range(&headers_with_range("bytes=5-3"), 1000),
            RangeResult::None
        );
        // Signs are not part of the grammar.
        for bad in ["bytes=+5-10", "bytes=5-+10", "bytes=-+5", "bytes=+5-"] {
            assert_eq!(
                parse_range(&headers_with_range(bad), 1000),
                RangeResult::None,
                "{bad}"
            );
        }
    }

    #[test]
    fn cache_policy_headers() {
        let p = |private, immutable| CachePolicy { private, immutable }.header_value();
        assert_eq!(p(false, true), "public, max-age=31536000, immutable");
        assert_eq!(p(true, true), "private, max-age=31536000, immutable");
        assert_eq!(p(false, false), "public, no-cache");
        assert_eq!(p(true, false), "private, no-cache");
    }

    #[test]
    fn content_disposition_keeps_only_expected_characters() {
        // The ordinary case is unchanged.
        let ok = content_disposition("contoso.utils.1.0.0.nupkg");
        assert!(ok.contains("filename=\"contoso.utils.1.0.0.nupkg\""));

        // A name carrying quotes, separators or CR/LF must not be able to break
        // out of the quoted form or steer where a client writes the file.
        let nasty = content_disposition("a\"b/../c\r\nX-Evil: 1");
        let quoted = nasty
            .split("filename=\"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap();
        assert!(!quoted.contains('"'));
        assert!(!quoted.contains('/'));
        assert!(!quoted.contains('\r'));
        assert!(!quoted.contains('\n'));

        // A name with nothing usable still yields a valid header.
        assert!(content_disposition("///").contains("filename=\"package.nupkg\""));
    }

    #[test]
    fn if_none_match_matches_strong_weak_and_wildcard() {
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"abc\""));
        assert!(if_none_match_hits(&h, "\"abc\""));
        assert!(!if_none_match_hits(&h, "\"def\""));

        h.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("W/\"abc\", \"other\""),
        );
        assert!(if_none_match_hits(&h, "\"abc\""));

        h.insert(header::IF_NONE_MATCH, HeaderValue::from_static("*"));
        assert!(if_none_match_hits(&h, "\"anything\""));

        assert!(!if_none_match_hits(&HeaderMap::new(), "\"abc\""));
    }

    #[test]
    fn if_range_honours_only_the_current_file() {
        let date = http_date(
            DateTime::parse_from_rfc3339("2026-09-24T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert_eq!(date, "Thu, 24 Sep 2026 10:00:00 GMT");
        let with = |v: &str| {
            let mut h = HeaderMap::new();
            h.insert(header::IF_RANGE, HeaderValue::from_str(v).unwrap());
            h
        };
        let (etag, lm) = (Some("\"abc\""), Some(date.as_str()));
        assert!(if_range_holds(&HeaderMap::new(), etag, lm));
        assert!(if_range_holds(&with("\"abc\""), etag, lm));
        assert!(!if_range_holds(&with("\"old\""), etag, lm));
        // A weak tag is never good enough to splice bytes together.
        assert!(!if_range_holds(&with("W/\"abc\""), etag, lm));
        assert!(if_range_holds(&with(&date), etag, lm));
        assert!(!if_range_holds(
            &with("Wed, 23 Sep 2026 10:00:00 GMT"),
            etag,
            lm
        ));
        // Nothing to compare against: the client's bytes cannot be vouched for.
        assert!(!if_range_holds(&with("\"abc\""), None, None));
    }

    #[test]
    fn only_the_start_of_a_transfer_counts_as_a_download() {
        use axum::http::Method;
        let range = |v: &str| headers_with_range(v);
        assert!(counts_as_download(&Method::GET, &HeaderMap::new()));
        assert!(counts_as_download(&Method::GET, &range("bytes=0-")));
        assert!(counts_as_download(&Method::GET, &range("bytes=0-1048575")));
        assert!(!counts_as_download(&Method::GET, &range("bytes=1048576-")));
        assert!(!counts_as_download(&Method::GET, &range("bytes=-500")));
        assert!(!counts_as_download(&Method::HEAD, &HeaderMap::new()));
    }

    #[test]
    fn multi_range_falls_back_to_whole() {
        assert_eq!(
            parse_range(&headers_with_range("bytes=0-1,2-3"), 1000),
            RangeResult::None
        );
    }
}
