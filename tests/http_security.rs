//! End-to-end tests of the security claims in the README and SECURITY.md:
//! per-feed key isolation, read-gated feeds, forwarding chains, TLS, caching of
//! gated content, host validation and strict configuration.
//!
//! The helpers are a minimal copy of those in `integration.rs`, so each test
//! binary stays self-contained.

use std::io::{Cursor, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::StatusCode;
use yanuget::config::{Config, FeedConfig};
use yanuget::database::SqliteDatabase;
use yanuget::storage::FilesystemStorage;
use yanuget::web::{self, AppState, FeedMeta};
use zip::write::SimpleFileOptions;

const API_KEY: &str = "test-key";

struct TestServer {
    base: String,
    client: reqwest::Client,
    _dir: tempfile::TempDir,
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

fn base_config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        host: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        tls_enabled: false,
        ..Config::default()
    }
}

/// Serve `app` over plain HTTP with peer connection info, as `main.rs` does.
async fn serve_plain(app: axum::Router, dir: tempfile::TempDir) -> TestServer {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    TestServer {
        base: format!("http://{addr}"),
        client: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        _dir: dir,
    }
}

/// Build every configured feed, exactly as `main.rs` does.
async fn build_states(config: Config) -> Vec<AppState> {
    let storage = Arc::new(FilesystemStorage::new(config.storage_path()).await.unwrap());
    let db = Arc::new(
        SqliteDatabase::connect(&config.database_path())
            .await
            .unwrap(),
    );
    let config = Arc::new(config);
    let feeds = config.resolved_feeds().unwrap();
    let meta = Arc::new(
        feeds
            .iter()
            .map(FeedMeta::from_resolved)
            .collect::<Vec<_>>(),
    );
    let mut states = Vec::new();
    for f in &feeds {
        states.push(
            AppState::for_feed(storage.clone(), db.clone(), config.clone(), f, meta.clone())
                .await
                .unwrap(),
        );
    }
    states
}

/// A single root feed with the global push key.
async fn spawn_with(customize: impl FnOnce(&mut Config)) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        api_key: Some(API_KEY.to_string()),
        ..base_config(&dir)
    };
    customize(&mut config);
    let app = web::build_app(build_states(config).await);
    serve_plain(app, dir).await
}

/// Two or more `[[feeds]]`, mounted under their names.
async fn spawn_feeds(customize: impl FnOnce(&mut Config)) -> TestServer {
    spawn_with(customize).await
}

fn feed(name: &str) -> FeedConfig {
    FeedConfig {
        name: name.to_string(),
        ..FeedConfig::default()
    }
}

/// A minimal but valid `.nupkg`, with an embedded icon.
fn build_nupkg(id: &str, version: &str) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Test Author</authors>
    <description>A security test package.</description>
    <icon>icon.png</icon>
  </metadata>
</package>"#
    );
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts = SimpleFileOptions::default();
        zip.start_file(format!("{id}.nuspec"), opts).unwrap();
        zip.write_all(nuspec.as_bytes()).unwrap();
        zip.start_file("lib/net8.0/Lib.dll", opts).unwrap();
        zip.write_all(b"payload").unwrap();
        zip.start_file("icon.png", opts).unwrap();
        zip.write_all(b"\x89PNG\r\n\x1a\n-not-really-a-png")
            .unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

/// Push to the feed under `prefix` (`""` for the root feed).
async fn push(server: &TestServer, prefix: &str, key: &str, nupkg: Vec<u8>) -> StatusCode {
    server
        .client
        .put(server.url(&format!("{prefix}/api/v2/package")))
        .header("X-NuGet-ApiKey", key)
        .body(nupkg)
        .send()
        .await
        .unwrap()
        .status()
}

