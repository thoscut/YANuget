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

// ---------------------------------------------------------------------------
// Per-feed keys (TEST-01, SEC-18)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_feeds_push_key_does_not_open_another() {
    let server = spawn_feeds(|c| {
        c.feeds = vec![
            FeedConfig {
                api_key: Some("key-a".into()),
                ..feed("a")
            },
            FeedConfig {
                api_keys: vec!["key-b".into()],
                ..feed("b")
            },
            // No key of its own: the global one applies.
            feed("c"),
        ];
    })
    .await;
    let pkg = || build_nupkg("Iso.Pkg", "1.0.0");

    assert_eq!(
        push(&server, "/a", "key-b", pkg()).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        push(&server, "/b", "key-a", pkg()).await,
        StatusCode::UNAUTHORIZED
    );
    // A feed with a key of its own no longer takes the global one.
    assert_eq!(
        push(&server, "/a", API_KEY, pkg()).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        push(&server, "/b", API_KEY, pkg()).await,
        StatusCode::UNAUTHORIZED
    );
    // And a feed that falls back to the global key takes no other feed's.
    assert_eq!(
        push(&server, "/c", "key-a", pkg()).await,
        StatusCode::UNAUTHORIZED
    );

    assert_eq!(
        push(&server, "/a", "key-a", pkg()).await,
        StatusCode::CREATED
    );
    assert_eq!(
        push(&server, "/b", "key-b", pkg()).await,
        StatusCode::CREATED
    );
    assert_eq!(
        push(&server, "/c", API_KEY, pkg()).await,
        StatusCode::CREATED
    );

    // Deleting is a push-key operation too.
    let del = |prefix: &str, key: &str| {
        server
            .client
            .delete(server.url(&format!("{prefix}/api/v2/package/Iso.Pkg/1.0.0")))
            .header("X-NuGet-ApiKey", key.to_string())
            .send()
    };
    assert_eq!(
        del("/b", "key-a").await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        del("/b", "key-b").await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn one_feeds_admin_key_does_not_open_another_admin_area() {
    let server = spawn_feeds(|c| {
        c.feeds = vec![
            FeedConfig {
                admin_api_key: Some("admin-a".into()),
                ..feed("a")
            },
            FeedConfig {
                admin_api_key: Some("admin-b".into()),
                ..feed("b")
            },
        ];
    })
    .await;
    assert_eq!(
        push(&server, "/b", API_KEY, build_nupkg("Adm.Pkg", "1.0.0")).await,
        StatusCode::CREATED
    );

    let admin = |path: &str, key: &str| {
        server
            .client
            .get(server.url(path))
            .basic_auth("admin", Some(key.to_string()))
            .send()
    };
    let wrong = admin("/b/admin", "admin-a").await.unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.headers()["cache-control"], "no-store");
    let wrong = admin("/b/admin/packages/adm.pkg", "admin-a").await.unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    // The sign-in page asks for the key the admin area actually wants.
    let page = wrong.text().await.unwrap();
    assert!(page.contains("admin key"), "{page}");
    assert!(!page.contains("for reading"), "{page}");

    let right = admin("/b/admin/packages/adm.pkg", "admin-b").await.unwrap();
    assert_eq!(right.status(), StatusCode::OK);
    // The page embeds a CSRF token: it must not be kept by any cache.
    assert_eq!(right.headers()["cache-control"], "no-store");

    // Feed A's CSRF token, with feed B's credentials, changes nothing.
    let token_a = yanuget::auth::AdminAuth::new(Some("admin-a".into()))
        .csrf_token()
        .unwrap();
    let token_b = yanuget::auth::AdminAuth::new(Some("admin-b".into()))
        .csrf_token()
        .unwrap();
    let disable = |token: String| {
        server
            .client
            .post(server.url("/b/admin/packages/adm.pkg/1.0.0/disable"))
            .basic_auth("admin", Some("admin-b"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("_csrf={token}"))
            .send()
    };
    assert_eq!(
        disable(token_a).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        disable(token_b).await.unwrap().status(),
        StatusCode::SEE_OTHER
    );
}

// ---------------------------------------------------------------------------
// Host validation, CORS and cross-site writes (SEC-13)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_request_for_another_host_is_misdirected() {
    let server = spawn_with(|c| {
        c.base_url = Some("https://feed.example.com".into());
        c.allowed_hosts = vec!["alias.example.com".into()];
    })
    .await;
    let get = |path: &'static str, host: Option<&'static str>| {
        let mut req = server.client.get(server.url(path));
        if let Some(host) = host {
            req = req.header("Host", host);
        }
        req.send()
    };
    // What a DNS-rebinding page sends: its own name, or the raw address.
    let rebound = get("/v3/index.json", Some("evil.example.net"))
        .await
        .unwrap();
    assert_eq!(rebound.status(), StatusCode::MISDIRECTED_REQUEST);
    let by_address = get("/v3/search", None).await.unwrap();
    assert_eq!(by_address.status(), StatusCode::MISDIRECTED_REQUEST);
    // Writes are refused the same way, before any handler runs.
    let push = server
        .client
        .put(server.url("/api/v2/package"))
        .header("Host", "evil.example.net")
        .body(build_nupkg("Rebind.Pkg", "1.0.0"))
        .send()
        .await
        .unwrap();
    assert_eq!(push.status(), StatusCode::MISDIRECTED_REQUEST);

    // The configured names work, with or without a port or in another case.
    for host in [
        "feed.example.com",
        "FEED.example.com:443",
        "alias.example.com",
    ] {
        let ok = server
            .client
            .get(server.url("/v3/index.json"))
            .header("Host", host)
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK, "{host}");
    }
    // Probes use an address, and must keep working.
    for path in ["/health", "/health/live", "/health/ready"] {
        assert_eq!(
            get(path, None).await.unwrap().status(),
            StatusCode::OK,
            "{path}"
        );
    }
}

