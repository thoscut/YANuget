//! Server plumbing end to end: forwarded headers, graceful shutdown,
//! monitoring endpoints and response compression.

mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use yanuget::config::Config;
use yanuget::database::SqliteDatabase;
use yanuget::storage::FilesystemStorage;
use yanuget::web::{self, AppState};

use common::*;

// ---------------------------------------------------------------------------
// Reverse-proxy URL derivation (X-Forwarded-*)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forwarded_headers_drive_generated_urls() {
    // Forwarding headers are ignored by default now, so a test about them has
    // to say the peer is a proxy — which is what a real proxy deployment does.
    let server = spawn_with(|c| c.trusted_proxies = vec!["private".into()]).await;
    let index: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .header("X-Forwarded-Proto", "https")
        .header("X-Forwarded-Host", "nuget.example.com")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        package_base_address(&index),
        "https://nuget.example.com/v3/package/"
    );
}

#[tokio::test]
async fn forwarded_host_takes_the_last_of_a_list() {
    // The rightmost entry is the one the trusted proxy wrote; anything to its
    // left came from the client.
    let server = spawn_with(|c| c.trusted_proxies = vec!["private".into()]).await;
    let index: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .header("X-Forwarded-Proto", "http, https")
        .header("X-Forwarded-Host", "client.example.net, proxy.example.com")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        package_base_address(&index),
        "https://proxy.example.com/v3/package/"
    );
}

#[tokio::test]
async fn host_header_used_without_forwarded() {
    // No forwarding headers: scheme follows config (http here) and host is the
    // request's Host header (the test server's address).
    let server = spawn().await;
    let index: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(package_base_address(&index).starts_with("http://127.0.0.1"));
}

// ---------------------------------------------------------------------------
// Graceful shutdown
// ---------------------------------------------------------------------------

#[tokio::test]
async fn graceful_shutdown_stops_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        api_key: Some(API_KEY.to_string()),
        tls_enabled: false,
        ..Config::default()
    };
    let storage = Arc::new(FilesystemStorage::new(config.storage_path()).await.unwrap());
    let db = Arc::new(
        SqliteDatabase::connect(&config.database_path())
            .await
            .unwrap(),
    );
    let state = AppState::new(storage, db, Arc::new(config)).await.unwrap();
    let app = web::router(state);

    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, connect_info(app))
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });

    let base = format!("http://{addr}");
    let client = common::client();
    assert!(client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());

    // Trigger graceful shutdown; the serve task must complete on its own.
    tx.send(()).unwrap();
    server.await.unwrap();

    // The listener is closed: new connections are refused.
    let after = common::client().get(format!("{base}/health")).send().await;
    assert!(after.is_err());
}

/// `TEST-28`: the real server ends on SIGTERM, the way a service manager or a
/// container runtime stops it: soon, with exit 0, and with the data lock let
/// go. The test above exercises axum's shutdown; `main` wires its own signal
/// handling, deadline and background work, which only the binary runs.
#[cfg(unix)]
#[tokio::test]
async fn the_server_stops_on_sigterm() {
    let data = tempfile::tempdir().unwrap();
    let server = common::binary::start_server(data.path(), &[]).await;
    let ready = common::client()
        .get(server.url("/health/ready"))
        .send()
        .await
        .unwrap();
    assert!(ready.status().is_success());

    // Idle, it has nothing to wait for.
    let status = server.terminate(std::time::Duration::from_secs(5)).await;
    assert!(status.success(), "{status}");
    let db = data
        .path()
        .join("yanuget.db")
        .to_string_lossy()
        .into_owned();
    let lock = yanuget::data_lock::DataLock::try_acquire(&db).unwrap();
    assert!(lock.is_some(), "the data lock is still held after the exit");
}

/// A push still arriving when SIGTERM comes holds the stop up for the grace
/// period, and no longer: the server then closes the connection and exits 0
/// rather than waiting on a client that may never finish.
#[cfg(unix)]
#[tokio::test]
async fn sigterm_waits_for_an_unfinished_request_only_for_the_grace_period() {
    use tokio::io::AsyncWriteExt;

    let data = tempfile::tempdir().unwrap();
    let command =
        common::binary::server_command_with(data.path(), &format!("api_key = '{API_KEY}'\n"));
    let server = common::binary::start(command).await;
    let mut upload = tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, server.port))
        .await
        .unwrap();
    // The head and the first bytes of a body that never comes in full.
    let head = format!(
        "PUT /api/v2/package HTTP/1.1\r\nHost: 127.0.0.1\r\nX-NuGet-ApiKey: {API_KEY}\r\n\
         Content-Type: multipart/form-data; boundary=b\r\nContent-Length: 1000000\r\n\r\n\
         --b\r\nContent-Disposition: form-data; name=\"package\"; filename=\"a.nupkg\"\r\n\r\nPK"
    );
    upload.write_all(head.as_bytes()).await.unwrap();
    upload.flush().await.unwrap();
    // Once a second request is answered, the first is being read.
    let ready = common::client()
        .get(server.url("/health/ready"))
        .send()
        .await
        .unwrap();
    assert!(ready.status().is_success());

    let started = std::time::Instant::now();
    let grace = yanuget::server::SHUTDOWN_GRACE;
    let status = server
        .terminate(grace + std::time::Duration::from_secs(20))
        .await;
    assert!(status.success(), "{status}");
    let took = started.elapsed();
    assert!(
        took >= grace - std::time::Duration::from_secs(1),
        "it stopped after {took:?}, without waiting for the request"
    );
    drop(upload);
}