// ---------------------------------------------------------------------------
// Forwarding chains and the rate limiter (SEC-05, SEC-19)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_appended_forwarding_chain_is_throttled_by_the_real_client() {
    // The test connects from 127.0.0.1, the trusted proxy. nginx's
    // `$proxy_add_x_forwarded_for` appends the address it saw to whatever the
    // client sent, so the client controls the *left* of the chain.
    let server = spawn_with(|c| {
        c.trusted_proxies = vec!["127.0.0.1".into()];
        c.rate_limit.max_requests = 3;
    })
    .await;
    let get = |chain: String| {
        server
            .client
            .get(server.url("/v3/index.json"))
            .header("X-Forwarded-For", chain)
            .send()
    };
    // One real client, a fresh spoofed entry every time: one bucket.
    let mut statuses = Vec::new();
    for i in 0..5 {
        statuses.push(
            get(format!("198.51.100.{i}, 203.0.113.7"))
                .await
                .unwrap()
                .status(),
        );
    }
    assert_eq!(
        statuses,
        [
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::OK,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::TOO_MANY_REQUESTS
        ],
        "the client-chosen end of the chain picked the bucket"
    );
    // Another real client behind the same proxy has its own budget.
    let other = get("198.51.100.1, 203.0.113.8".into()).await.unwrap();
    assert_eq!(other.status(), StatusCode::OK);
}

#[tokio::test]
async fn failed_authentication_has_its_own_small_budget() {
    let server = spawn_with(|c| c.rate_limit.max_failed_auth = 3).await;
    let delete = |key: &'static str| {
        server
            .client
            .delete(server.url("/api/v2/package/Some.Pkg/1.0.0"))
            .header("X-NuGet-ApiKey", key)
            .send()
    };
    for _ in 0..3 {
        assert_eq!(
            delete("guess").await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    // Out of guesses: refused before the key is even compared, including the
    // right one, until the window rolls over.
    assert_eq!(
        delete("guess").await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        delete(API_KEY).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    // Requests without credentials are not guesses and are not refused.
    let open = server
        .client
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(open.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_challenge_without_credentials_is_not_a_failed_guess() {
    // A NuGet client asks unauthenticated first and sends its key only after
    // the 401 challenge; a restore does that once per package.
    let server = spawn_feeds(|c| {
        c.rate_limit.max_failed_auth = 2;
        c.feeds = vec![FeedConfig {
            read_api_key: Some("reader".into()),
            ..feed("gated")
        }];
    })
    .await;
    for _ in 0..5 {
        let resp = server
            .client
            .get(server.url("/gated/v3/search"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }
    let authed = server
        .client
        .get(server.url("/gated/v3/search"))
        .basic_auth("user", Some("reader"))
        .send()
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::OK);
}

/// Run the real binary with `env` and return its exit status and stderr,
/// killing it if it is still running after `timeout` (it then started, which
/// is what these tests assert does not happen).
fn run_binary(
    env: &[(&str, &str)],
    timeout: Duration,
) -> (Option<std::process::ExitStatus>, String) {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_yanuget"));
    for (k, _) in std::env::vars() {
        if k.starts_with("YANUGET_") {
            cmd.env_remove(k);
        }
    }
    cmd.env("YANUGET_DATA_DIR", dir.path())
        .env("YANUGET_PORT", "0")
        .env("YANUGET_HOST", "127.0.0.1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    (status, stderr)
}

#[test]
fn an_unparseable_environment_value_stops_the_server() {
    // `enabled` is not a boolean spelling; it used to be read as "off" and
    // serve plain HTTP.
    for (name, value) in [
        ("YANUGET_TLS_ENABLED", "enabled"),
        ("YANUGET_MAX_PACKAGE_SIZE_BYTES", "10G"),
        ("YANUGET_RATELIMIT_WINDOW_SECS", "0"),
    ] {
        let (status, stderr) = run_binary(&[(name, value)], Duration::from_secs(30));
        let status = status.unwrap_or_else(|| panic!("{name}={value}: the server started"));
        assert!(!status.success(), "{name}={value}: exited successfully");
        let expect = if name.ends_with("WINDOW_SECS") {
            "window_secs"
        } else {
            name
        };
        assert!(
            stderr.contains(expect),
            "{name}={value}: error does not name the setting: {stderr}"
        );
    }
}
