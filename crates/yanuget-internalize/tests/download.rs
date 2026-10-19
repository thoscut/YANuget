//! The downloader against a local host: checksums, the cache, the size cap,
//! resuming and restarting, and the address policy.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use sha2::{Digest, Sha256};
use yanuget_internalize::capture::{Checksum, ChecksumType};
use yanuget_internalize::config::DownloadsConfig;
use yanuget_internalize::download::{DownloadError, Downloader};
use yanuget_internalize::upstream::http::RetryPolicy;
use yanuget_internalize::upstream::UpstreamError;
use yanuget_pkg::report::ChecksumStatus;

/// The file every route serves: 64 KiB, so it arrives in several chunks.
fn content() -> Vec<u8> {
    (0..64 * 1024).map(|i| (i % 251) as u8).collect()
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Default)]
struct Seen {
    /// The `Range` header of each request, in order.
    ranges: Vec<Option<String>>,
}

type Shared = Arc<Mutex<Seen>>;

/// Half the file, then, once the client has had time to read it, the
/// connection breaks.
fn broken_body(bytes: Vec<u8>) -> Body {
    use futures_util::StreamExt;
    let half = Bytes::from(bytes[..bytes.len() / 2].to_vec());
    let first = futures_util::stream::once(async move { Ok::<_, std::io::Error>(half) });
    let cut = futures_util::stream::once(async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Err(std::io::Error::other("cut"))
    });
    Body::from_stream(first.chain(cut))
}

async fn file(State(s): State<Shared>, headers: HeaderMap, resumable: bool) -> Response {
    let range = headers
        .get("range")
        .map(|v| v.to_str().unwrap().to_string());
    let first = {
        let mut seen = s.lock().unwrap();
        seen.ranges.push(range.clone());
        seen.ranges.len() == 1
    };
    let bytes = content();
    let len = bytes.len();
    let mut response = if let (true, Some(r)) = (resumable, &range) {
        assert_eq!(headers.get("if-range").unwrap(), "\"v1\"");
        let start: usize = r
            .trim_start_matches("bytes=")
            .trim_end_matches('-')
            .parse()
            .unwrap();
        (
            StatusCode::PARTIAL_CONTENT,
            [("content-range", format!("bytes {start}-{}/{len}", len - 1))],
            bytes[start..].to_vec(),
        )
            .into_response()
    } else if first {
        Response::builder()
            .header("content-length", len)
            .body(broken_body(bytes))
            .unwrap()
    } else {
        bytes.into_response()
    };
    if resumable {
        let h = response.headers_mut();
        h.insert("accept-ranges", "bytes".parse().unwrap());
        h.insert("etag", "\"v1\"".parse().unwrap());
    }
    response
}

async fn host() -> (String, Shared) {
    let state: Shared = Arc::default();
    let app = Router::new()
        .route(
            "/plain/setup.exe",
            get(|| async {
                (
                    [(
                        "content-disposition",
                        "attachment; filename=\"Setup 1.0.exe\"",
                    )],
                    content(),
                )
            }),
        )
        .route(
            "/resumable",
            get(|s: State<Shared>, h: HeaderMap| file(s, h, true)),
        )
        .route(
            "/restart",
            get(|s: State<Shared>, h: HeaderMap| file(s, h, false)),
        )
        .route(
            "/redirect",
            get(|| async { axum::response::Redirect::temporary("/plain/setup.exe") }),
        )
        .route("/missing", get(|| async { StatusCode::NOT_FOUND }))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), state)
}

fn config() -> DownloadsConfig {
    DownloadsConfig {
        allow_private_hosts: vec!["127.0.0.1".into()],
        timeout_secs: 5,
        ..DownloadsConfig::default()
    }
}

fn downloader(config: &DownloadsConfig, cache: &std::path::Path) -> Downloader {
    let mut d = Downloader::new(config, cache).unwrap();
    d.http_mut().set_retry(RetryPolicy {
        attempts: 2,
        base: Duration::from_millis(10),
        max_wait: Duration::from_millis(10),
    });
    d
}

fn sum(kind: ChecksumType, hex: String) -> Checksum {
    Checksum { kind, hex }
}