#[tokio::test]
async fn cross_origin_access_is_read_only() {
    let server = spawn_with(|c| c.cors_allowed_origins = vec!["*".into()]).await;
    let preflight = |method: &'static str| {
        server
            .client
            .request(
                reqwest::Method::OPTIONS,
                server.url("/api/v2/package/X/1.0.0"),
            )
            .header("Origin", "https://page.example")
            .header("Access-Control-Request-Method", method)
            .send()
    };
    let get = preflight("GET").await.unwrap();
    let allowed = header(&get, "access-control-allow-methods").to_string();
    assert!(allowed.contains("GET"), "{allowed}");
    for write in ["DELETE", "PUT", "POST", "PATCH"] {
        assert!(
            !allowed.contains(write),
            "{write} allowed cross-origin: {allowed}"
        );
    }
}

#[tokio::test]
async fn a_browser_write_from_another_site_is_refused() {
    // No API key: the default, and the case where a page could otherwise
    // change the feed through a visitor's browser.
    let server = spawn_with(|c| c.api_key = None).await;
    assert_eq!(
        push(&server, "", "", build_nupkg("Site.Pkg", "1.0.0")).await,
        StatusCode::CREATED
    );
    // Relisting is a CORS-simple POST: no preflight protects it.
    let relist = |site: &'static str| {
        server
            .client
            .post(server.url("/api/v2/package/Site.Pkg/1.0.0"))
            .header("Sec-Fetch-Site", site)
            .send()
    };
    assert_eq!(
        relist("cross-site").await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        relist("same-origin").await.unwrap().status(),
        StatusCode::OK
    );
    // Reads from anywhere are still reads.
    let read = server
        .client
        .get(server.url("/v3/index.json"))
        .header("Sec-Fetch-Site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_plain_index_page_escapes_the_host() {
    let server = spawn_with(|c| c.enable_web_ui = false).await;
    let resp = server
        .client
        .get(server.url("/"))
        .header("Host", "h\"><b>x</b>")
        .send()
        .await
        .unwrap();
    let html = resp.text().await.unwrap();
    assert!(!html.contains("<b>x</b>"), "{html}");
    assert!(html.contains("&lt;b&gt;"), "{html}");
}