// ---------------------------------------------------------------------------
// Monitoring: /metrics and /health/status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn monitoring_endpoints_are_off_unless_enabled() {
    let server = spawn().await;
    for path in ["/metrics", "/health/status"] {
        let resp = server.client.get(server.url(path)).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND, "{path}");
    }
    // The plain probes are always there.
    let resp = server
        .client
        .get(server.url("/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

#[tokio::test]
async fn metrics_count_pushes_and_downloads_per_feed() {
    let server = spawn_with(|c| c.monitoring.enabled = true).await;
    let resp = push_multipart(&server, API_KEY, build_nupkg("Metrics.Pkg", "1.0.0", b"x")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    let resp = server
        .client
        .get(server.url("/v3/package/metrics.pkg/1.0.0/metrics.pkg.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    resp.bytes().await.unwrap();

    let resp = server
        .client
        .get(server.url("/metrics"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/plain; version=0.0.4"));
    let body = resp.text().await.unwrap();
    for line in [
        "yanuget_database_up 1",
        "yanuget_pushes_total{feed=\"default\"} 1",
        "yanuget_downloads_total{feed=\"default\"} 1",
        "yanuget_feed_packages{feed=\"default\"} 1",
        "yanuget_feed_versions{feed=\"default\"} 1",
    ] {
        assert!(
            body.lines().any(|l| l == line),
            "missing {line:?} in\n{body}"
        );
    }
    // No mirror, no mirror metrics.
    assert!(!body.contains("yanuget_mirror_"));

    let status: serde_json::Value = server
        .client
        .get(server.url("/health/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["status"], "ok");
    assert_eq!(status["database"], "ok");
    assert_eq!(status["feeds"][0]["name"], "default");
    assert_eq!(status["feeds"][0]["pushes"], 1);
    assert_eq!(status["feeds"][0]["packages"], 1);
    assert!(status["feeds"][0].get("mirror").is_none());
}

#[tokio::test]
async fn a_monitoring_token_is_required_when_set() {
    let server = spawn_with(|c| {
        c.monitoring.enabled = true;
        c.monitoring.token = Some("scrape-me".into());
    })
    .await;
    for path in ["/metrics", "/health/status"] {
        let resp = server.client.get(server.url(path)).send().await.unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(resp.headers()["www-authenticate"], "Bearer");
        let resp = server
            .client
            .get(server.url(path))
            .bearer_auth("wrong")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED, "{path}");
        let resp = server
            .client
            .get(server.url(path))
            .bearer_auth("scrape-me")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK, "{path}");
    }
}

// ---------------------------------------------------------------------------
// Response compression
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_is_compressed_and_package_downloads_are_not() {
    use std::io::Read;
    let server = spawn().await;
    let nupkg = build_nupkg("Compress.Pkg", "1.0.0", &[b'x'; 4096]);
    let resp = push_multipart(&server, API_KEY, nupkg).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    let resp = server
        .client
        .get(server.url("/v3/registration/compress.pkg/index.json"))
        .header("accept-encoding", "gzip")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert_eq!(resp.headers()["content-encoding"], "gzip");
    let vary: Vec<_> = resp.headers().get_all("vary").iter().collect();
    assert!(
        vary.iter().any(|v| v
            .to_str()
            .unwrap()
            .to_ascii_lowercase()
            .contains("accept-encoding")),
        "{vary:?}"
    );
    let gz = resp.bytes().await.unwrap();
    let mut json = String::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_string(&mut json)
        .unwrap();
    let doc: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(doc["count"], 1);

    // Brotli is offered too; a client that asks for nothing gets plain JSON.
    let resp = server
        .client
        .get(server.url("/v3/registration/compress.pkg/index.json"))
        .header("accept-encoding", "br")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["content-encoding"], "br");
    let resp = server
        .client
        .get(server.url("/v3/registration/compress.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert!(resp.headers().get("content-encoding").is_none());

    let url = server.url("/v3/package/compress.pkg/1.0.0/compress.pkg.1.0.0.nupkg");
    let resp = server
        .client
        .get(&url)
        .header("accept-encoding", "gzip, br")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    assert!(resp.headers().get("content-encoding").is_none());
    assert_eq!(resp.headers()["accept-ranges"], "bytes");
    let length: usize = resp.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().len(), length);

    let resp = server
        .client
        .get(&url)
        .header("accept-encoding", "gzip")
        .header("range", "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert!(resp.headers().get("content-encoding").is_none());
    assert_eq!(resp.bytes().await.unwrap().len(), 10);
}
