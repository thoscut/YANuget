//! End-to-end tests of what the server accepts in a package or symbol package,
//! and how it describes what it accepted: manifest and version parsing, archive
//! consistency, embedded files, symbol ownership and the protocol documents.
//!
//! The harness is a trimmed copy of the one in `integration.rs`.

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

    async fn push(&self, nupkg: Vec<u8>) -> reqwest::Response {
        self.put("/api/v2/package", nupkg).await
    }

    async fn push_symbols(&self, snupkg: Vec<u8>) -> reqwest::Response {
        self.put("/api/v2/symbol", snupkg).await
    }

    async fn put(&self, path: &str, body: Vec<u8>) -> reqwest::Response {
        self.client
            .put(self.url(path))
            .header("X-NuGet-ApiKey", API_KEY)
            .header("Content-Type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .unwrap()
    }

    async fn get_json(&self, path: &str) -> serde_json::Value {
        let response = self.client.get(self.url(path)).send().await.unwrap();
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        response.json().await.unwrap()
    }

    async fn status(&self, path: &str) -> u16 {
        self.client
            .get(self.url(path))
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}

async fn spawn() -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        api_key: Some(API_KEY.to_string()),
        host: Ipv4Addr::LOCALHOST.into(),
        port: 0,
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
        client: reqwest::Client::new(),
        _dir: dir,
    }
}

fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

/// A nuspec whose `<metadata>` holds `id`, `version` and then `extra`.
fn nuspec(id: &str, version: &str, extra: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Test Author</authors>
    <description>A test package.</description>
    {extra}
  </metadata>
</package>"#
    )
}

fn nupkg(id: &str, version: &str, extra: &str) -> Vec<u8> {
    let manifest = nuspec(id, version, extra);
    zip(&[
        (&format!("{id}.nuspec"), manifest.as_bytes()),
        ("lib/net8.0/Lib.dll", b"MZ"),
    ])
}

async fn refused(response: reqwest::Response, needle: &str) {
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(needle), "expected {needle:?} in {body}");
}

// ---------------------------------------------------------------------------
// Versions (COR-13)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn versions_nuget_would_refuse_are_refused() {
    let server = spawn().await;
    for version in ["v1.0.0", "1.0.0-01", "1.0.0+has space", "2147483648.0.0"] {
        let response = server.push(nupkg("Bad.Version", version, "")).await;
        assert_eq!(response.status(), 400, "{version} was accepted");
    }
    let long = format!("1.0.0-{}", "a".repeat(64));
    assert_eq!(
        server.push(nupkg("Bad.Version", &long, "")).await.status(),
        400
    );
    assert_eq!(
        server.status("/v3/package/bad.version/index.json").await,
        404
    );

    // `1.0.0-1` is fine, and a URL with a leading `v` is not a version.
    assert_eq!(
        server
            .push(nupkg("Ok.Version", "1.0.0-1", ""))
            .await
            .status(),
        201
    );
    assert_eq!(
        server
            .status("/v3/registration/ok.version/v1.0.0-1.json")
            .await,
        400
    );
}

// ---------------------------------------------------------------------------
// Manifests (SEC-09, SEC-10)
// ---------------------------------------------------------------------------

/// The identity the feed indexes is the one NuGet's reader sees in the same
/// bytes, or the package is refused.
#[tokio::test]
async fn a_manifest_is_read_as_nuget_reads_it() {
    let server = spawn().await;

    let duplicate = nupkg("First.Id", "1.0.0", "<id>Second.Id</id>");
    refused(server.push(duplicate).await, "more than once").await;

    let nested = nupkg("Real.Id", "1.0.0", "<owners><id>Fake.Id</id></owners>");
    assert_eq!(server.push(nested).await.status(), 201);
    assert_eq!(server.status("/v3/package/real.id/index.json").await, 200);
    assert_eq!(server.status("/v3/package/fake.id/index.json").await, 404);

    let license = r#"<license type="expression">MIT</license>
                     <license type="expression">GPL-3.0-only</license>"#;
    refused(
        server.push(nupkg("Two.Licenses", "1.0.0", license)).await,
        "more than once",
    )
    .await;

    let inner = nupkg("Inner.Element", "1.0.0<b/>", "");
    refused(server.push(inner).await, "contains an element").await;
}

