//! End-to-end HTTP tests exercising the real router over a TCP socket with a
//! real `reqwest` client, a filesystem store and an on-disk SQLite database.

use std::io::{Cursor, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use yanuget::config::Config;
use yanuget::database::SqliteDatabase;
use yanuget::storage::FilesystemStorage;
use yanuget::web::{self, AppState};
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

async fn spawn() -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        api_key: Some(API_KEY.to_string()),
        host: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        ..Config::default()
    };

    let storage = Arc::new(FilesystemStorage::new(config.storage_path()).await.unwrap());
    let db = Arc::new(
        SqliteDatabase::connect(&config.database_path())
            .await
            .unwrap(),
    );
    let config = Arc::new(config);
    let state = AppState::new(storage, db, config).await.unwrap();
    let app = web::router(state);

    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestServer {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
        _dir: dir,
    }
}

/// Build a minimal but valid `.nupkg` in memory.
fn build_nupkg(id: &str, version: &str, payload_filler: &[u8]) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Test Author</authors>
    <description>An integration test package for {id}.</description>
    <tags>integration test</tags>
    <dependencies>
      <group targetFramework="net8.0">
        <dependency id="Newtonsoft.Json" version="[13.0.1, )" />
      </group>
    </dependencies>
  </metadata>
</package>"#
    );

    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(format!("{id}.nuspec"), opts).unwrap();
        zip.write_all(nuspec.as_bytes()).unwrap();
        zip.start_file("lib/net8.0/Lib.dll", opts).unwrap();
        zip.write_all(payload_filler).unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

async fn push_multipart(server: &TestServer, key: &str, nupkg: Vec<u8>) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(nupkg)
        .file_name("package.nupkg")
        .mime_str("application/octet-stream")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("package", part);
    server
        .client
        .put(server.url("/api/v2/package"))
        .header("X-NuGet-ApiKey", key)
        .multipart(form)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn service_index_is_served() {
    let server = spawn().await;
    let resp = server
        .client
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["version"], "3.0.0");
    let types: Vec<&str> = body["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["@type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"PackagePublish/2.0.0"));
    assert!(types.contains(&"SearchQueryService"));
}

#[tokio::test]
async fn push_requires_api_key() {
    let server = spawn().await;
    let nupkg = build_nupkg("Auth.Test", "1.0.0", b"x");
    let resp = push_multipart(&server, "wrong-key", nupkg).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn full_publish_and_consume_flow() {
    let server = spawn().await;
    let payload = vec![0xABu8; 64 * 1024];
    let nupkg = build_nupkg("Contoso.Utils", "1.2.3", &payload);
    let expected_len = nupkg.len();

    // Push.
    let resp = push_multipart(&server, API_KEY, nupkg.clone()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // Duplicate push is rejected.
    let dup = push_multipart(&server, API_KEY, nupkg.clone()).await;
    assert_eq!(dup.status(), reqwest::StatusCode::CONFLICT);

    // Flat-container version list.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/contoso.utils/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"][0], "1.2.3");

    // Registration index with dependency groups.
    let reg: serde_json::Value = server
        .client
        .get(server.url("/v3/registration/contoso.utils/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = &reg["items"][0]["items"][0]["catalogEntry"];
    assert_eq!(entry["id"], "Contoso.Utils");
    assert_eq!(entry["version"], "1.2.3");
    assert_eq!(
        entry["dependencyGroups"][0]["dependencies"][0]["id"],
        "Newtonsoft.Json"
    );

    // Search finds it.
    let search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=contoso"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search["totalHits"], 1);
    assert_eq!(search["data"][0]["id"], "Contoso.Utils");

    // Download returns the exact bytes that were pushed.
    let download = server
        .client
        .get(server.url("/v3/package/contoso.utils/1.2.3/contoso.utils.1.2.3.nupkg"))
        .send()
        .await
        .unwrap();
    assert!(download.status().is_success());
    assert_eq!(download.headers()["accept-ranges"], "bytes");
    let downloaded = download.bytes().await.unwrap();
    assert_eq!(downloaded.len(), expected_len);
    assert_eq!(&downloaded[..], &nupkg[..]);
}

#[tokio::test]
async fn range_request_returns_partial_content() {
    let server = spawn().await;
    let nupkg = build_nupkg("Range.Test", "1.0.0", &[7u8; 8192]);
    push_multipart(&server, API_KEY, nupkg.clone()).await;

    let resp = server
        .client
        .get(server.url("/v3/package/range.test/1.0.0/range.test.1.0.0.nupkg"))
        .header("Range", "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers()["content-length"], "10");
    let content_range = resp.headers()["content-range"].to_str().unwrap();
    assert!(content_range.starts_with("bytes 0-9/"));
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 10);
    assert_eq!(&body[..], &nupkg[..10]);
}

#[tokio::test]
async fn raw_body_push_is_supported() {
    // Some clients PUT the raw .nupkg instead of multipart form data.
    let server = spawn().await;
    let nupkg = build_nupkg("Raw.Push", "2.0.0", b"raw");
    let resp = server
        .client
        .put(server.url("/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .header("Content-Type", "application/octet-stream")
        .body(nupkg)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/raw.push/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"][0], "2.0.0");
}

#[tokio::test]
async fn delete_unlists_package() {
    let server = spawn().await;
    let nupkg = build_nupkg("Unlist.Me", "1.0.0", b"data");
    push_multipart(&server, API_KEY, nupkg).await;

    // Unlist (default delete behaviour).
    let resp = server
        .client
        .delete(server.url("/api/v2/package/unlist.me/1.0.0"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    // No longer in search...
    let search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=unlist"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search["totalHits"], 0);

    // ...but still downloadable by exact version (NuGet restore semantics).
    let download = server
        .client
        .get(server.url("/v3/package/unlist.me/1.0.0/unlist.me.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert!(download.status().is_success());

    // Relist restores it.
    let resp = server
        .client
        .post(server.url("/api/v2/package/unlist.me/1.0.0"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=unlist"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search["totalHits"], 1);
}

#[tokio::test]
async fn nuspec_endpoint_serves_manifest() {
    let server = spawn().await;
    let nupkg = build_nupkg("Manifest.Test", "1.0.0", b"data");
    push_multipart(&server, API_KEY, nupkg).await;

    let resp = server
        .client
        .get(server.url("/v3/package/manifest.test/1.0.0/manifest.test.nuspec"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .contains("xml"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("<id>Manifest.Test</id>"));
}

#[tokio::test]
async fn missing_package_returns_404() {
    let server = spawn().await;
    let resp = server
        .client
        .get(server.url("/v3/package/does.not.exist/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}
