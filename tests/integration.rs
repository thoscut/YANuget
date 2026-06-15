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
    spawn_with(|_| {}).await
}

async fn spawn_with(customize: impl FnOnce(&mut Config)) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        api_key: Some(API_KEY.to_string()),
        host: Ipv4Addr::LOCALHOST.into(),
        port: 0,
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

// ---------------------------------------------------------------------------
// Symbol server
// ---------------------------------------------------------------------------

const METADATA_SIGNATURE: u32 = 0x424A_5342;

/// Build a minimal Portable PDB whose `#Pdb` stream starts with `guid`.
fn build_portable_pdb(guid: &[u8; 16]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&METADATA_SIGNATURE.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes()); // major
    buf.extend_from_slice(&1u16.to_le_bytes()); // minor
    buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
    let version = b"PDB v1.0\0\0\0\0";
    buf.extend_from_slice(&(version.len() as u32).to_le_bytes());
    buf.extend_from_slice(version);
    buf.extend_from_slice(&0u16.to_le_bytes()); // flags
    buf.extend_from_slice(&1u16.to_le_bytes()); // one stream
    let header_pos = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes()); // offset (patched)
    buf.extend_from_slice(&20u32.to_le_bytes()); // size
    buf.extend_from_slice(b"#Pdb\0\0\0\0");
    let stream_offset = buf.len() as u32;
    buf[header_pos..header_pos + 4].copy_from_slice(&stream_offset.to_le_bytes());
    buf.extend_from_slice(guid);
    buf.extend_from_slice(&[0u8; 4]);
    buf
}

/// Build a `.snupkg` (symbol package): a nuspec plus one `.pdb`.
fn build_snupkg(id: &str, version: &str, pdb_name: &str, pdb: &[u8]) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Test Author</authors>
    <description>Symbols for {id}.</description>
    <packageTypes><packageType name="SymbolsPackage" /></packageTypes>
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
        zip.start_file(format!("lib/net8.0/{pdb_name}"), opts)
            .unwrap();
        zip.write_all(pdb).unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

async fn push_symbol(server: &TestServer, key: &str, snupkg: Vec<u8>) -> reqwest::Response {
    server
        .client
        .put(server.url("/api/v2/symbol"))
        .header("X-NuGet-ApiKey", key)
        .header("Content-Type", "application/octet-stream")
        .body(snupkg)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn symbol_push_and_download_roundtrip() {
    let server = spawn().await;

    // The owning package must exist before symbols can be pushed.
    push_multipart(&server, API_KEY, build_nupkg("Sym.Lib", "1.0.0", b"dll")).await;

    let guid: [u8; 16] = [
        0xF6, 0x72, 0x7B, 0x49, 0x0A, 0x39, 0xFC, 0x44, 0x87, 0x8E, 0x5A, 0x2D, 0x63, 0xB6, 0xCC,
        0x4B,
    ];
    let pdb = build_portable_pdb(&guid);
    let key = yanuget::pdb::portable_pdb_signature(&pdb).expect("portable pdb key");
    let snupkg = build_snupkg("Sym.Lib", "1.0.0", "sym.lib.pdb", &pdb);

    let resp = push_symbol(&server, API_KEY, snupkg).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // The debugger fetches it over the SSQP path.
    let url = format!("/download/symbols/sym.lib.pdb/{key}/sym.lib.pdb");
    let resp = server.client.get(server.url(&url)).send().await.unwrap();
    assert!(resp.status().is_success(), "symbol download failed: {url}");
    let body = resp.bytes().await.unwrap();
    assert_eq!(&body[..], &pdb[..]);

    // An unknown key yields 404 so debuggers move to the next source.
    let miss = server
        .client
        .get(server.url("/download/symbols/sym.lib.pdb/DEADBEEFFFFFFFFF/sym.lib.pdb"))
        .send()
        .await
        .unwrap();
    assert_eq!(miss.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn symbol_push_requires_existing_package() {
    let server = spawn().await;
    let pdb = build_portable_pdb(&[1u8; 16]);
    let snupkg = build_snupkg("Ghost.Pkg", "9.9.9", "ghost.pdb", &pdb);
    let resp = push_symbol(&server, API_KEY, snupkg).await;
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn service_index_advertises_symbol_server() {
    let server = spawn().await;
    let body: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let types: Vec<&str> = body["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["@type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"SymbolServer/4.9.0"));
    assert!(types.contains(&"SymbolPackagePublish/4.9.0"));
}

// ---------------------------------------------------------------------------
// Web gallery
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gallery_lists_and_details_packages() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Web.Ui.Pkg", "1.2.3", b"x")).await;

    // Home page lists the package.
    let home = server.client.get(server.url("/")).send().await.unwrap();
    assert!(home.status().is_success());
    assert!(home
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .contains("text/html"));
    let html = home.text().await.unwrap();
    assert!(html.contains("Web.Ui.Pkg"));
    assert!(html.contains("/packages/web.ui.pkg"));

    // Detail page shows the Chocolatey install command (default primary client).
    let detail = server
        .client
        .get(server.url("/packages/web.ui.pkg"))
        .send()
        .await
        .unwrap();
    assert!(detail.status().is_success());
    let body = detail.text().await.unwrap();
    assert!(body.contains("choco install Web.Ui.Pkg --version 1.2.3"));
    assert!(body.contains("Newtonsoft.Json")); // dependency rendered

    // Unknown package detail is a 404.
    let missing = server
        .client
        .get(server.url("/packages/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn web_ui_can_be_disabled() {
    let server = spawn_with(|c| c.enable_web_ui = false).await;
    push_multipart(&server, API_KEY, build_nupkg("Hidden.Pkg", "1.0.0", b"x")).await;
    let resp = server
        .client
        .get(server.url("/packages/hidden.pkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
    // Root still serves the minimal fallback page.
    let root = server.client.get(server.url("/")).send().await.unwrap();
    assert!(root.status().is_success());
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prune_on_push_keeps_newest_versions() {
    let server = spawn_with(|c| {
        c.retention.enabled = true;
        c.retention.prune_on_push = true;
        c.retention.keep_latest_stable = Some(2);
    })
    .await;

    for v in ["1.0.0", "1.1.0", "1.2.0", "1.3.0"] {
        let resp = push_multipart(&server, API_KEY, build_nupkg("Keep.Me", v, b"x")).await;
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    }

    // Only the two newest stable versions remain.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/keep.me/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list: Vec<String> = versions["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(list, vec!["1.2.0".to_string(), "1.3.0".to_string()]);

    // The pruned payload is gone from storage too.
    let gone = server
        .client
        .get(server.url("/v3/package/keep.me/1.0.0/keep.me.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), reqwest::StatusCode::NOT_FOUND);
}
