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
        // The harness serves plain HTTP via axum::serve, so reflect that in the
        // config (otherwise generated URLs would default to the https scheme).
        tls_enabled: false,
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
async fn push_accepts_trailing_slash() {
    // The NuGet client appends a trailing slash to the publish endpoint, so it
    // PUTs to `/api/v2/package/` rather than `/api/v2/package`. Both must work.
    let server = spawn().await;
    let nupkg = build_nupkg("Trailing.Slash", "1.0.0", b"slash");
    let part = reqwest::multipart::Part::bytes(nupkg)
        .file_name("package.nupkg")
        .mime_str("application/octet-stream")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("package", part);
    let resp = server
        .client
        .put(server.url("/api/v2/package/"))
        .header("X-NuGet-ApiKey", API_KEY)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/trailing.slash/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"][0], "1.0.0");
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

#[tokio::test]
async fn registration_leaf_serves_single_version() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Leaf.Pkg", "1.2.3", b"x")).await;
    let leaf: serde_json::Value = server
        .client
        .get(server.url("/v3/registration/leaf.pkg/1.2.3.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(leaf["catalogEntry"]["id"], "Leaf.Pkg");
    assert_eq!(leaf["catalogEntry"]["version"], "1.2.3");
}

#[tokio::test]
async fn autocomplete_ids_and_versions() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Auto.Cli", "1.0.0", b"x")).await;
    push_multipart(&server, API_KEY, build_nupkg("Auto.Cli", "1.1.0", b"x")).await;
    push_multipart(&server, API_KEY, build_nupkg("Auto.Core", "1.0.0", b"x")).await;

    // Id autocomplete.
    let ids: serde_json::Value = server
        .client
        .get(server.url("/v3/autocomplete?q=auto"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let data = ids["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);

    // Version enumeration for one id.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/autocomplete?id=auto.cli"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let vs: Vec<&str> = versions["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(vs, vec!["1.0.0", "1.1.0"]);
}

#[tokio::test]
async fn overwrite_allows_republish() {
    let server = spawn_with(|c| c.allow_overwrite = true).await;
    let nupkg = build_nupkg("Over.Write", "1.0.0", b"x");
    assert_eq!(
        push_multipart(&server, API_KEY, nupkg.clone())
            .await
            .status(),
        reqwest::StatusCode::CREATED
    );
    // Re-pushing the same version succeeds instead of conflicting.
    assert_eq!(
        push_multipart(&server, API_KEY, nupkg).await.status(),
        reqwest::StatusCode::CREATED
    );
}

#[tokio::test]
async fn hard_delete_removes_payload() {
    let server = spawn_with(|c| c.hard_delete_enabled = true).await;
    push_multipart(&server, API_KEY, build_nupkg("Hard.Del", "1.0.0", b"x")).await;
    let resp = server
        .client
        .delete(server.url("/api/v2/package/hard.del/1.0.0"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    // Gone for good — not downloadable.
    let dl = server
        .client
        .get(server.url("/v3/package/hard.del/1.0.0/hard.del.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn oversized_upload_is_rejected() {
    let server = spawn_with(|c| c.max_package_size_bytes = Some(64)).await;
    let big = build_nupkg("Too.Big", "1.0.0", &[0u8; 4096]);
    let resp = server
        .client
        .put(server.url("/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .header("Content-Type", "application/octet-stream")
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
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
                                               // Accessibility + UX affordances.
    assert!(body.contains("Skip to content"));
    assert!(body.contains("role=\"search\""));
    assert!(body.contains("class=\"copy\"")); // copy-to-clipboard button
    assert!(body.contains("aria-label=\"Breadcrumb\""));

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
async fn stats_page_aggregates_the_feed() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Stat.A", "1.0.0", b"xx")).await;
    push_multipart(&server, API_KEY, build_nupkg("Stat.A", "1.1.0", b"xx")).await;
    push_multipart(&server, API_KEY, build_nupkg("Stat.B", "2.0.0", b"xx")).await;

    let resp = server
        .client
        .get(server.url("/stats"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    assert!(body.contains("Statistics"));
    // 2 distinct packages, 3 versions.
    assert!(body.contains(">2</div><div class=\"l\">Packages"));
    assert!(body.contains(">3</div><div class=\"l\">Versions"));
    // Both lists reference the published packages.
    assert!(body.contains("Most downloaded"));
    assert!(body.contains("Recently published"));
    assert!(body.contains("Stat.A"));
}

// ---------------------------------------------------------------------------
// Admin area
// ---------------------------------------------------------------------------

const ADMIN_KEY: &str = "admin-key";

async fn spawn_admin() -> TestServer {
    spawn_with(|c| c.admin_api_key = Some(ADMIN_KEY.to_string())).await
}

/// A client that does not auto-follow redirects, so 303s can be asserted.
fn no_redirect() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn admin_requires_authentication() {
    let server = spawn_admin().await;
    // No credentials -> 401 with a Basic-auth challenge.
    let resp = server
        .client
        .get(server.url("/admin"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(resp.headers()["www-authenticate"]
        .to_str()
        .unwrap()
        .contains("Basic"));
    // Correct credentials -> 200.
    let ok = server
        .client
        .get(server.url("/admin"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success());
}

#[tokio::test]
async fn admin_area_absent_without_key() {
    let server = spawn().await; // no admin key configured
    let resp = server
        .client
        .get(server.url("/admin"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn admin_disable_withholds_then_enable_restores() {
    let server = spawn_admin().await;
    push_multipart(&server, API_KEY, build_nupkg("Adm.Pkg", "1.0.0", b"data")).await;
    let client = no_redirect();
    let dl_url = "/v3/package/adm.pkg/1.0.0/adm.pkg.1.0.0.nupkg";

    // Disable the version.
    let resp = client
        .post(server.url("/admin/packages/adm.pkg/1.0.0/disable"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);

    // Disabled: not downloadable, and gone from the flat container / search.
    let dl = server.client.get(server.url(dl_url)).send().await.unwrap();
    assert_eq!(dl.status(), reqwest::StatusCode::NOT_FOUND);
    let fc = server
        .client
        .get(server.url("/v3/package/adm.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(fc.status(), reqwest::StatusCode::NOT_FOUND);

    // The admin page still shows it as disabled.
    let adm = server
        .client
        .get(server.url("/admin/packages/adm.pkg"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert!(adm.text().await.unwrap().contains("disabled"));

    // Disable requires auth, too.
    let unauth = client
        .post(server.url("/admin/packages/adm.pkg/1.0.0/enable"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

    // Re-enable: downloadable again.
    let resp = client
        .post(server.url("/admin/packages/adm.pkg/1.0.0/enable"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    let dl = server.client.get(server.url(dl_url)).send().await.unwrap();
    assert!(dl.status().is_success());
}

#[tokio::test]
async fn admin_delete_removes_version_and_symbols() {
    let server = spawn_admin().await;
    push_multipart(&server, API_KEY, build_nupkg("Del.Pkg", "1.0.0", b"data")).await;

    // Attach symbols, then confirm they are reachable.
    let pdb = build_portable_pdb(&[9u8; 16]);
    let key = yanuget::pdb::portable_pdb_signature(&pdb).unwrap();
    push_symbol(
        &server,
        API_KEY,
        build_snupkg("Del.Pkg", "1.0.0", "del.pkg.pdb", &pdb),
    )
    .await;
    let sym_url = format!("/download/symbols/del.pkg.pdb/{key}/del.pkg.pdb");
    assert!(server
        .client
        .get(server.url(&sym_url))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());

    // Admin delete removes the version...
    let resp = no_redirect()
        .post(server.url("/admin/packages/del.pkg/1.0.0/delete"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::SEE_OTHER);
    let dl = server
        .client
        .get(server.url("/v3/package/del.pkg/1.0.0/del.pkg.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), reqwest::StatusCode::NOT_FOUND);
    // ...and its symbols are gone too.
    let sym = server
        .client
        .get(server.url(&sym_url))
        .send()
        .await
        .unwrap();
    assert_eq!(sym.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn gallery_page_size_is_configurable() {
    let server = spawn_with(|c| c.gallery_page_size = 1).await;
    for id in ["Sz.A", "Sz.B"] {
        push_multipart(&server, API_KEY, build_nupkg(id, "1.0.0", b"x")).await;
    }
    // With a page size of 1 and 2 packages, a pager must appear.
    let body = server
        .client
        .get(server.url("/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(body.contains("class=\"pager\""));
    assert!(body.contains("of 2"));
}

#[tokio::test]
async fn settings_page_shows_policy_without_secrets() {
    let server = spawn_with(|c| {
        c.retention.enabled = true;
        c.retention.keep_latest_stable = Some(7);
    })
    .await;
    let resp = server
        .client
        .get(server.url("/settings"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    assert!(body.contains("Settings"));
    assert!(body.contains("Required (API key)"));
    assert!(body.contains("Keep newest stable"));
    assert!(body.contains("7"));
    // The actual API key must never appear on the unauthenticated page.
    assert!(!body.contains(API_KEY));
}

#[tokio::test]
async fn gallery_paginates_results() {
    let server = spawn().await;
    for id in ["Pg.A", "Pg.B", "Pg.C"] {
        push_multipart(&server, API_KEY, build_nupkg(id, "1.0.0", b"x")).await;
    }
    // One result per page → a pager with a Next link must appear.
    let resp = server
        .client
        .get(server.url("/packages?take=1"))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("class=\"pager\""));
    assert!(body.contains("of 3"));
    assert!(body.contains("skip=1")); // next page link
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

// ---------------------------------------------------------------------------
// Reverse-proxy URL derivation (X-Forwarded-*)
// ---------------------------------------------------------------------------

fn package_base_address(index: &serde_json::Value) -> String {
    index["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["@type"] == "PackageBaseAddress/3.0.0")
        .unwrap()["@id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn forwarded_headers_drive_generated_urls() {
    let server = spawn().await;
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
async fn forwarded_host_takes_first_of_a_list() {
    let server = spawn().await;
    let index: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .header("X-Forwarded-Proto", "https, http")
        .header("X-Forwarded-Host", "first.example.com, second.example.com")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        package_base_address(&index),
        "https://first.example.com/v3/package/"
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
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
    });

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();
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
    let after = reqwest::Client::new()
        .get(format!("{base}/health"))
        .send()
        .await;
    assert!(after.is_err());
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_distinct_pushes_all_succeed() {
    let server = Arc::new(spawn().await);
    let mut handles = Vec::new();
    for i in 0..12 {
        let s = server.clone();
        handles.push(tokio::spawn(async move {
            let nupkg = build_nupkg(&format!("Conc.P{i}"), "1.0.0", b"x");
            push_multipart(s.as_ref(), API_KEY, nupkg).await.status()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), reqwest::StatusCode::CREATED);
    }
    // Every package is queryable afterwards.
    for i in 0..12 {
        let resp = server
            .client
            .get(server.url(&format!("/v3/package/conc.p{i}/index.json")))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "Conc.P{i} missing");
    }
}

#[tokio::test]
async fn concurrent_same_version_push_one_wins() {
    let server = Arc::new(spawn().await);
    let mut handles = Vec::new();
    for _ in 0..6 {
        let s = server.clone();
        handles.push(tokio::spawn(async move {
            push_multipart(s.as_ref(), API_KEY, build_nupkg("Race.Pkg", "1.0.0", b"x"))
                .await
                .status()
        }));
    }
    let mut created = 0;
    let mut conflict = 0;
    for h in handles {
        match h.await.unwrap() {
            reqwest::StatusCode::CREATED => created += 1,
            reqwest::StatusCode::CONFLICT => conflict += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    // The unique constraint guarantees exactly one winner under the race.
    assert_eq!(created, 1);
    assert_eq!(conflict, 5);

    // The winner's payload must survive: a loser's rollback must never delete
    // the shared version directory the winner owns.
    let download = server
        .client
        .get(server.url("/v3/package/race.pkg/1.0.0/race.pkg.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert!(download.status().is_success(), "winner payload was deleted");
}

#[tokio::test]
async fn concurrent_pushes_with_prune_on_push_stay_consistent() {
    let server = Arc::new(
        spawn_with(|c| {
            c.retention.enabled = true;
            c.retention.prune_on_push = true;
            c.retention.keep_latest_stable = Some(2);
        })
        .await,
    );
    let mut handles = Vec::new();
    for i in 0..8 {
        let s = server.clone();
        handles.push(tokio::spawn(async move {
            let v = format!("1.{i}.0");
            push_multipart(s.as_ref(), API_KEY, build_nupkg("Race.Prune", &v, b"x"))
                .await
                .status()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), reqwest::StatusCode::CREATED);
    }

    // The server stayed responsive; the newest version is never pruned.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/race.prune/index.json"))
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
    assert!(
        list.contains(&"1.7.0".to_string()),
        "newest version was pruned: {list:?}"
    );
    assert!(list.len() <= 8);
}
