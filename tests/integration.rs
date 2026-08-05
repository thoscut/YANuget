//! End-to-end HTTP tests exercising the real router over a TCP socket with a
//! real `reqwest` client, a filesystem store and an on-disk SQLite database.

use std::io::{Cursor, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use yanuget::config::{Config, MirrorConfig};
use yanuget::database::{PackageDatabase, SqliteDatabase};
use yanuget::migrate::MigrateOptions;
use yanuget::storage::FilesystemStorage;
use yanuget::version::NuGetVersion;
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
        axum::serve(listener, connect_info(app)).await.unwrap();
    });

    TestServer {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
        _dir: dir,
    }
}

/// Serve with peer connection info, exactly as `main.rs` does.
///
/// The server only honours `X-Forwarded-*` from a peer in `trusted_proxies`,
/// which it can only identify when connection info is wired up. Tests connect
/// from `127.0.0.1`, which the default `private` trust set covers — so with this
/// in place the harness exercises the same trusted-proxy path production uses.
fn connect_info(
    app: axum::Router,
) -> axum::extract::connect_info::IntoMakeServiceWithConnectInfo<axum::Router, SocketAddr> {
    app.into_make_service_with_connect_info::<SocketAddr>()
}

/// Build a minimal but valid `.nupkg` in memory.
fn build_nupkg(id: &str, version: &str, payload_filler: &[u8]) -> Vec<u8> {
    build_nupkg_with_icon(id, version, payload_filler, None)
}

/// A minimal 1x1 PNG, for exercising the embedded-icon path.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

fn build_nupkg_with_icon(
    id: &str,
    version: &str,
    payload_filler: &[u8],
    icon: Option<&[u8]>,
) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Test Author</authors>
    <description>An integration test package for {id}.</description>
    <tags>integration test</tags>
    <requireLicenseAcceptance>true</requireLicenseAcceptance>
    {icon_element}
    <dependencies>
      <group targetFramework="net8.0">
        <dependency id="Newtonsoft.Json" version="[13.0.1, )" />
      </group>
    </dependencies>
  </metadata>
