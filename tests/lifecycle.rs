//! End-to-end tests of a version's life across feeds: who may claim an id,
//! what an overwrite keeps, and what retention may delete.

use std::io::{Cursor, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use yanuget::config::{Config, FeedConfig, OverwriteMode, RetentionConfig};
use yanuget::database::SqliteDatabase;
use yanuget::storage::FilesystemStorage;
use yanuget::web::{self, AppState, FeedMeta};
use zip::write::SimpleFileOptions;

const API_KEY: &str = "test-key";
const ADMIN_KEY: &str = "admin-key";

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

async fn spawn_feeds(customize: impl FnOnce(&mut Config)) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        host: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        tls_enabled: false,
        api_key: Some(API_KEY.into()),
        admin_api_key: Some(ADMIN_KEY.into()),
        ..Config::default()
    };
    customize(&mut config);

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
    let app = web::build_app(states);
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

fn feed(name: &str) -> FeedConfig {
    FeedConfig {
        name: name.into(),
        ..Default::default()
    }
}

fn build_nupkg(id: &str, version: &str, filler: &[u8]) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Test Author</authors>
    <description>A lifecycle test package.</description>
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
        zip.write_all(filler).unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

async fn push(server: &TestServer, prefix: &str, nupkg: Vec<u8>) -> reqwest::StatusCode {
    server
        .client
        .put(server.url(&format!("{prefix}/api/v2/package")))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(nupkg)
        .send()
        .await
        .unwrap()
        .status()
}

async fn admin(server: &TestServer, path: &str) -> reqwest::StatusCode {
    let csrf = yanuget::auth::AdminAuth::new(Some(ADMIN_KEY.into()))
        .csrf_token()
        .unwrap();
    server
        .client
        .post(server.url(path))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("_csrf={csrf}"))
        .send()
        .await
        .unwrap()
        .status()
}

/// The versions a NuGet client is offered from a feed's flat container.
async fn offered(server: &TestServer, prefix: &str, lower_id: &str) -> Vec<String> {
    let resp = server
        .client
        .get(server.url(&format!("{prefix}/v3/package/{lower_id}/index.json")))
        .send()
        .await
        .unwrap();
    if !resp.status().is_success() {
        return Vec::new();
    }
    let body: serde_json::Value = resp.json().await.unwrap();
    body["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_reserved_prefix_belongs_to_its_feed_alone() {
    let server = spawn_feeds(|c| {
        let mut internal = feed("internal");
        internal.reserved_id_prefixes = vec!["Contoso.".into()];
        c.feeds = vec![internal, feed("public")];
    })
    .await;
    let package = build_nupkg("Contoso.Utils", "1.0.0", b"ours");

    // Another feed cannot claim the name first...
    assert_eq!(
        push(&server, "/public", package.clone()).await,
        reqwest::StatusCode::FORBIDDEN
    );
    assert_eq!(
        push(&server, "/public", build_nupkg("contoso", "1.0.0", b"x")).await,
        reqwest::StatusCode::FORBIDDEN
    );
    // ...so the owner's push is not refused as a conflict.
    assert_eq!(
        push(&server, "/internal", package.clone()).await,
        reqwest::StatusCode::CREATED
    );
    assert_eq!(
        push(&server, "/public", package).await,
        reqwest::StatusCode::FORBIDDEN
    );
    // Other ids are unaffected.
    assert_eq!(
        push(
            &server,
            "/public",
            build_nupkg("ContosoFan.Utils", "1.0.0", b"x")
        )
        .await,
        reqwest::StatusCode::CREATED
    );
}

#[tokio::test]
async fn different_bytes_under_a_version_another_feed_holds_conflict() {
    let server = spawn_feeds(|c| c.feeds = vec![feed("one"), feed("two")]).await;
    assert_eq!(
        push(
            &server,
            "/one",
            build_nupkg("Shared.Pkg", "1.0.0", b"first")
        )
        .await,
        reqwest::StatusCode::CREATED
    );
    assert_eq!(
        push(
            &server,
            "/two",
            build_nupkg("Shared.Pkg", "1.0.0", b"second")
        )
        .await,
        reqwest::StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn an_overwrite_does_not_undo_a_disable() {
    let server = spawn_feeds(|c| {
        let mut dev = feed("dev");
        dev.allow_overwrite = Some(OverwriteMode::Enabled);
        c.feeds = vec![dev];
    })
    .await;
    for v in ["1.0.0", "2.0.0"] {
        assert_eq!(
            push(&server, "/dev", build_nupkg("Broken.Pkg", v, b"first")).await,
            reqwest::StatusCode::CREATED
        );
    }
    assert_eq!(
        admin(&server, "/dev/admin/packages/broken.pkg/2.0.0/disable").await,
        reqwest::StatusCode::SEE_OTHER
    );
    assert_eq!(offered(&server, "/dev", "broken.pkg").await, ["1.0.0"]);

    // A rebuild pushed over the disabled version stays disabled.
    assert_eq!(
        push(
            &server,
            "/dev",
            build_nupkg("Broken.Pkg", "2.0.0", b"rebuilt")
        )
        .await,
        reqwest::StatusCode::CREATED
    );
    assert_eq!(offered(&server, "/dev", "broken.pkg").await, ["1.0.0"]);
}

#[tokio::test]
async fn pending_builds_do_not_prune_the_approved_version() {
    let server = spawn_feeds(|c| {
        let mut gated = feed("gated");
        gated.requires_approval = true;
        gated.retention = Some(RetentionConfig {
            enabled: true,
            prune_on_push: true,
            keep_latest_stable: Some(1),
            ..Default::default()
        });
        c.feeds = vec![gated];
    })
    .await;
    assert_eq!(
        push(&server, "/gated", build_nupkg("Ring.Pkg", "1.0.0", b"v1")).await,
        reqwest::StatusCode::CREATED
    );
    assert_eq!(
        admin(&server, "/gated/admin/packages/ring.pkg/1.0.0/approve").await,
        reqwest::StatusCode::SEE_OTHER
    );
    // Builds awaiting approval, each pushed with pruning on.
    for v in ["2.0.0", "3.0.0"] {
        assert_eq!(
            push(&server, "/gated", build_nupkg("Ring.Pkg", v, v.as_bytes())).await,
            reqwest::StatusCode::CREATED
        );
    }
    assert_eq!(offered(&server, "/gated", "ring.pkg").await, ["1.0.0"]);

    // Approving one makes it the newest servable version, and the next
    // push with pruning on may take the old one.
    assert_eq!(
        admin(&server, "/gated/admin/packages/ring.pkg/3.0.0/approve").await,
        reqwest::StatusCode::SEE_OTHER
    );
    assert_eq!(
        push(&server, "/gated", build_nupkg("Ring.Pkg", "4.0.0", b"v4")).await,
        reqwest::StatusCode::CREATED
    );
    assert_eq!(offered(&server, "/gated", "ring.pkg").await, ["3.0.0"]);
}
