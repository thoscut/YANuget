//! Streaming file responses with HTTP range support.
//!
//! Large packages must be downloadable without ever holding the file in memory
//! and must be *resumable*, so this serves files as a streamed body and honours
//! a single `Range: bytes=...` request, replying `206 Partial Content`.

use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::error::{Error, Result};

/// Serve a local file as a streamed response, honouring a `Range` header.
///
/// `download_name`, when set, is offered as a `Content-Disposition` filename.
pub async fn serve_local_file(
    path: PathBuf,
    headers: &HeaderMap,
    content_type: &'static str,
    download_name: Option<&str>,
) -> Result<Response> {
    let mut file = tokio::fs::File::open(&path)
        .await
        .map_err(|_| Error::PackageNotFound)?;
    let total = file.metadata().await?.len();

    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some(name) = download_name {
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}\""),
        );
    }

    match parse_range(headers, total) {
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

/// Parse a single-range `Range: bytes=...` header against a file of `total`
/// bytes. Multi-range requests are intentionally treated as "serve whole file".
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
        ("", suffix) => match suffix.parse::<u64>() {
            Ok(0) => RangeResult::Unsatisfiable,
            Ok(n) => {
                let start = total.saturating_sub(n);
                RangeResult::Satisfiable { start, end: last }
            }
            Err(_) => RangeResult::None,
        },
        // Open-ended range: start to EOF.
        (start, "") => match start.parse::<u64>() {
            Ok(start) if start <= last => RangeResult::Satisfiable { start, end: last },
            Ok(_) => RangeResult::Unsatisfiable,
            Err(_) => RangeResult::None,
        },
        // Explicit range.
        (start, end) => match (start.parse::<u64>(), end.parse::<u64>()) {
            (Ok(start), Ok(end)) if start <= end && start <= last => RangeResult::Satisfiable {
                start,
                end: end.min(last),
            },
            (Ok(_), Ok(_)) => RangeResult::Unsatisfiable,
            _ => RangeResult::None,
        },
    }
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
    fn multi_range_falls_back_to_whole() {
        assert_eq!(
            parse_range(&headers_with_range("bytes=0-1,2-3"), 1000),
            RangeResult::None
        );
    }
}