#[tokio::test]
async fn a_manifest_with_absurdly_many_attributes_is_refused_quickly() {
    let server = spawn().await;
    let attributes: String = (0..30_000).map(|i| format!(" a{i}=\"x\"")).collect();
    let extra = format!("<dependencies><dependency id=\"Dep\"{attributes} /></dependencies>");
    let started = std::time::Instant::now();
    refused(
        server.push(nupkg("Many.Attributes", "1.0.0", &extra)).await,
        "attributes",
    )
    .await;
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

// ---------------------------------------------------------------------------
// Archives (SEC-26, PERF-03) and embedded files (COR-19)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_archive_with_two_manifests_is_refused() {
    let server = spawn().await;
    let (a, b) = (nuspec("A.Id", "1.0.0", ""), nuspec("B.Id", "1.0.0", ""));
    let bytes = zip(&[("A.nuspec", a.as_bytes()), ("B.nuspec", b.as_bytes())]);
    refused(server.push(bytes).await, "more than one root .nuspec").await;
}

/// A readme over nuget.org's 1 MiB limit used to be stored cut short — possibly
/// mid-way through a UTF-8 sequence — and still flagged as present.
#[tokio::test]
async fn an_oversized_readme_is_refused_not_truncated() {
    let server = spawn().await;
    let manifest = nuspec("Big.Readme", "1.0.0", "<readme>docs/README.md</readme>");
    let readme = "é".repeat(600 * 1024); // 1.2 MiB of two-byte characters
    let bytes = zip(&[
        ("Big.Readme.nuspec", manifest.as_bytes()),
        ("docs/README.md", readme.as_bytes()),
    ]);
    refused(server.push(bytes).await, "docs/README.md is larger than").await;
    assert_eq!(
        server.status("/v3/package/big.readme/index.json").await,
        404
    );

    // Just under the limit is fine.
    let readme = "a".repeat(1024 * 1024);
    let bytes = zip(&[
        ("Big.Readme.nuspec", manifest.as_bytes()),
        ("docs/README.md", readme.as_bytes()),
    ]);
    assert_eq!(server.push(bytes).await.status(), 201);
}

// ---------------------------------------------------------------------------
// Protocol documents (COR-14)
// ---------------------------------------------------------------------------

/// A minimal 1x1 PNG.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

#[tokio::test]
async fn registration_describes_packages_as_nuget_org_does() {
    let server = spawn().await;
    let extra =
        r#"<icon>images/icon.png</icon><license type="expression">MIT OR Apache-2.0</license>"#;
    let manifest = nuspec("Shown.Pkg", "2.0.0-Beta.1+Sha.ABC", extra);
    let bytes = zip(&[
        ("Shown.Pkg.nuspec", manifest.as_bytes()),
        ("images/icon.png", TINY_PNG),
    ]);
    assert_eq!(server.push(bytes).await.status(), 201);

    let index = server
        .get_json("/v3/registration-semver2/shown.pkg/index.json")
        .await;
    let entry = &index["items"][0]["items"][0]["catalogEntry"];
    assert_eq!(entry["version"], "2.0.0-Beta.1+Sha.ABC");
    assert_eq!(
        entry["licenseUrl"],
        "https://licenses.nuget.org/MIT+OR+Apache-2.0"
    );
    let icon = entry["iconUrl"].as_str().unwrap();
    assert!(
        icon.ends_with("/packages/shown.pkg/2.0.0-beta.1/icon"),
        "{icon}"
    );
    // The icon URL really serves the icon.
    let path = icon.strip_prefix(&server.base).unwrap();
    assert_eq!(server.status(path).await, 200);
}