#[tokio::test]
async fn the_feed_index_does_not_name_gated_feeds() {
    let server = spawn_feeds(|c| {
        c.feeds = vec![
            feed("public-feed"),
            FeedConfig {
                read_api_key: Some("reader".into()),
                ..feed("secret-project")
            },
        ];
    })
    .await;
    let html = server
        .client
        .get(server.url("/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("public-feed"));
    assert!(!html.contains("secret-project"), "{html}");
    assert!(html.contains("require credentials"));
}

// ---------------------------------------------------------------------------
// Ids in URLs (SEC-22)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_id_no_package_could_have_reaches_nothing() {
    let server = spawn_with(|c| c.hard_delete_enabled = true).await;
    assert_eq!(
        push(&server, "", API_KEY, build_nupkg("Kit.Pkg", "1.0.0")).await,
        StatusCode::CREATED
    );
    // U+212A KELVIN SIGN lower-cases to `k` in Unicode but not in ASCII, so it
    // used to match the database row and miss the directory and the lock.
    let kelvin = "%E2%84%AAit.Pkg";
    let del = server
        .client
        .delete(server.url(&format!("/api/v2/package/{kelvin}/1.0.0")))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::NOT_FOUND);
    for path in [
        format!("/v3/package/{kelvin}/index.json"),
        format!("/v3/package/{kelvin}/1.0.0/kit.pkg.1.0.0.nupkg"),
        format!("/v3/registration/{kelvin}/index.json"),
        format!("/packages/{kelvin}"),
        "/v3/package/..%2F..%2Fetc/index.json".to_string(),
    ] {
        let resp = server.client.get(server.url(&path)).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{path}");
    }
    // The real package is untouched.
    let ok = server
        .client
        .get(server.url("/v3/package/kit.pkg/1.0.0/kit.pkg.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Caching (SEC-04, COR-12, SEC-25, COR-25)
// ---------------------------------------------------------------------------

fn header<'a>(resp: &'a reqwest::Response, name: &str) -> &'a str {
    resp.headers()
        .get(name)
        .unwrap_or_else(|| panic!("no {name} header"))
        .to_str()
        .unwrap()
}

fn vary(resp: &reqwest::Response) -> String {
    resp.headers()
        .get_all("vary")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect::<Vec<_>>()
        .join(", ")
}

#[tokio::test]
async fn gated_content_is_never_shared_by_a_cache() {
    let server = spawn_feeds(|c| {
        c.feeds = vec![FeedConfig {
            read_api_key: Some("reader".into()),
            ..feed("g")
        }];
    })
    .await;
    assert_eq!(
        push(&server, "/g", API_KEY, build_nupkg("Gated.Pkg", "1.0.0")).await,
        StatusCode::CREATED
    );
    let get = |path: &str| {
        server
            .client
            .get(server.url(path))
            .basic_auth("dotnet", Some("reader"))
            .send()
    };

    let nupkg = get("/g/v3/package/gated.pkg/1.0.0/gated.pkg.1.0.0.nupkg")
        .await
        .unwrap();
    assert_eq!(nupkg.status(), StatusCode::OK);
    assert!(header(&nupkg, "cache-control").starts_with("private"));
    let v = vary(&nupkg);
    assert!(
        v.contains("Authorization") && v.contains("X-NuGet-ApiKey"),
        "{v}"
    );

    // The manifest and the icon get validators now, and the same privacy.
    for path in [
        "/g/v3/package/gated.pkg/1.0.0/gated.pkg.nuspec",
        "/g/packages/gated.pkg/1.0.0/icon",
    ] {
        let first = get(path).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK, "{path}");
        assert!(
            header(&first, "cache-control").starts_with("private"),
            "{path}"
        );
        let etag = header(&first, "etag").to_string();
        let again = server
            .client
            .get(server.url(path))
            .basic_auth("dotnet", Some("reader"))
            .header("If-None-Match", &etag)
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED, "{path}");
    }

    // Protocol JSON has no caching of its own; on a gated feed it is private.
    let reg = get("/g/v3/registration/gated.pkg/index.json")
        .await
        .unwrap();
    assert_eq!(reg.status(), StatusCode::OK);
    assert_eq!(header(&reg, "cache-control"), "private");
    assert!(vary(&reg).contains("Authorization"));
    // And the security layer's own Vary survives next to it.
    assert!(vary(&reg).contains("X-Forwarded-Host"));
}