fn files(cache: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(cache.join("files"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn checksums_decide_what_a_file_is_worth() {
    let (base, _) = host().await;
    let cache = tempfile::tempdir().unwrap();
    let d = downloader(&config(), cache.path());
    let url = format!("{base}/redirect").parse().unwrap();
    let body = content();

    // A declared SHA-256 that matches verifies the file, which lands under
    // its hash; the redirect's target is where it came from.
    let declared = sum(ChecksumType::Sha256, sha256(&body));
    let got = d.fetch(&url, Some(&declared)).await.unwrap();
    assert_eq!(got.checksum.status, ChecksumStatus::Verified);
    assert_eq!(got.checksum.algorithm.as_deref(), Some("sha256"));
    assert_eq!(std::fs::read(&got.path).unwrap(), body);
    assert_eq!(got.size, body.len() as u64);
    assert_eq!(got.head, body[..16]);
    assert!(got.final_url.unwrap().path().ends_with("/plain/setup.exe"));
    assert_eq!(
        got.content_disposition.as_deref(),
        Some("attachment; filename=\"Setup 1.0.exe\"")
    );
    // The second time it comes from the cache.
    let again = d.fetch(&url, Some(&declared)).await.unwrap();
    assert!(again.final_url.is_none());
    assert_eq!(again.head, body[..16]);

    // MD5 and nothing are trust-on-first-use, a matching MD5 included.
    let md5 = hex::encode(<md5::Md5 as Digest>::digest(&body));
    let got = d
        .fetch(&url, Some(&sum(ChecksumType::Md5, md5)))
        .await
        .unwrap();
    assert_eq!(got.checksum.status, ChecksumStatus::TrustOnFirstUse);
    assert_eq!(got.checksum.algorithm.as_deref(), Some("md5"));
    let got = d.fetch(&url, None).await.unwrap();
    assert_eq!(got.checksum.status, ChecksumStatus::TrustOnFirstUse);
    assert_eq!(got.checksum.algorithm, None);
    let sha512 = hex::encode(sha2::Sha512::digest(&body));
    let got = d
        .fetch(&url, Some(&sum(ChecksumType::Sha512, sha512)))
        .await
        .unwrap();
    assert_eq!(got.checksum.status, ChecksumStatus::Verified);

    // A mismatch fails, and leaves nothing behind.
    let wrong = sum(ChecksumType::Sha1, "0".repeat(40));
    let err = d.fetch(&url, Some(&wrong)).await.unwrap_err();
    assert!(
        matches!(err, DownloadError::ChecksumMismatch { .. }),
        "{err}"
    );
    assert_eq!(files(cache.path()), [format!("sha256-{}", sha256(&body))]);

    let missing = format!("{base}/missing").parse().unwrap();
    let err = d.fetch(&missing, None).await.unwrap_err();
    assert!(err.to_string().contains("404"), "{err}");
}

#[tokio::test]
async fn a_broken_transfer_resumes_or_restarts() {
    let (base, seen) = host().await;
    let cache = tempfile::tempdir().unwrap();
    let d = downloader(&config(), cache.path());
    let body = content();

    let got = d
        .fetch(&format!("{base}/resumable").parse().unwrap(), None)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&got.path).unwrap(), body);
    assert_eq!(got.sha256, sha256(&body));
    let ranges = std::mem::take(&mut seen.lock().unwrap().ranges);
    assert_eq!(ranges.len(), 2, "{ranges:?}");
    assert_eq!(ranges[0], None);
    assert!(
        ranges[1].as_deref().unwrap().starts_with("bytes="),
        "{ranges:?}"
    );

    // A host without ranges sends the whole file again.
    let got = d
        .fetch(&format!("{base}/restart").parse().unwrap(), None)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&got.path).unwrap(), body);
    assert_eq!(got.head, body[..16], "the restart starts the head over");
    let ranges = std::mem::take(&mut seen.lock().unwrap().ranges);
    assert_eq!(ranges, [None, None]);

    // With no resume allowed, a break fails the file.
    let d = downloader(
        &DownloadsConfig {
            resume_attempts: 0,
            ..config()
        },
        cache.path(),
    );
    let err = d
        .fetch(&format!("{base}/restart").parse().unwrap(), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("broke off"), "{err}");
}

#[tokio::test]
async fn size_and_addresses_are_policed() {
    let (base, _) = host().await;
    let cache = tempfile::tempdir().unwrap();
    let url: reqwest::Url = format!("{base}/plain/setup.exe").parse().unwrap();

    let small = downloader(
        &DownloadsConfig {
            max_file_bytes: 1000,
            ..config()
        },
        cache.path(),
    );
    let err = small.fetch(&url, None).await.unwrap_err();
    assert!(matches!(err, DownloadError::TooLarge(_)), "{err}");

    // Loopback is private: refused unless the host is allowed.
    let strict = downloader(&DownloadsConfig::default(), cache.path());
    let err = strict.fetch(&url, None).await.unwrap_err();
    assert!(
        matches!(err, DownloadError::Http(UpstreamError::Refused(_))),
        "{err}"
    );
    assert!(
        err.to_string().contains("downloads.allow_private_hosts"),
        "{err}"
    );
    let named = format!("http://localhost:{}/plain/setup.exe", url.port().unwrap());
    let err = strict
        .fetch(&named.parse().unwrap(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, DownloadError::Http(_)), "{err}");

    let partials: Vec<String> = files(cache.path())
        .into_iter()
        .filter(|n| n.starts_with("partial-"))
        .collect();
    assert!(partials.is_empty(), "{partials:?}");
}