#[tokio::test]
async fn autocomplete_leaves_out_prereleases_unless_asked() {
    let server = spawn().await;
    assert_eq!(
        server
            .push(nupkg("Pre.Only", "1.0.0-alpha", ""))
            .await
            .status(),
        201
    );
    assert_eq!(
        server.push(nupkg("Stable.Too", "1.0.0", "")).await.status(),
        201
    );

    let ids = |doc: serde_json::Value| -> Vec<String> {
        doc["data"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_lowercase())
            .collect()
    };
    let default = ids(server.get_json("/v3/autocomplete?q=").await);
    assert!(default.contains(&"stable.too".to_string()));
    assert!(!default.contains(&"pre.only".to_string()), "{default:?}");
    let asked = ids(server.get_json("/v3/autocomplete?q=&prerelease=true").await);
    assert!(asked.contains(&"pre.only".to_string()));

    let versions = server.get_json("/v3/autocomplete?id=pre.only").await;
    assert_eq!(versions["totalHits"], 0);
}

#[tokio::test]
async fn search_total_downloads_counts_every_version() {
    let server = spawn().await;
    assert_eq!(
        server
            .push(nupkg("Counted.Pkg", "1.0.0", ""))
            .await
            .status(),
        201
    );
    assert_eq!(
        server
            .push(nupkg("Counted.Pkg", "2.0.0-rc", ""))
            .await
            .status(),
        201
    );
    for path in [
        "/v3/package/counted.pkg/2.0.0-rc/counted.pkg.2.0.0-rc.nupkg",
        "/v3/package/counted.pkg/2.0.0-rc/counted.pkg.2.0.0-rc.nupkg",
        "/v3/package/counted.pkg/1.0.0/counted.pkg.1.0.0.nupkg",
    ] {
        assert_eq!(server.status(path).await, 200);
    }
    // Download counting is not necessarily done by the time the response is.
    let mut total = serde_json::Value::Null;
    for _ in 0..50 {
        let doc = server.get_json("/v3/search?q=counted.pkg").await;
        total = doc["data"][0]["totalDownloads"].clone();
        if total == 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    // Stable-only search, but the package's total still counts the RC.
    assert_eq!(total, 3);
}

// ---------------------------------------------------------------------------
// Symbols (SEC-08, COR-20)
// ---------------------------------------------------------------------------

/// A minimal Portable PDB: the metadata root, one `#Pdb` stream starting with
/// the 20-byte id (`guid`, then a zero stamp), and `body` after it.
fn portable_pdb(guid: &[u8; 16], body: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&0x424A_5342u32.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    let version = b"PDB v1.0\0\0\0\0";
    buf.extend_from_slice(&(version.len() as u32).to_le_bytes());
    buf.extend_from_slice(version);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    let header = buf.len();
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&20u32.to_le_bytes());
    buf.extend_from_slice(b"#Pdb\0\0\0\0");
    let offset = buf.len() as u32;
    buf[header..header + 4].copy_from_slice(&offset.to_le_bytes());
    buf.extend_from_slice(guid);
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(body);
    buf
}