#[tokio::test]
async fn immutable_only_while_versions_cannot_be_overwritten() {
    let url = "/v3/package/over.pkg/1.0.0/over.pkg.1.0.0.nupkg";

    let fixed = spawn_with(|_| {}).await;
    push(&fixed, "", API_KEY, build_nupkg("Over.Pkg", "1.0.0")).await;
    let resp = fixed.client.get(fixed.url(url)).send().await.unwrap();
    assert_eq!(
        header(&resp, "cache-control"),
        "public, max-age=31536000, immutable"
    );

    let overwritable = spawn_with(|c| {
        c.allow_overwrite = yanuget::config::OverwriteMode::Enabled;
    })
    .await;
    push(&overwritable, "", API_KEY, build_nupkg("Over.Pkg", "1.0.0")).await;
    let resp = overwritable
        .client
        .get(overwritable.url(url))
        .send()
        .await
        .unwrap();
    // Revalidate every time; the ETag makes that a cheap 304.
    assert_eq!(header(&resp, "cache-control"), "public, no-cache");
    let etag = header(&resp, "etag").to_string();
    let again = overwritable
        .client
        .get(overwritable.url(url))
        .header("If-None-Match", etag)
        .send()
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn a_not_modified_answer_is_not_a_download() {
    let server = spawn_with(|_| {}).await;
    push(&server, "", API_KEY, build_nupkg("Count.Pkg", "1.0.0")).await;
    let url = server.url("/v3/package/count.pkg/1.0.0/count.pkg.1.0.0.nupkg");
    let first = server.client.get(&url).send().await.unwrap();
    let etag = header(&first, "etag").to_string();
    for _ in 0..3 {
        let again = server
            .client
            .get(&url)
            .header("If-None-Match", &etag)
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
    }
    // Counted off the request path: give it a moment, then expect one.
    let mut downloads = serde_json::Value::Null;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        let search: serde_json::Value = server
            .client
            .get(server.url("/v3/search?q=count.pkg"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        downloads = search["data"][0]["totalDownloads"].clone();
        if downloads == 1 {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=count.pkg"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(downloads, 1);
    assert_eq!(search["data"][0]["totalDownloads"], 1, "a 304 was counted");
}

#[tokio::test]
async fn an_invalid_range_is_ignored_rather_than_refused() {
    let server = spawn_with(|_| {}).await;
    push(&server, "", API_KEY, build_nupkg("Range.Pkg", "1.0.0")).await;
    let url = server.url("/v3/package/range.pkg/1.0.0/range.pkg.1.0.0.nupkg");
    let whole = server.client.get(&url).send().await.unwrap();
    let len = whole.bytes().await.unwrap().len();
    for range in ["bytes=5-3", "bytes=+0-1"] {
        let resp = server
            .client
            .get(&url)
            .header("Range", range)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{range}");
        assert_eq!(resp.bytes().await.unwrap().len(), len, "{range}");
    }
    // A well-formed range past the end is still a 416.
    let past = server
        .client
        .get(&url)
        .header("Range", "bytes=999999-")
        .send()
        .await
        .unwrap();
    assert_eq!(past.status(), StatusCode::RANGE_NOT_SATISFIABLE);
}

// ---------------------------------------------------------------------------
// Copy and promote (COR-16)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn promotion_does_not_republish_what_the_source_withholds() {
    let server = spawn_feeds(|c| {
        c.admin_api_key = Some("admin".into());
        c.feeds = vec![
            FeedConfig {
                promotes_to: Some("stable".into()),
                ..feed("dev")
            },
            feed("stable"),
        ];
    })
    .await;
    for id in ["Unlisted.Pkg", "Disabled.Pkg"] {
        assert_eq!(
            push(&server, "/dev", API_KEY, build_nupkg(id, "1.0.0")).await,
            StatusCode::CREATED
        );
    }
    // Unlist one (the client's "delete"), disable the other.
    let unlist = server
        .client
        .delete(server.url("/dev/api/v2/package/Unlisted.Pkg/1.0.0"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(unlist.status(), StatusCode::NO_CONTENT);
    let csrf = yanuget::auth::AdminAuth::new(Some("admin".into()))
        .csrf_token()
        .unwrap();
    let admin_post = |path: String| {
        server
            .client
            .post(server.url(&path))
            .basic_auth("admin", Some("admin"))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("_csrf={csrf}"))
            .send()
    };
    let disable = admin_post("/dev/admin/packages/disabled.pkg/1.0.0/disable".into())
        .await
        .unwrap();
    assert_eq!(disable.status(), StatusCode::SEE_OTHER);
    for id in ["unlisted.pkg", "disabled.pkg"] {
        let promote = admin_post(format!("/dev/admin/packages/{id}/1.0.0/promote"))
            .await
            .unwrap();
        assert_eq!(promote.status(), StatusCode::SEE_OTHER, "{id}");
    }

    // The disabled version stays withheld in the next ring.
    let disabled = server
        .client
        .get(server.url("/stable/v3/package/disabled.pkg/1.0.0/disabled.pkg.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(disabled.status(), StatusCode::NOT_FOUND);
    // The unlisted one stays restorable, and out of search.
    let unlisted = server
        .client
        .get(server.url("/stable/v3/package/unlisted.pkg/1.0.0/unlisted.pkg.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(unlisted.status(), StatusCode::OK);
    let search: serde_json::Value = server
        .client
        .get(server.url("/stable/v3/search?q=unlisted.pkg"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search["totalHits"], 0, "{search}");
}

// ---------------------------------------------------------------------------
// The real serve path: TLS, timeouts, the connection cap, shutdown
// (TEST-06, SEC-23)
// ---------------------------------------------------------------------------

struct Served {
    addr: SocketAddr,
    handle: axum_server::Handle,
    task: tokio::task::JoinHandle<yanuget::Result<()>>,
    dir: tempfile::TempDir,
}

/// Serve through `yanuget::server::serve`, as `main.rs` does.
async fn serve_real(
    customize: impl FnOnce(&mut Config),
    limits: impl FnOnce(&mut yanuget::server::ServeLimits),
) -> Served {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        api_key: Some(API_KEY.to_string()),
        ..base_config(&dir)
    };
    customize(&mut config);
    let mut serve_limits = yanuget::server::ServeLimits::from_config(&config);
    limits(&mut serve_limits);
    let app = web::build_app(build_states(config.clone()).await);
    let handle = axum_server::Handle::new();
    let task = tokio::spawn({
        let handle = handle.clone();
        async move { yanuget::server::serve(app, &config, handle, serve_limits).await }
    });
    let addr = tokio::time::timeout(Duration::from_secs(30), handle.listening())
        .await
        .expect("server did not start")
        .expect("server did not bind");
    Served {
        addr,
        handle,
        task,
        dir,
    }
}

#[tokio::test]
async fn tls_serves_hsts_and_keeps_its_generated_key_private() {
    let served = serve_real(|c| c.tls_enabled = true, |_| {}).await;
    let tls = served.dir.path().join("tls");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(tls.join("key.pem"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "key mode {:o}", mode & 0o777);
    }

    // Trust exactly the generated certificate, and reach it by the name it
    // was issued for.
    let cert =
        reqwest::Certificate::from_pem(&std::fs::read(tls.join("cert.pem")).unwrap()).unwrap();
    let client = reqwest::Client::builder()
        .add_root_certificate(cert)
        .resolve("localhost", served.addr)
        .build()
        .unwrap();
    let port = served.addr.port();
    let resp = client
        .get(format!("https://localhost:{port}/v3/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        header(&resp, "strict-transport-security"),
        "max-age=31536000"
    );
    let index = resp.text().await.unwrap();
    assert!(
        index.contains(&format!("https://localhost:{port}/")),
        "URLs keep the https scheme: {index}"
    );
    served.handle.shutdown();
}

#[tokio::test]
async fn a_client_that_trickles_its_headers_is_cut_off() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let served = serve_real(
        |_| {},
        |l| l.header_read_timeout = Duration::from_millis(300),
    )
    .await;
    let mut conn = tokio::net::TcpStream::connect(served.addr).await.unwrap();
    conn.write_all(b"GET /v3/index.json HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    // Never finish the headers. The server must give up, not wait forever.
    let mut buf = Vec::new();
    let read = tokio::time::timeout(Duration::from_secs(10), conn.read_to_end(&mut buf)).await;
    assert!(read.is_ok(), "the connection was still open after 10 s");
    served.handle.shutdown();
}

#[tokio::test]
async fn connections_over_the_cap_are_turned_away() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let served = serve_real(|_| {}, |l| l.max_connections = 1).await;
    let first = tokio::net::TcpStream::connect(served.addr).await.unwrap();
    // Give the server a moment to accept the first and take its slot.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut second = tokio::net::TcpStream::connect(served.addr).await.unwrap();
    let mut buf = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(5), second.read_to_end(&mut buf)).await;
    assert!(
        matches!(closed, Ok(Ok(0)) | Ok(Err(_))),
        "the second connection was served: {closed:?}"
    );

    // Closing the first frees its slot.
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut third = tokio::net::TcpStream::connect(served.addr).await.unwrap();
    third
        .write_all(b"GET /health/live HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut reply = String::new();
    tokio::time::timeout(Duration::from_secs(5), third.read_to_string(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    served.handle.shutdown();
}

#[tokio::test]
async fn plain_http_shutdown_has_a_deadline() {
    use tokio::io::AsyncWriteExt;
    // A client in the middle of a request, with nothing to cut it off but
    // the shutdown deadline.
    let served = serve_real(
        |_| {},
        |l| l.header_read_timeout = Duration::from_secs(3600),
    )
    .await;
    let mut conn = tokio::net::TcpStream::connect(served.addr).await.unwrap();
    conn.write_all(b"GET /v3/index.json HTTP/1.1\r\nHost: x\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    served
        .handle
        .graceful_shutdown(Some(Duration::from_millis(500)));
    let stopped = tokio::time::timeout(Duration::from_secs(10), served.task).await;
    assert!(stopped.is_ok(), "shutdown waited on the connection forever");
    drop(conn);
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
