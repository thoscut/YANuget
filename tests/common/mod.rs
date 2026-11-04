//! Helpers shared by the end-to-end HTTP tests: a server on a real TCP
//! socket, packages built in memory, and requests against them.

#![allow(dead_code)]

use std::io::{Cursor, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use yanuget::config::Config;
use yanuget::database::SqliteDatabase;
use yanuget::storage::FilesystemStorage;
use yanuget::web::{self, AppState, FeedMeta};
use zip::write::SimpleFileOptions;

pub const API_KEY: &str = "test-key";

pub struct TestServer {
    pub base: String,
    pub client: reqwest::Client,
    pub _dir: tempfile::TempDir,
}

impl TestServer {
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }
}

pub async fn spawn() -> TestServer {
    spawn_with(|_| {}).await
}

pub async fn spawn_with(customize: impl FnOnce(&mut Config)) -> TestServer {
    spawn_with_free_space(customize, None).await
}

/// [`spawn_with`], on a storage volume that always has `free` bytes free when
/// given, rather than whatever the test machine's disk has.
pub async fn spawn_with_free_space(
    customize: impl FnOnce(&mut Config),
    free: Option<u64>,
) -> TestServer {
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
    let mut space = yanuget::space::SpaceGuard::for_store(&config, db.clone());
    if let Some(free) = free {
        space = space.with_measure(move |_| Ok(free));
    }
    let config = Arc::new(config);
    let state = AppState::new_with_space(storage, db, config, Arc::new(space))
        .await
        .unwrap();
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
pub fn connect_info(
    app: axum::Router,
) -> axum::extract::connect_info::IntoMakeServiceWithConnectInfo<axum::Router, SocketAddr> {
    app.into_make_service_with_connect_info::<SocketAddr>()
}

/// Build a minimal but valid `.nupkg` in memory.
pub fn build_nupkg(id: &str, version: &str, payload_filler: &[u8]) -> Vec<u8> {
    build_nupkg_with_icon(id, version, payload_filler, None)
}

/// A minimal 1x1 PNG, for exercising the embedded-icon path.
pub const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

pub fn build_nupkg_with_icon(
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

/// A package declaring an SPDX `licenseExpression`, for exercising the license
/// policy.
pub fn build_nupkg_with_license(id: &str, version: &str, expression: &str) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Test Author</authors>
    <description>A package licensed under {expression}.</description>
    <license type="expression">{expression}</license>
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
        zip.write_all(b"dll").unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

pub async fn push_multipart(server: &TestServer, key: &str, nupkg: Vec<u8>) -> reqwest::Response {
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

pub const METADATA_SIGNATURE: u32 = 0x424A_5342;

/// Build a minimal Portable PDB whose `#Pdb` stream starts with `guid`.
pub fn build_portable_pdb(guid: &[u8; 16]) -> Vec<u8> {
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

/// A minimal PE32 assembly that vouches for `pdb` (built by
/// [`build_portable_pdb`], so its 20-byte id is its last 20 bytes): a CodeView
/// entry with the PDB's GUID and stamp, and a SHA-256 `PdbChecksum` entry over
/// the PDB with that id zeroed. The server only accepts symbols an assembly of
/// the owning package vouches for, as nuget.org does, and this is what
/// `dotnet pack` puts in a real one.
pub fn build_assembly_for(pdb: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    let id_at = pdb.len() - 20;
    let mut zeroed = pdb.to_vec();
    zeroed[id_at..].fill(0);
    let checksum = sha2::Sha256::digest(&zeroed);
    let guid = &pdb[id_at..id_at + 16];
    let stamp = &pdb[id_at + 16..];

    const RVA: u32 = 0x2000;
    const RAW: u32 = 0x200;
    let mut image = vec![0u8; RAW as usize];
    image[..2].copy_from_slice(b"MZ");
    image[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    image[0x80..0x84].copy_from_slice(b"PE\0\0");
    image[0x86..0x88].copy_from_slice(&1u16.to_le_bytes()); // one section
    image[0x94..0x96].copy_from_slice(&224u16.to_le_bytes()); // optional header size
    let optional = 0x98;
    image[optional..optional + 2].copy_from_slice(&0x10Bu16.to_le_bytes()); // PE32
    image[optional + 92..optional + 96].copy_from_slice(&16u32.to_le_bytes());

    // The section: two debug-directory entries, then their data.
    let mut codeview = b"RSDS".to_vec();
    codeview.extend_from_slice(guid);
    codeview.extend_from_slice(&1u32.to_le_bytes());
    codeview.extend_from_slice(b"Lib.pdb\0");
    let mut sum = b"SHA256\0".to_vec();
    sum.extend_from_slice(&checksum);
    let mut section = vec![0u8; 56];
    for (i, (kind, entry_stamp, minor, data)) in [
        (2u32, stamp, 0x504Du16, &codeview),
        (19u32, &[0u8; 4][..], 0u16, &sum),
    ]
    .into_iter()
    .enumerate()
    {
        let at = section.len() as u32;
        section.extend_from_slice(data);
        let e = &mut section[i * 28..i * 28 + 28];
        e[4..8].copy_from_slice(entry_stamp);
        e[8..10].copy_from_slice(&0x0100u16.to_le_bytes());
        e[10..12].copy_from_slice(&minor.to_le_bytes());
        e[12..16].copy_from_slice(&kind.to_le_bytes());
        e[16..20].copy_from_slice(&(data.len() as u32).to_le_bytes());
        e[20..24].copy_from_slice(&(RVA + at).to_le_bytes());
        e[24..28].copy_from_slice(&(RAW + at).to_le_bytes());
    }
    let debug = optional + 96 + 6 * 8;
    image[debug..debug + 4].copy_from_slice(&RVA.to_le_bytes());
    image[debug + 4..debug + 8].copy_from_slice(&56u32.to_le_bytes());
    let header = optional + 224;
    image[header..header + 5].copy_from_slice(b".text");
    let len = (section.len() as u32).to_le_bytes();
    image[header + 8..header + 12].copy_from_slice(&len);
    image[header + 12..header + 16].copy_from_slice(&RVA.to_le_bytes());
    image[header + 16..header + 20].copy_from_slice(&len);
    image[header + 20..header + 24].copy_from_slice(&RAW.to_le_bytes());
    image.extend_from_slice(&section);
    image
}

/// A package whose `lib/net8.0/` holds the assembly `pdb_name` belongs to
/// (`x.pdb` → `x.dll`), so that [`build_snupkg`] with the same name and PDB is
/// accepted for it.
pub fn build_nupkg_for_pdb(id: &str, version: &str, pdb_name: &str, pdb: &[u8]) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Test Author</authors>
    <description>A package with symbols.</description>
  </metadata>
</package>"#
    );
    let dll = pdb_name.strip_suffix(".pdb").unwrap_or(pdb_name);
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(format!("{id}.nuspec"), opts).unwrap();
        zip.write_all(nuspec.as_bytes()).unwrap();
        zip.start_file(format!("lib/net8.0/{dll}.dll"), opts)
            .unwrap();
        zip.write_all(&build_assembly_for(pdb)).unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

/// Build a `.snupkg` (symbol package): a nuspec plus one `.pdb`.
pub fn build_snupkg(id: &str, version: &str, pdb_name: &str, pdb: &[u8]) -> Vec<u8> {
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

pub async fn push_symbol(server: &TestServer, key: &str, snupkg: Vec<u8>) -> reqwest::Response {
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

pub const ADMIN_KEY: &str = "admin-key";

pub async fn spawn_admin() -> TestServer {
    spawn_with(|c| c.admin_api_key = Some(ADMIN_KEY.to_string())).await
}

/// A client that does not auto-follow redirects, so 303s can be asserted.
/// The CSRF token the admin UI embeds in its forms, derived from the admin key.
///
/// Admin state changes require it, so a signed-in operator visiting a hostile
/// page cannot have their browser's auto-replayed Basic credentials used to
/// delete packages.
pub fn admin_csrf(key: &str) -> String {
    yanuget::auth::AdminAuth::new(Some(key.to_string()))
        .csrf_token()
        .unwrap()
}

/// A form-encoded admin action body carrying the CSRF token.
pub fn admin_body(key: &str) -> String {
    format!("_csrf={}", admin_csrf(key))
}

pub fn no_redirect() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

/// Bytes that look like an image to nobody but are big enough to page: 300 KB.
pub fn image_bytes(seed: u8) -> Vec<u8> {
    (0..300_000u32)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

/// A tus request builder with the protocol header set.
pub fn tus(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header("Tus-Resumable", "1.0.0")
        .header("X-NuGet-ApiKey", API_KEY)
}

pub fn tus_metadata(pairs: &[(&str, &str)]) -> String {
    use base64::Engine;
    pairs
        .iter()
        .map(|(k, v)| {
            format!(
                "{k} {}",
                base64::engine::general_purpose::STANDARD.encode(v)
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

pub fn package_base_address(index: &serde_json::Value) -> String {
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

/// Spawn a server from fully resolved feeds (the multi-feed `build_app` path).
pub async fn spawn_feeds(customize: impl FnOnce(&mut Config)) -> TestServer {
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
            .map(FeedMeta::from_resolved)
            .collect::<Vec<_>>(),
    );
    let space = Arc::new(yanuget::space::SpaceGuard::for_store(&config, db.clone()));
    let mut states = Vec::new();
    for f in &feeds {
        states.push(
            AppState::for_feed(
                storage.clone(),
                db.clone(),
                config.clone(),
                f,
                feeds_meta.clone(),
                space.clone(),
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

pub async fn push_to(
    server: &TestServer,
    path: &str,
    key: &str,
    nupkg: Vec<u8>,
) -> reqwest::Response {
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

pub fn feed(name: &str) -> yanuget::config::FeedConfig {
    yanuget::config::FeedConfig {
        name: name.to_string(),
        ..Default::default()
    }
}

/// POST an admin action (`approve`, `promote`, …) at `path`, not following
/// the redirect.
pub async fn admin_action(server: &TestServer, path: &str) -> reqwest::Response {
    no_redirect()
        .post(server.url(path))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap()
}

/// POST the admin page's selection form for `id` under `prefix`.
pub async fn admin_bulk(
    server: &TestServer,
    prefix: &str,
    id: &str,
    key: &str,
    fields: &str,
) -> reqwest::Response {
    no_redirect()
        .post(server.url(&format!("{prefix}/admin/packages/{id}")))
        .basic_auth("admin", Some(key))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("{}&{fields}", admin_body(key)))
        .send()
        .await
        .unwrap()
}

pub async fn status_of(server: &TestServer, path: &str) -> reqwest::StatusCode {
    server
        .client
        .get(server.url(path))
        .send()
        .await
        .unwrap()
        .status()
}

/// Wait until search reports `expected` downloads of `id`.
///
/// Downloads are counted off the request path, so the count can trail the
/// response that caused it by a moment.
pub async fn await_downloads(server: &TestServer, id: &str, expected: u64) {
    let mut last = serde_json::Value::Null;
    for _ in 0..100 {
        let search: serde_json::Value = server
            .client
            .get(server.url(&format!("/v3/search?q={id}")))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        last = search["data"][0]["totalDownloads"].clone();
        if last == expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("{id}: expected {expected} downloads, search says {last}");
}

/// PUT `body` as file `name` of `id`/`v` in the feed under `prefix`.
pub async fn put_feed_file(
    server: &TestServer,
    prefix: &str,
    id: &str,
    v: &str,
    name: &str,
    body: Vec<u8>,
) -> reqwest::Response {
    server
        .client
        .put(server.url(&format!("{prefix}/api/v2/files/{id}/{v}/{name}")))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(body)
        .send()
        .await
        .unwrap()
}

/// An intake feed that takes installers, gated, promoting into a gated
/// production feed that keeps the default file types.
pub async fn spawn_intake() -> TestServer {
    spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut intake = feed("intake");
        intake.requires_approval = true;
        intake.promotes_to = Some("prod".into());
        intake.files.allowed_extensions = Some(vec!["exe".into(), ".MSI".into()]);
        let mut prod = feed("prod");
        prod.requires_approval = true;
        c.feeds = vec![intake, prod];
    })
    .await
}