/// A minimal PE32 assembly vouching for `pdb` (built by [`portable_pdb`]): a
/// CodeView entry with its GUID and stamp, and a SHA-256 `PdbChecksum` over it
/// with the id zeroed.
fn assembly_for(pdb: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    // Metadata root (16), version string (12), flags and stream count (4), the
    // one stream header (16).
    let id_at = 48;
    let mut zeroed = pdb.to_vec();
    zeroed[id_at..id_at + 20].fill(0);
    let checksum = sha2::Sha256::digest(&zeroed);

    const RVA: u32 = 0x2000;
    const RAW: u32 = 0x200;
    let mut image = vec![0u8; RAW as usize];
    image[..2].copy_from_slice(b"MZ");
    image[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
    image[0x80..0x84].copy_from_slice(b"PE\0\0");
    image[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
    image[0x94..0x96].copy_from_slice(&224u16.to_le_bytes());
    let optional = 0x98;
    image[optional..optional + 2].copy_from_slice(&0x10Bu16.to_le_bytes());
    image[optional + 92..optional + 96].copy_from_slice(&16u32.to_le_bytes());

    let mut codeview = b"RSDS".to_vec();
    codeview.extend_from_slice(&pdb[id_at..id_at + 16]);
    codeview.extend_from_slice(&1u32.to_le_bytes());
    codeview.extend_from_slice(b"Lib.pdb\0");
    let mut sum = b"SHA256\0".to_vec();
    sum.extend_from_slice(&checksum);
    let stamp = &pdb[id_at + 16..id_at + 20];
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

fn snupkg(id: &str, pdbs: &[(&str, &[u8])]) -> Vec<u8> {
    let manifest = nuspec(id, "1.0.0", "");
    let mut entries: Vec<(&str, &[u8])> = vec![("Sym.nuspec", manifest.as_bytes())];
    entries.extend_from_slice(pdbs);
    zip(&entries)
}

fn ssqp(pdb: &[u8]) -> String {
    yanuget::pdb::portable_pdb_signature(pdb).unwrap()
}

#[tokio::test]
async fn symbols_must_belong_to_an_assembly_of_the_package() {
    let server = spawn().await;
    let pdb = portable_pdb(&[3u8; 16], b"tables");
    let manifest = nuspec("Owned.Lib", "1.0.0", "");
    let bytes = zip(&[
        ("Owned.Lib.nuspec", manifest.as_bytes()),
        ("lib/net8.0/Owned.Lib.dll", &assembly_for(&pdb)),
    ]);
    assert_eq!(server.push(bytes).await.status(), 201);

    // A PDB with no assembly of that name in the package.
    let stray = portable_pdb(&[4u8; 16], b"tables");
    refused(
        server
            .push_symbols(snupkg("Owned.Lib", &[("lib/net8.0/Other.pdb", &stray)]))
            .await,
        "for it to belong to",
    )
    .await;

    // A forgery carrying the assembly's (public) id over other content.
    let forged = portable_pdb(&[3u8; 16], b"forged");
    refused(
        server
            .push_symbols(snupkg(
                "Owned.Lib",
                &[("lib/net8.0/Owned.Lib.pdb", &forged)],
            ))
            .await,
        "checksum does not match",
    )
    .await;

    // The genuine PDB is accepted and served byte for byte.
    let response = server
        .push_symbols(snupkg("Owned.Lib", &[("lib/net8.0/Owned.Lib.pdb", &pdb)]))
        .await;
    assert_eq!(response.status(), 201);
    let key = ssqp(&pdb);
    let served = server
        .client
        .get(server.url(&format!(
            "/download/symbols/owned.lib.pdb/{key}/owned.lib.pdb"
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(served.status(), 200);
    assert_eq!(served.bytes().await.unwrap().as_ref(), pdb.as_slice());
}

/// One refused PDB refuses the push; the ones before it used to stay stored.
#[tokio::test]
async fn a_symbol_push_is_all_or_nothing() {
    let server = spawn().await;
    let good = portable_pdb(&[5u8; 16], b"good");
    let manifest = nuspec("Half.Lib", "1.0.0", "");
    let bytes = zip(&[
        ("Half.Lib.nuspec", manifest.as_bytes()),
        ("lib/A.dll", &assembly_for(&good)),
    ]);
    assert_eq!(server.push(bytes).await.status(), 201);

    let orphan = portable_pdb(&[6u8; 16], b"orphan");
    let response = server
        .push_symbols(snupkg(
            "Half.Lib",
            &[("lib/A.pdb", &good), ("lib/B.pdb", &orphan)],
        ))
        .await;
    assert_eq!(response.status(), 400);
    let key = ssqp(&good);
    assert_eq!(
        server
            .status(&format!("/download/symbols/a.pdb/{key}/a.pdb"))
            .await,
        404,
        "the valid PDB of a refused push must not be stored"
    );
}