</package>"#,
        icon_element = if icon.is_some() {
            "<icon>images/icon.png</icon>"
        } else {
            ""
        }
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
        if let Some(bytes) = icon {
            zip.start_file("images/icon.png", opts).unwrap();
            zip.write_all(bytes).unwrap();
        }
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
async fn any_configured_api_key_authenticates_push() {
    let server = spawn_with(|c| c.api_keys = vec!["team-key".into()]).await;
    // The primary key still works...
    let resp = push_multipart(&server, API_KEY, build_nupkg("Multi.Key", "1.0.0", b"a")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    // ...and so does an additional configured key.
    let resp = push_multipart(&server, "team-key", build_nupkg("Multi.Key", "2.0.0", b"b")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    // An unconfigured key is still rejected.
    let resp = push_multipart(&server, "nope", build_nupkg("Multi.Key", "3.0.0", b"c")).await;
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

    // ...but the flat container must still list it. That endpoint is how a
    // client resolves a version it is about to restore, so omitting unlisted
    // versions makes a project pinned to one fail with NU1101 — which defeats
    // the entire point of unlisting rather than deleting. (Verified against the
    // real `dotnet restore`, which reported exactly that before this was fixed.)
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/unlist.me/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        versions["versions"],
        serde_json::json!(["1.0.0"]),
        "an unlisted version must stay resolvable through the flat container"
    );

    // ...and still downloadable by exact version (NuGet restore semantics).
    let download = server
        .client
        .get(server.url("/v3/package/unlist.me/1.0.0/unlist.me.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert!(download.status().is_success());

    // Search still hides it — discovery and resolution are different questions.
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
    let server = spawn_with(|c| c.allow_overwrite = yanuget::config::OverwriteMode::Enabled).await;
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
async fn prerelease_only_overwrite_protects_stable_versions() {
    let server =
        spawn_with(|c| c.allow_overwrite = yanuget::config::OverwriteMode::PrereleaseOnly).await;

    // A stable version is immutable: the re-push conflicts.
    let stable = build_nupkg("Pre.Only", "1.0.0", b"a");
    assert_eq!(
        push_multipart(&server, API_KEY, stable.clone())
            .await
            .status(),
        reqwest::StatusCode::CREATED
    );
    assert_eq!(
        push_multipart(&server, API_KEY, stable).await.status(),
        reqwest::StatusCode::CONFLICT
    );

    // A pre-release version may be overwritten.
    let pre = build_nupkg("Pre.Only", "2.0.0-rc.1", b"a");
    assert_eq!(
        push_multipart(&server, API_KEY, pre.clone()).await.status(),
        reqwest::StatusCode::CREATED
    );
    assert_eq!(
        push_multipart(&server, API_KEY, pre).await.status(),
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

#[tokio::test]
async fn rate_limit_returns_429_after_threshold() {
    let server = spawn_with(|c| {
        c.rate_limit.enabled = true;
        c.rate_limit.max_requests = 3;
        c.rate_limit.window_secs = 60;
    })
    .await;
    let url = server.url("/health");

    // Three requests from one client IP (carried in X-Forwarded-For) pass; the
    // fourth is throttled.
    for _ in 0..3 {
        let resp = server
            .client
            .get(&url)
            .header("X-Forwarded-For", "9.9.9.9")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }
    let resp = server
        .client
        .get(&url)
        .header("X-Forwarded-For", "9.9.9.9")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    // A different client IP has its own budget.
    let resp = server
        .client
        .get(&url)
        .header("X-Forwarded-For", "8.8.8.8")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
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
/// The CSRF token the admin UI embeds in its forms, derived from the admin key.
///
/// Admin state changes require it, so a signed-in operator visiting a hostile
/// page cannot have their browser's auto-replayed Basic credentials used to
/// delete packages.
fn admin_csrf(key: &str) -> String {
    yanuget::auth::AdminAuth::new(Some(key.to_string()))
        .csrf_token()
        .unwrap()
}

/// A form-encoded admin action body carrying the CSRF token.
fn admin_body(key: &str) -> String {
    format!("_csrf={}", admin_csrf(key))
}

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
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
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
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
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
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
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
    // The docs site is part of the web UI, so it is gone too.
    let docs = server
        .client
        .get(server.url("/docs/"))
        .send()
        .await
        .unwrap();
    assert_eq!(docs.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn embedded_docs_site_is_served() {
    let server = spawn().await;
    // The embedded site is served at /docs/ (the placeholder in test builds, the
    // real mkdocs site in CI/release builds) as self-contained HTML.
    let resp = server
        .client
        .get(server.url("/docs/"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(content_type.starts_with("text/html"), "got {content_type}");
    assert!(!resp.text().await.unwrap().is_empty());

    // /docs (no trailing slash) redirects to /docs/ (reqwest follows it to 200).
    let red = server.client.get(server.url("/docs")).send().await.unwrap();
    assert!(red.status().is_success());

    // An unknown doc path is a 404, not a panic or a wildcard match.
    let missing = server
        .client
        .get(server.url("/docs/nope/not-here.html"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
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
        axum::serve(listener, connect_info(app))
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

// ---------------------------------------------------------------------------
// Feeds: multi-feed isolation, approval rings, promotion, license policy, read auth
// ---------------------------------------------------------------------------

/// Spawn a server from fully resolved feeds (the multi-feed `build_app` path).
async fn spawn_feeds(customize: impl FnOnce(&mut Config)) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        host: Ipv4Addr::LOCALHOST.into(),
        port: 0,
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
    let feeds = config.resolved_feeds().unwrap();
    let feeds_meta = Arc::new(
        feeds
            .iter()
            .map(|f| FeedMeta {
                name: f.name.clone(),
                prefix: f.prefix.clone(),
                requires_approval: f.requires_approval,
            })
            .collect::<Vec<_>>(),
    );
    let mut states = Vec::new();
    for f in &feeds {
        states.push(
            AppState::for_feed(
                storage.clone(),
                db.clone(),
                config.clone(),
                f,
                feeds_meta.clone(),
            )
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
        axum::serve(listener, connect_info(app)).await.unwrap();
    });
    TestServer {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
        _dir: dir,
    }
}

async fn push_to(server: &TestServer, path: &str, key: &str, nupkg: Vec<u8>) -> reqwest::Response {
    let part = reqwest::multipart::Part::bytes(nupkg)
        .file_name("package.nupkg")
        .mime_str("application/octet-stream")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("package", part);
    server
        .client
        .put(server.url(path))
        .header("X-NuGet-ApiKey", key)
        .multipart(form)
        .send()
        .await
        .unwrap()
}

fn feed(name: &str) -> yanuget::config::FeedConfig {
    yanuget::config::FeedConfig {
        name: name.to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn feeds_are_isolated_and_prefixed() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.feeds = vec![feed("stable"), feed("dev")];
    })
    .await;

    // Push only into /stable.
    let resp = push_to(
        &server,
        "/stable/api/v2/package",
        API_KEY,
        build_nupkg("Iso.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // Visible in stable, absent in dev.
    let in_stable = server
        .client
        .get(server.url("/stable/v3/package/iso.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert!(in_stable.status().is_success());
    let in_dev = server
        .client
        .get(server.url("/dev/v3/package/iso.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(in_dev.status(), reqwest::StatusCode::NOT_FOUND);

    // The service index advertises feed-prefixed resource URLs.
    let index: serde_json::Value = server
        .client
        .get(server.url("/stable/v3/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        package_base_address(&index),
        format!("{}/stable/v3/package/", server.base)
    );

    // Root lists the feeds; each feed's gallery lives at its prefix `/{name}`.
    let root = server.client.get(server.url("/")).send().await.unwrap();
    assert!(root.text().await.unwrap().contains("href=\"/stable\""));
    let gallery = server
        .client
        .get(server.url("/stable"))
        .send()
        .await
        .unwrap();
    assert!(gallery.status().is_success());
}

#[tokio::test]
async fn approval_ring_withholds_until_approved() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut gated = feed("gated");
        gated.requires_approval = true;
        c.feeds = vec![gated];
    })
    .await;

    assert_eq!(
        push_to(
            &server,
            "/gated/api/v2/package",
            API_KEY,
            build_nupkg("Ring.Pkg", "1.0.0", b"x")
        )
        .await
        .status(),
        reqwest::StatusCode::CREATED
    );

    // Pending: not visible to clients yet.
    let pending = server
        .client
        .get(server.url("/gated/v3/package/ring.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(pending.status(), reqwest::StatusCode::NOT_FOUND);

    // Admin approves it.
    let approve = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(server.url("/gated/admin/packages/ring.pkg/1.0.0/approve"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(approve.status(), reqwest::StatusCode::SEE_OTHER);

    // Now servable.
    let live = server
        .client
        .get(server.url("/gated/v3/package/ring.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert!(live.status().is_success());
}

#[tokio::test]
async fn promotion_moves_a_version_into_the_next_ring() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut dev = feed("dev");
        dev.promotes_to = Some("stable".into());
        c.feeds = vec![dev, feed("stable")];
    })
    .await;

    push_to(
        &server,
        "/dev/api/v2/package",
        API_KEY,
        build_nupkg("Prom.Pkg", "1.0.0", b"x"),
    )
    .await;

    // Not in stable yet.
    assert_eq!(
        server
            .client
            .get(server.url("/stable/v3/package/prom.pkg/index.json"))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NOT_FOUND
    );

    // Promote dev -> stable.
    let promote = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
        .post(server.url("/dev/admin/packages/prom.pkg/1.0.0/promote"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(promote.status(), reqwest::StatusCode::SEE_OTHER);

    // Now present in both rings (shared payload, two memberships).
    assert!(server
        .client
        .get(server.url("/stable/v3/package/prom.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
    assert!(server
        .client
        .get(server.url("/dev/v3/package/prom.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

#[tokio::test]
async fn blocking_license_policy_rejects_push() {
    use yanuget::config::PolicyAction;
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut strict = feed("strict");
        strict.license_policy.enabled = true;
        strict.license_policy.allow_unlicensed = false;
        strict.license_policy.action = PolicyAction::Block;
        c.feeds = vec![strict];
    })
    .await;

    // build_nupkg declares no license -> blocked.
    let resp = push_to(
        &server,
        "/strict/api/v2/package",
        API_KEY,
        build_nupkg("Lic.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn read_auth_gates_downloads() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut private = feed("private");
        private.read_api_key = Some("read-key".into());
        c.feeds = vec![private];
    })
    .await;

    push_to(
        &server,
        "/private/api/v2/package",
        API_KEY,
        build_nupkg("Priv.Pkg", "1.0.0", b"x"),
    )
    .await;

    let url = "/private/v3/package/priv.pkg/index.json";
    // No credential -> 401.
    assert_eq!(
        server
            .client
            .get(server.url(url))
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    // With the read key -> 200.
    assert!(server
        .client
        .get(server.url(url))
        .header("X-NuGet-ApiKey", "read-key")
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

/// A standalone target (storage + database + a resolved default feed) that a
/// migration imports into, mirroring how the `migrate` command bootstraps.
struct MigrateTarget {
    storage: FilesystemStorage,
    db: SqliteDatabase,
    config: Arc<Config>,
    temp_dir: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn migrate_target() -> MigrateTarget {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        tls_enabled: false,
        ..Config::default()
    };
    let storage = FilesystemStorage::new(config.storage_path()).await.unwrap();
    let db = SqliteDatabase::connect(&config.database_path())
        .await
        .unwrap();
    let temp_dir = config.storage_path().join(".migrate");
    tokio::fs::create_dir_all(&temp_dir).await.unwrap();
    MigrateTarget {
        storage,
        db,
        config: Arc::new(config),
        temp_dir,
        _dir: dir,
    }
}

#[tokio::test]
async fn migrate_imports_all_packages_and_is_idempotent() {
    // The source is a real YANuget server holding several packages/versions.
    let source = spawn().await;
    for (id, version) in [
        ("Migrate.One", "1.0.0"),
        ("Migrate.One", "2.0.0"),
        ("Migrate.Two", "1.0.0"),
    ] {
        let resp = push_multipart(&source, API_KEY, build_nupkg(id, version, b"payload")).await;
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    }

    let target = migrate_target().await;
    let feeds = target.config.resolved_feeds().unwrap();
    let feed = &feeds[0];
    let source_cfg = MirrorConfig {
        enabled: true,
        upstream: source.url("/v3/index.json"),
        // The test source is on loopback; a migration is an operator-driven
        // command against a source they picked, so this is the same opt-in the
        // `migrate` sub-command applies.
        allow_private_upstream: true,
        ..Default::default()
    };
    let opts = MigrateOptions {
        quiet: true,
        ..Default::default()
    };

    // First run migrates everything.
    let summary = yanuget::migrate::run(
        &target.storage,
        &target.db,
        feed,
        &target.temp_dir,
        source_cfg.clone(),
        opts.clone(),
        indicatif::ProgressDrawTarget::hidden(),
    )
    .await
    .unwrap();

    assert_eq!(summary.discovered_ids, 2);
    assert_eq!(summary.total_versions, 3);
    assert_eq!(summary.imported, 3);
    assert_eq!(summary.skipped, 0);
    assert_eq!(summary.failed, 0);
    assert!(summary.total_bytes > 0);

    // The target feed now actually exposes every migrated version.
    for (id, version) in [
        ("migrate.one", "1.0.0"),
        ("migrate.one", "2.0.0"),
        ("migrate.two", "1.0.0"),
    ] {
        let v = NuGetVersion::parse(version).unwrap();
        assert!(
            target.db.exists(&feed.name, id, &v).await.unwrap(),
            "expected {id} {version} in the target feed"
        );
    }

    // Second run is a no-op: everything is already present (idempotent/resumable).
    let again = yanuget::migrate::run(
        &target.storage,
        &target.db,
        feed,
        &target.temp_dir,
        source_cfg,
        opts,
        indicatif::ProgressDrawTarget::hidden(),
    )
    .await
    .unwrap();
    assert_eq!(again.imported, 0);
    assert_eq!(again.skipped, 3);
    assert_eq!(again.failed, 0);
}

#[tokio::test]
async fn migrate_dry_run_reports_without_importing() {
    let source = spawn().await;
    let resp = push_multipart(&source, API_KEY, build_nupkg("Dry.Run", "1.0.0", b"x")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    let target = migrate_target().await;
    let feeds = target.config.resolved_feeds().unwrap();
    let feed = &feeds[0];
    let source_cfg = MirrorConfig {
        enabled: true,
        upstream: source.url("/v3/index.json"),
        // The test source is on loopback; a migration is an operator-driven
        // command against a source they picked, so this is the same opt-in the
        // `migrate` sub-command applies.
        allow_private_upstream: true,
        ..Default::default()
    };

    let summary = yanuget::migrate::run(
        &target.storage,
        &target.db,
        feed,
        &target.temp_dir,
        source_cfg,
        MigrateOptions {
            quiet: true,
            dry_run: true,
            ..Default::default()
        },
        indicatif::ProgressDrawTarget::hidden(),
    )
    .await
    .unwrap();

    assert_eq!(summary.total_versions, 1);
    assert_eq!(summary.imported, 0);
    // Nothing was actually written to the target.
    let v = NuGetVersion::parse("1.0.0").unwrap();
    assert!(!target.db.exists(&feed.name, "dry.run", &v).await.unwrap());
}

// ---------------------------------------------------------------------------
// Hardening: forwarding-header trust, response headers, auth gaps, CSRF,
// conditional downloads and cross-feed payload integrity.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forwarded_headers_are_ignored_from_an_untrusted_peer() {
    // Nobody is trusted, so the loopback test client is not a proxy either.
    let server = spawn_with(|c| c.trusted_proxies = Vec::new()).await;

    let index: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .header("X-Forwarded-Host", "evil.example.com")
        .header("X-Forwarded-Proto", "https")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Every advertised resource must still point at this server. A client that
    // followed a poisoned packageContent URL would fetch its packages from
    // whoever set the header.
    let base = package_base_address(&index);
    assert!(
        !base.contains("evil.example.com"),
        "spoofed forwarded host reached a generated URL: {base}"
    );
    assert!(base.starts_with(&server.base), "unexpected base: {base}");
}

#[tokio::test]
async fn rate_limit_survives_spoofed_forwarded_for() {
    // With no trusted proxy, X-Forwarded-For is stripped, so every request is
    // attributed to the real peer and the throttle actually holds.
    let server = spawn_with(|c| {
        c.trusted_proxies = Vec::new();
        c.rate_limit.enabled = true;
        c.rate_limit.max_requests = 3;
        c.rate_limit.window_secs = 60;
    })
    .await;

    let mut statuses = Vec::new();
    for i in 0..8u8 {
        let resp = server
            .client
            .get(server.url("/v3/index.json"))
            // A fresh "client IP" every time — which is the whole point of the
            // bypass this guards against.
            .header("X-Forwarded-For", format!("203.0.113.{i}"))
            .send()
            .await
            .unwrap();
        statuses.push(resp.status());
    }
    assert!(
        statuses.contains(&reqwest::StatusCode::TOO_MANY_REQUESTS),
        "rotating X-Forwarded-For bypassed the rate limit: {statuses:?}"
    );
}

#[tokio::test]
async fn responses_carry_baseline_security_headers() {
    let server = spawn().await;

    let resp = server.client.get(server.url("/")).send().await.unwrap();
    let headers = resp.headers().clone();
    assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
    assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
    let vary = headers.get("vary").unwrap().to_str().unwrap();
    assert!(vary.contains("X-Forwarded-Host"), "vary was {vary}");

    // The gallery renders package-supplied metadata, so it gets a policy that
    // denies everything except the two inline assets the server itself emits.
    let csp = headers
        .get("content-security-policy")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(csp.contains("default-src 'none'"), "csp was {csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "csp was {csp}");
    assert!(csp.contains("script-src 'sha256-"), "csp was {csp}");

    // JSON protocol documents get the headers too, but no gallery CSP.
    let json = server
        .client
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        json.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
}

#[tokio::test]
async fn symbol_download_and_settings_require_read_auth() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut private = feed("private");
        private.read_api_key = Some("read-key".into());
        c.feeds = vec![private];
    })
    .await;

    // A PDB carries source paths and, with embedded sources, code — so a gated
    // feed must gate it like any other package content.
    let sym = server
        .client
        .get(server.url("/private/download/symbols/app.pdb/ABCDEF01FFFFFFFF/app.pdb"))
        .send()
        .await
        .unwrap();
    assert_eq!(sym.status(), reqwest::StatusCode::UNAUTHORIZED);

    let settings = server
        .client
        .get(server.url("/private/settings"))
        .send()
        .await
        .unwrap();
    assert_eq!(settings.status(), reqwest::StatusCode::UNAUTHORIZED);

    // With the read key the settings page is reachable again.
    let ok = server
        .client
        .get(server.url("/private/settings"))
        .header("X-NuGet-ApiKey", "read-key")
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success());
}

#[tokio::test]
async fn admin_actions_require_a_csrf_token() {
    let server = spawn_admin().await;
    push_multipart(&server, API_KEY, build_nupkg("Csrf.Pkg", "1.0.0", b"x")).await;
    let client = no_redirect();
    let url = server.url("/admin/packages/csrf.pkg/1.0.0/disable");

    // Authenticated but with no token: this is what a cross-site form POST
    // looks like, since the browser attaches the Basic credentials by itself.
    let no_token = client
        .post(&url)
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(no_token.status(), reqwest::StatusCode::BAD_REQUEST);

    // A guessed token is no better.
    let bad_token = client
        .post(&url)
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("_csrf=not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(bad_token.status(), reqwest::StatusCode::BAD_REQUEST);

    // A browser that tells us the request came from another site is refused
    // even when it somehow carries the token.
    let cross_site = client
        .post(&url)
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("sec-fetch-site", "cross-site")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_site.status(), reqwest::StatusCode::BAD_REQUEST);

    // The real thing, as the admin page submits it, still works.
    let good = client
        .post(&url)
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("sec-fetch-site", "same-origin")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(good.status(), reqwest::StatusCode::SEE_OTHER);

    // And the page really does hand out that token, so the UI keeps working.
    let page = server
        .client
        .get(server.url("/admin/packages/csrf.pkg"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains(&admin_csrf(ADMIN_KEY)));
}

#[tokio::test]
async fn package_download_is_conditional_and_cacheable() {
    let server = spawn().await;
    push_multipart(
        &server,
        API_KEY,
        build_nupkg("Cache.Pkg", "1.0.0", &vec![7u8; 4096]),
    )
    .await;
    let url = server.url("/v3/package/cache.pkg/1.0.0/cache.pkg.1.0.0.nupkg");

    let first = server.client.get(&url).send().await.unwrap();
    assert!(first.status().is_success());
    let etag = first
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let cache_control = first
        .headers()
        .get("cache-control")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(cache_control.contains("immutable"), "{cache_control}");

    // A restore that already holds the package pays for a header exchange, not
    // for the payload again.
    let second = server
        .client
        .get(&url)
        .header("If-None-Match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), reqwest::StatusCode::NOT_MODIFIED);
    assert!(second.bytes().await.unwrap().is_empty());

    // A stale validator still gets the bytes.
    let changed = server
        .client
        .get(&url)
        .header("If-None-Match", "\"something-else\"")
        .send()
        .await
        .unwrap();
    assert!(changed.status().is_success());
}

#[tokio::test]
async fn a_second_feed_cannot_republish_different_bytes_under_a_taken_version() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.feeds = vec![feed("one"), feed("two")];
    })
    .await;

    // The payload is stored once and shared by every feed that holds the
    // version, so the *first* upload's bytes are what all of them serve.
    let original = build_nupkg("Shared.Pkg", "1.0.0", b"original-payload");
    let resp = push_to(&server, "/one/api/v2/package", API_KEY, original.clone()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // Pushing *different* bytes for the same id/version into another feed must
    // not succeed: the advertised hash would stop describing the served bytes,
    // and a client verifying packageHash would fail the restore.
    let different = build_nupkg("Shared.Pkg", "1.0.0", b"a-completely-different-payload");
    let resp = push_to(&server, "/two/api/v2/package", API_KEY, different).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CONFLICT);

    // The identical package is still free to join a second feed.
    let resp = push_to(&server, "/two/api/v2/package", API_KEY, original).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // ...and what that feed advertises matches what it serves.
    let reg: serde_json::Value = server
        .client
        .get(server.url("/two/v3/registration/shared.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let advertised = reg["items"][0]["items"][0]["packageContent"]
        .as_str()
        .unwrap()
        .to_string();
    let served = server.client.get(&advertised).send().await.unwrap();
    assert!(served.status().is_success());
}

#[tokio::test]
async fn health_reports_readiness_and_liveness() {
    let server = spawn().await;
    for path in ["/health", "/health/ready", "/health/live"] {
        let resp = server.client.get(server.url(path)).send().await.unwrap();
        assert!(resp.status().is_success(), "{path} was {}", resp.status());
    }
}

#[tokio::test]
async fn symbols_are_scoped_to_the_feed_that_owns_the_package() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.feeds = vec![feed("one"), feed("two")];
    })
    .await;

    // Publish a package and its symbols into /one only.
    let resp = push_to(
        &server,
        "/one/api/v2/package",
        API_KEY,
        build_nupkg("Sym.Scoped", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    let pdb = build_portable_pdb(&[42u8; 16]);
    let snupkg = build_snupkg("Sym.Scoped", "1.0.0", "sym.scoped.pdb", &pdb);
    let part = reqwest::multipart::Part::bytes(snupkg)
        .file_name("symbols.snupkg")
        .mime_str("application/octet-stream")
        .unwrap();
    let resp = server
        .client
        .put(server.url("/one/api/v2/symbol"))
        .header("X-NuGet-ApiKey", API_KEY)
        .multipart(reqwest::multipart::Form::new().part("package", part))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // The SSQP key is a property of the PDB itself, so anyone holding the .pdb
    // can compute it and ask any feed for it. The store is global — a symbol is
    // addressed by its signature, not by feed — so the feed that owns the
    // package is what has to decide who may fetch it.
    let ssqp = symbol_key_for(&pdb);
    let path = format!("/download/symbols/sym.scoped.pdb/{ssqp}/sym.scoped.pdb");

    let from_owner = server
        .client
        .get(server.url(&format!("/one{path}")))
        .send()
        .await
        .unwrap();
    assert!(
        from_owner.status().is_success(),
        "the owning feed should serve its own symbols: {}",
        from_owner.status()
    );

    // /two never received this package, so it must not serve its symbols.
    let from_other = server
        .client
        .get(server.url(&format!("/two{path}")))
        .send()
        .await
        .unwrap();
    assert_eq!(
        from_other.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a feed without the package leaked its symbols"
    );
}

/// The SSQP key a debugger computes for a Portable PDB: the GUID in canonical
/// order, upper-case hex, followed by the literal age `FFFFFFFF`.
fn symbol_key_for(pdb: &[u8]) -> String {
    // The fixture writes the GUID immediately after the 8-byte `#Pdb` name.
    let guid_at = pdb.len() - 20;
    let g = &pdb[guid_at..guid_at + 16];
    let order = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
    let mut s = String::new();
    for i in order {
        s.push_str(&format!("{:02X}", g[i]));
    }
    s.push_str("FFFFFFFF");
    s
}

#[tokio::test]
async fn require_license_acceptance_is_reported_as_the_author_declared_it() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Lic.Accept", "1.0.0", b"x")).await;

    // The fixture's nuspec sets <requireLicenseAcceptance>true</...>. Reporting
    // it as false would tell clients the author asked for no acceptance step
    // when they did.
    let reg: serde_json::Value = server
        .client
        .get(server.url("/v3/registration/lic.accept/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        reg["items"][0]["items"][0]["catalogEntry"]["requireLicenseAcceptance"],
        serde_json::Value::Bool(true)
    );
}

#[tokio::test]
async fn embedded_icons_are_served_only_when_they_really_are_images() {
    let server = spawn().await;

    // A package with a genuine PNG icon.
    push_multipart(
        &server,
        API_KEY,
        build_nupkg_with_icon("Icon.Pkg", "1.0.0", b"x", Some(TINY_PNG)),
    )
    .await;

    let icon = server
        .client
        .get(server.url("/packages/icon.pkg/1.0.0/icon"))
        .send()
        .await
        .unwrap();
    assert!(icon.status().is_success());
    assert_eq!(icon.headers().get("content-type").unwrap(), "image/png");
    // Icon bytes come from an uploaded package, so they are attacker-controlled
    // content served same-origin. They must not be sniffable into a document,
    // framable, or able to load anything of their own.
    assert_eq!(
        icon.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    assert_eq!(
        icon.headers().get("content-security-policy").unwrap(),
        "default-src 'none'"
    );
    assert_eq!(icon.bytes().await.unwrap().as_ref(), TINY_PNG);

    // An "icon" that is really an SVG — a script-bearing document — must not be
    // handed to a browser, whatever the manifest calls it.
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" onload="alert(1)"/>"#;
    push_multipart(
        &server,
        API_KEY,
        build_nupkg_with_icon("Evil.Icon", "1.0.0", b"x", Some(svg)),
    )
    .await;
    let refused = server
        .client
        .get(server.url("/packages/evil.icon/1.0.0/icon"))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::NOT_FOUND);

    // A package with no icon simply has none.
    push_multipart(&server, API_KEY, build_nupkg("Plain.Pkg", "1.0.0", b"x")).await;
    let none = server
        .client
        .get(server.url("/packages/plain.pkg/1.0.0/icon"))
        .send()
        .await
        .unwrap();
    assert_eq!(none.status(), reqwest::StatusCode::NOT_FOUND);

    // The detail page links the icon only for the package that has one.
    let with_icon = server
        .client
        .get(server.url("/packages/icon.pkg"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(with_icon.contains("/packages/icon.pkg/1.0.0/icon"));
    let without = server
        .client
        .get(server.url("/packages/plain.pkg"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!without.contains("/icon\""));
}

#[tokio::test]
async fn the_semver1_hive_withholds_versions_that_client_cannot_parse() {
    let server = spawn().await;

    // `1.0.0` is SemVer1-safe. `2.0.0-alpha.1` has a dotted pre-release label
    // and `3.0.0+build` carries build metadata — both are SemVer2-only.
    for version in ["1.0.0", "2.0.0-alpha.1", "3.0.0+build"] {
        let resp = push_multipart(&server, API_KEY, build_nupkg("Hive.Pkg", version, b"x")).await;
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED, "{version}");
    }

    // The service index points the two hives at different bases.
    let index: serde_json::Value = server
        .client
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resource = |ty: &str| -> String {
        index["resources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["@type"] == ty)
            .unwrap_or_else(|| panic!("{ty} missing from the service index"))["@id"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let sv1_base = resource("RegistrationsBaseUrl/3.4.0");
    let sv2_base = resource("RegistrationsBaseUrl/3.6.0");
    assert_ne!(
        sv1_base, sv2_base,
        "advertising one hive under both @types hands a SemVer1 client versions it cannot parse"
    );

    let versions_in = |doc: &serde_json::Value| -> Vec<String> {
        doc["items"][0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["catalogEntry"]["version"].as_str().unwrap().to_string())
            .collect()
    };

    let sv1: serde_json::Value = server
        .client
        .get(format!("{sv1_base}hive.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions_in(&sv1), vec!["1.0.0".to_string()]);

    let sv2: serde_json::Value = server
        .client
        .get(format!("{sv2_base}hive.pkg/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        versions_in(&sv2),
        vec![
            "1.0.0".to_string(),
            "2.0.0-alpha.1".to_string(),
            "3.0.0".to_string()
        ]
    );

    // A hive's documents must keep their self-references inside that hive,
    // otherwise a client following them crosses over and sees the versions the
    // hive just withheld.
    let sv1_leaf = sv1["items"][0]["items"][0]["@id"].as_str().unwrap();
    assert!(sv1_leaf.starts_with(&sv1_base), "leaked hive: {sv1_leaf}");
    let sv2_leaf = sv2["items"][0]["items"][0]["@id"].as_str().unwrap();
    assert!(sv2_leaf.starts_with(&sv2_base), "leaked hive: {sv2_leaf}");

    // A SemVer2-only version simply has no leaf in the SemVer1 hive.
    let missing = server
        .client
        .get(format!("{sv1_base}hive.pkg/2.0.0-alpha.1.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    let present = server
        .client
        .get(format!("{sv2_base}hive.pkg/2.0.0-alpha.1.json"))
        .send()
        .await
        .unwrap();
    assert!(present.status().is_success());

    // Search links into the hive matching the caller's semVerLevel.
    let sv1_search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=Hive.Pkg&prerelease=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reg = sv1_search["data"][0]["registration"].as_str().unwrap();
    assert!(reg.starts_with(&sv1_base), "search linked to {reg}");

    let sv2_search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=Hive.Pkg&prerelease=true&semVerLevel=2.0.0"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let reg = sv2_search["data"][0]["registration"].as_str().unwrap();
    assert!(reg.starts_with(&sv2_base), "search linked to {reg}");
}

#[tokio::test]
async fn http2_clients_are_given_this_servers_urls_not_localhost() {
    // HTTP/1.1 carries the target host in `Host`; HTTP/2 carries it in the
    // `:authority` pseudo-header, which hyper surfaces on the URI rather than as
    // a header. A server that only reads `Host` falls through to its default and
    // hands an HTTP/2 client absolute package URLs pointing at `localhost` —
    // every restore over HTTP/2 then fails.
    let server = spawn().await;
    let h2 = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();

    let index: serde_json::Value = h2
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let base = package_base_address(&index);
    assert!(
        base.starts_with(&server.base),
        "HTTP/2 request produced {base}, expected it under {}",
        server.base
    );
    assert!(!base.contains("localhost"), "fell back to the default host");

    // And the URL it produced is actually fetchable.
    push_multipart(&server, API_KEY, build_nupkg("H2.Pkg", "1.0.0", b"x")).await;
    let versions = h2
        .get(format!("{base}h2.pkg/index.json"))
        .send()
        .await
        .unwrap();
    assert!(versions.status().is_success());
}

#[tokio::test]
async fn autocomplete_does_not_suggest_packages_the_filters_exclude() {
    let server = spawn().await;
    // One package with only a pre-release version, one with a stable release.
    push_multipart(
        &server,
        API_KEY,
        build_nupkg("Pre.Only", "1.0.0-alpha", b"x"),
    )
    .await;
    push_multipart(&server, API_KEY, build_nupkg("Stable.One", "1.0.0", b"x")).await;

    let ids = |q: &str| {
        let url = server.url(q);
        let client = server.client.clone();
        async move {
            let doc: serde_json::Value =
                client.get(url).send().await.unwrap().json().await.unwrap();
            doc["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };

    // With pre-releases included, both are suggested.
    let all = ids("/v3/autocomplete?q=&prerelease=true").await;
    assert!(all.contains(&"Pre.Only".to_string()));
    assert!(all.contains(&"Stable.One".to_string()));

    // Without them, suggesting `Pre.Only` sends the caller to a package it will
    // then find nothing in.
    let stable = ids("/v3/autocomplete?q=&prerelease=false").await;
    assert!(
        !stable.contains(&"Pre.Only".to_string()),
        "suggested a package with no matching version: {stable:?}"
    );
    assert!(stable.contains(&"Stable.One".to_string()));
}

/// A pre-release label differing only in case is the *same* version, and the
/// server has to treat it as one everywhere or it hands clients bytes that do
/// not match the hash it published for them.
///
/// Before this was fixed the two pushes below produced two database rows that
/// shared a single file on disk: the second push was accepted even with
/// `allow_overwrite` off, it replaced the first package's bytes, and the flat
/// container listed the version twice.
#[tokio::test]
async fn a_case_variant_of_a_published_prerelease_is_the_same_version() {
    let server = spawn().await;

    let first = build_nupkg("Case.Probe", "1.0.0-Beta", b"AAAA");
    let response = push_multipart(&server, API_KEY, first.clone()).await;
    assert_eq!(response.status(), 201);

    // Same version, different case, different bytes: a conflict, not a push.
    let second = build_nupkg("Case.Probe", "1.0.0-beta", b"BBBB");
    let response = push_multipart(&server, API_KEY, second).await;
    assert_eq!(
        response.status(),
        409,
        "a case-variant re-push must conflict, not silently overwrite"
    );

    // The flat container lists the version once, in its canonical form.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/case.probe/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"], serde_json::json!(["1.0.0-beta"]));

    // Both spellings resolve to the originally published bytes.
    for spelling in ["1.0.0-Beta", "1.0.0-beta"] {
        let response = server
            .client
            .get(server.url(&format!(
                "/v3/package/case.probe/{}/case.probe.{}.nupkg",
                spelling.to_ascii_lowercase(),
                spelling.to_ascii_lowercase()
            )))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{spelling}");
        let body = response.bytes().await.unwrap();
        assert_eq!(
            body.as_ref(),
            first.as_slice(),
            "{spelling} served other bytes"
        );
    }
}

/// The SSQP key comes out of the uploaded PDB and the filename out of the zip
/// entry name, so both are chosen by whoever pushes. The symbol store is global
/// — a debugger asks for a key and nothing else — so a package must not be able
/// to claim a key another package already owns.
///
/// Before this was fixed, the second push below overwrote both the stored bytes
/// and the ownership row, and a debugger asking for the victim's key was served
/// the attacker's PDB.
#[tokio::test]
async fn a_package_cannot_claim_another_packages_symbols() {
    let server = spawn().await;

    // The victim publishes a package and its symbols.
    let guid = [7u8; 16];
    let victim_pdb = build_portable_pdb(&guid);
    let key = yanuget::pdb::portable_pdb_signature(&victim_pdb).expect("portable pdb key");
    push_multipart(&server, API_KEY, build_nupkg("Victim.Lib", "1.0.0", b"dll")).await;
    let response = push_symbol(
        &server,
        API_KEY,
        build_snupkg("Victim.Lib", "1.0.0", "victim.pdb", &victim_pdb),
    )
    .await;
    assert_eq!(response.status(), 201);

    // The path a debugger actually requests.
    let symbol_url = server.url(&format!("/download/symbols/victim.pdb/{key}/victim.pdb"));
    let fetch = || server.client.get(&symbol_url).send();

    let original = fetch().await.unwrap();
    assert_eq!(original.status(), 200);
    assert_eq!(
        original.bytes().await.unwrap().as_ref(),
        victim_pdb.as_slice()
    );

    // The attacker publishes their own package, then a symbol package carrying
    // a PDB with the victim's debug GUID under the victim's PDB name. Both are
    // public information, readable straight out of the victim's own assembly.
    push_multipart(
        &server,
        API_KEY,
        build_nupkg("Attacker.Lib", "1.0.0", b"dll"),
    )
    .await;
    let attacker_pdb = build_portable_pdb(&guid);
    let response = push_symbol(
        &server,
        API_KEY,
        build_snupkg("Attacker.Lib", "1.0.0", "victim.pdb", &attacker_pdb),
    )
    .await;
    assert_eq!(
        response.status(),
        400,
        "claiming another package's symbol key must be refused"
    );

    // And the victim's symbols are untouched and still theirs.
    let after = fetch().await.unwrap();
    assert_eq!(after.status(), 200);
    assert_eq!(after.bytes().await.unwrap().as_ref(), victim_pdb.as_slice());
}

/// A symbol package names as many entries as it likes, and each one used to be
/// extracted by re-opening the archive and re-scanning its central directory —
/// quadratic in the entry count. A 200 KiB upload naming a couple of thousand
/// PDBs occupied a blocking thread for tens of seconds, and `tokio::fs` shares
/// that pool, so enough of them stall file I/O server-wide.
///
/// The entry count is now capped, and the check has to be cheap: this test
/// fails on the time as well as the status.
#[tokio::test]
async fn a_symbol_package_with_absurdly_many_pdbs_is_refused_quickly() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Many.Pdbs", "1.0.0", b"dll")).await;

    // Entry names compress away to almost nothing, so this is a small upload.
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("Many.Pdbs.nuspec", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0"?><package><metadata><id>Many.Pdbs</id>
                <version>1.0.0</version><authors>a</authors>
                <description>d</description></metadata></package>"#,
        )
        .unwrap();
        for i in 0..4000 {
            zip.start_file(format!("lib/net8.0/f{i}.pdb"), opts)
                .unwrap();
            zip.write_all(&[0u8; 64]).unwrap();
        }
        zip.finish().unwrap();
    }
    let snupkg = cursor.into_inner();
    assert!(
        snupkg.len() < 1024 * 1024,
        "the point is that the upload is small"
    );

    let started = std::time::Instant::now();
    let response = push_symbol(&server, API_KEY, snupkg).await;
    let elapsed = started.elapsed();

    assert_eq!(
        response.status(),
        400,
        "an absurd entry count must be refused"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "took {elapsed:?}; the entry count should be rejected before any extraction"
    );
}

/// Overwriting a version must not leave the previous build's symbols behind.
///
/// The replacement build's PDBs carry different SSQP keys, so the old mappings
/// survived an overwrite and still resolved to an id/version that existed —
/// meaning a debugger attached to the new build was served the old build's PDB,
/// with its stale source mapping.
#[tokio::test]
async fn overwriting_a_version_retires_its_old_symbols() {
    let server = spawn_with(|c| c.allow_overwrite = yanuget::config::OverwriteMode::Enabled).await;

    push_multipart(&server, API_KEY, build_nupkg("Rebuilt.Lib", "1.0.0", b"v1")).await;
    let old_pdb = build_portable_pdb(&[0xA1; 16]);
    let old_key = yanuget::pdb::portable_pdb_signature(&old_pdb).unwrap();
    let response = push_symbol(
        &server,
        API_KEY,
        build_snupkg("Rebuilt.Lib", "1.0.0", "rebuilt.lib.pdb", &old_pdb),
    )
    .await;
    assert_eq!(response.status(), 201);

    let old_url = server.url(&format!(
        "/download/symbols/rebuilt.lib.pdb/{old_key}/rebuilt.lib.pdb"
    ));
    assert_eq!(
        server.client.get(&old_url).send().await.unwrap().status(),
        200,
        "the first build's symbols should be servable before the rebuild"
    );

    // Same version, rebuilt with different content.
    let response = push_multipart(
        &server,
        API_KEY,
        build_nupkg("Rebuilt.Lib", "1.0.0", b"v2-different"),
    )
    .await;
    assert_eq!(response.status(), 201);

    assert_eq!(
        server.client.get(&old_url).send().await.unwrap().status(),
        404,
        "the superseded build's PDB must no longer be served"
    );

    // And the rebuild can publish its own symbols under a new key.
    let new_pdb = build_portable_pdb(&[0xB2; 16]);
    let new_key = yanuget::pdb::portable_pdb_signature(&new_pdb).unwrap();
    let response = push_symbol(
        &server,
        API_KEY,
        build_snupkg("Rebuilt.Lib", "1.0.0", "rebuilt.lib.pdb", &new_pdb),
    )
    .await;
    assert_eq!(response.status(), 201);
    let new_url = server.url(&format!(
        "/download/symbols/rebuilt.lib.pdb/{new_key}/rebuilt.lib.pdb"
    ));
    let served = server.client.get(&new_url).send().await.unwrap();
    assert_eq!(served.status(), 200);
    assert_eq!(served.bytes().await.unwrap().as_ref(), new_pdb.as_slice());
}

/// The gallery is read in a browser, so its errors have to be pages.
///
/// Every one of them used to answer with a bare `{"error":"package not found"}`
/// — no chrome, no styling, no way back. Not a rare path either: the detail page
/// links every dependency by id, and on a private feed most dependencies come
/// from nuget.org and are not held locally, so the most obvious click on the
/// page produced raw JSON.
#[tokio::test]
async fn gallery_errors_are_pages_but_api_errors_stay_json() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Real.Pkg", "1.0.0", b"x")).await;

    let response = server
        .client
        .get(server.url("/packages/no.such.package"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .starts_with("text/html"),
        "gallery 404 should be HTML"
    );
    let body = response.text().await.unwrap();
    assert!(body.contains("<html"), "{body}");
    assert!(body.contains("Not found"), "{body}");
    assert!(body.contains("Back to the package list"), "{body}");
    assert!(!body.contains(r#"{"error""#), "{body}");

    // A NuGet client still gets JSON, because that is what it parses.
    let response = server
        .client
        .get(server.url("/v3/package/no.such.package/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let body = response.text().await.unwrap();
    assert!(
        !body.contains("<html"),
        "v3 errors must not be HTML: {body}"
    );
}
