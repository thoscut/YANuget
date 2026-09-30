//! Attached files at the edges: the SSH inbox faced with links an uploader
//! planted, blobs shared between versions, and what cancelled or finished
//! uploads leave behind.

// The inbox tests plant links, which only Unix lets an unprivileged user do;
// elsewhere their helpers go unused.
#![cfg_attr(not(unix), allow(dead_code))]

use std::io::{Cursor, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
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
    dir: tempfile::TempDir,
}

impl TestServer {
    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn data(&self) -> &Path {
        self.dir.path()
    }
}

async fn spawn_with(customize: impl FnOnce(&mut Config)) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        api_key: Some(API_KEY.to_string()),
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
    let state = AppState::new(storage, db, Arc::new(config)).await.unwrap();
    let app = web::router(state).into_make_service_with_connect_info::<SocketAddr>();
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
        dir,
    }
}

async fn spawn() -> TestServer {
    spawn_with(|_| {}).await
}

/// A minimal but valid `.nupkg`.
fn build_nupkg(id: &str, version: &str) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Test Author</authors>
    <description>A test package.</description>
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
        zip.write_all(b"dll").unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

async fn push(server: &TestServer, nupkg: Vec<u8>) -> reqwest::Response {
    server
        .client
        .put(server.url("/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(nupkg)
        .send()
        .await
        .unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(bytes))
}

async fn download(server: &TestServer, id: &str, v: &str, name: &str) -> reqwest::Response {
    server
        .client
        .get(server.url(&format!("/files/{id}/{v}/{name}")))
        .send()
        .await
        .unwrap()
}

/// Everything under the store's staging directory.
fn staged(server: &TestServer) -> Vec<PathBuf> {
    std::fs::read_dir(server.data().join("packages/.uploads"))
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The inbox
// ---------------------------------------------------------------------------

/// An inbox next to a running server, scanned on demand.
struct Inbox {
    dir: tempfile::TempDir,
    storage: FilesystemStorage,
    db: SqliteDatabase,
    files: yanuget::config::FilesConfig,
    feeds: Vec<String>,
    staging: PathBuf,
}

impl Inbox {
    async fn new(server: &TestServer) -> Self {
        let data = server.data();
        Self {
            dir: tempfile::tempdir().unwrap(),
            storage: FilesystemStorage::new(data.join("packages")).await.unwrap(),
            db: SqliteDatabase::connect(&data.join("yanuget.db").to_string_lossy())
                .await
                .unwrap(),
            files: yanuget::config::FilesConfig::default(),
            feeds: vec!["default".to_string()],
            staging: data.join("packages/.uploads"),
        }
    }

    /// The directory a file for `id`/`version` is dropped into.
    fn version_dir(&self, id: &str, version: &str) -> PathBuf {
        let dir = self.dir.path().join("default").join(id).join(version);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn scan(&self) -> yanuget::inbox::ScanReport {
        yanuget::inbox::Inbox {
            dir: self.dir.path(),
            storage: &self.storage,
            db: &self.db,
            files: &self.files,
            max_file_size: None,
            feeds: &self.feeds,
            staging: &self.staging,
        }
        .scan()
        .await
    }
}

const NOTHING: yanuget::inbox::ScanReport = yanuget::inbox::ScanReport {
    imported: 0,
    failed: 0,
};
const ONE_FAILED: yanuget::inbox::ScanReport = yanuget::inbox::ScanReport {
    imported: 0,
    failed: 1,
};

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_inbox_file_is_never_imported() {
    use std::os::unix::fs::symlink;
    let server = spawn().await;
    push(&server, build_nupkg("Drop.Pkg", "1.0.0")).await;
    let inbox = Inbox::new(&server).await;
    let dir = inbox.version_dir("Drop.Pkg", "1.0.0");

    // A file of the server's that the uploader could not read, and whose
    // content (and so hash) it knows.
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.wim");
    std::fs::write(&secret, b"the server's own file").unwrap();
    let sha = sha256_hex(b"the server's own file");
    symlink(&secret, dir.join("base.wim")).unwrap();
    std::fs::write(dir.join("base.wim.sha256"), format!("{sha}  base.wim\n")).unwrap();
    // The same through a linked version directory.
    std::fs::write(outside.path().join("other.wim.sha256"), &sha).unwrap();
    std::fs::copy(&secret, outside.path().join("other.wim")).unwrap();
    std::fs::remove_dir(inbox.version_dir("Drop.Pkg", "2.0.0")).unwrap();
    symlink(
        outside.path(),
        inbox.dir.path().join("default/Drop.Pkg/2.0.0"),
    )
    .unwrap();
    push(&server, build_nupkg("Drop.Pkg", "2.0.0")).await;

    assert_eq!(inbox.scan().await, NOTHING);
    for (v, name) in [("1.0.0", "base.wim"), ("2.0.0", "other.wim")] {
        let got = download(&server, "drop.pkg", v, name).await;
        assert_eq!(got.status(), reqwest::StatusCode::NOT_FOUND, "{v}/{name}");
    }
    assert!(
        yanuget::storage::PackageStorage::get_blob(&inbox.storage, &sha)
            .await
            .is_err(),
        "the link's target was stored"
    );
    // Nothing of the server's was touched, removed or reported into.
    assert!(secret.exists() && outside.path().join("other.wim").exists());
    assert!(!outside.path().join("other.wim.error").exists());
    assert!(staged(&server).is_empty(), "{:?}", staged(&server));
}

#[cfg(unix)]
#[tokio::test]
async fn a_hard_linked_inbox_file_is_refused() {
    let server = spawn().await;
    push(&server, build_nupkg("Drop.Pkg", "1.0.0")).await;
    let inbox = Inbox::new(&server).await;
    let dir = inbox.version_dir("Drop.Pkg", "1.0.0");
    let other = inbox.dir.path().join("elsewhere.wim");
    std::fs::write(&other, b"linked bytes").unwrap();
    std::fs::hard_link(&other, dir.join("base.wim")).unwrap();
    std::fs::write(dir.join("base.wim.sha256"), sha256_hex(b"linked bytes")).unwrap();

    assert_eq!(inbox.scan().await, ONE_FAILED);
    let why = std::fs::read_to_string(dir.join("base.wim.error")).unwrap();
    assert!(why.contains("hard links"), "{why}");
    let got = download(&server, "drop.pkg", "1.0.0", "base.wim").await;
    assert_eq!(got.status(), reqwest::StatusCode::NOT_FOUND);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_error_report_is_replaced_not_written_through() {
    use std::os::unix::fs::symlink;
    let server = spawn().await;
    push(&server, build_nupkg("Drop.Pkg", "1.0.0")).await;
    let inbox = Inbox::new(&server).await;
    let dir = inbox.version_dir("Drop.Pkg", "1.0.0");
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim.conf");
    std::fs::write(&victim, b"precious").unwrap();

    // Two files that will fail their checksum: one whose `.error` is a link
    // to an existing file of the server's, one whose `.error` dangles.
    for (name, target) in [
        ("a.wim", victim.clone()),
        ("b.wim", outside.path().join("created.conf")),
    ] {
        std::fs::write(dir.join(name), b"not what was promised").unwrap();
        std::fs::write(dir.join(format!("{name}.sha256")), "0".repeat(64)).unwrap();
        symlink(&target, dir.join(format!("{name}.error"))).unwrap();
    }

    let report = inbox.scan().await;
    assert_eq!(report.failed, 2, "{report:?}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    assert!(!outside.path().join("created.conf").exists());
    for name in ["a.wim", "b.wim"] {
        let report = dir.join(format!("{name}.error"));
        let meta = std::fs::symlink_metadata(&report).unwrap();
        assert!(meta.file_type().is_file(), "{name}: still a link");
        let why = std::fs::read_to_string(&report).unwrap();
        assert!(why.contains("checksum mismatch"), "{why}");
        // The report never says what the file's hash is.
        let actual = sha256_hex(b"not what was promised");
        assert!(!why.contains(&actual), "{why}");
        // The file itself stays where the uploader left it.
        assert!(dir.join(name).exists());
    }
    // And now that a real report is there, the next scan waits for it.
    assert_eq!(inbox.scan().await, NOTHING);
}

#[cfg(unix)]
#[tokio::test]
async fn an_inbox_file_changed_after_the_import_does_not_change_the_blob() {
    let server = spawn().await;
    push(&server, build_nupkg("Drop.Pkg", "1.0.0")).await;
    let inbox = Inbox::new(&server).await;
    let dir = inbox.version_dir("Drop.Pkg", "1.0.0");
    let good = b"verified content".to_vec();
    std::fs::write(dir.join("base.wim"), &good).unwrap();
    std::fs::write(dir.join("base.wim.sha256"), sha256_hex(&good)).unwrap();
    // The uploader keeps a handle open on the file it uploaded.
    let mut handle = std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("base.wim"))
        .unwrap();

    assert_eq!(inbox.scan().await.imported, 1);
    handle.write_all(b"TAMPERED").unwrap();
    handle.sync_all().unwrap();
    let got = download(&server, "drop.pkg", "1.0.0", "base.wim").await;
    assert_eq!(got.bytes().await.unwrap().as_ref(), good.as_slice());
    assert!(staged(&server).is_empty(), "{:?}", staged(&server));
}

// ---------------------------------------------------------------------------
// Ids the store cannot hold
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_id_that_is_a_windows_device_name_is_refused_up_front() {
    let server = spawn().await;
    for id in ["Aux.Core", "Con.Utils", "COM1.Sdk"] {
        let res = push(&server, build_nupkg(id, "1.0.0")).await;
        assert_eq!(res.status(), reqwest::StatusCode::BAD_REQUEST, "{id}");
        let body = res.text().await.unwrap();
        assert!(body.contains("Windows device name"), "{id}: {body}");
    }
    // A name that merely starts with the same letters is fine.
    let res = push(&server, build_nupkg("Console.Utils", "1.0.0")).await;
    assert_eq!(res.status(), reqwest::StatusCode::CREATED);
}

// ---------------------------------------------------------------------------
// Blobs shared between versions
// ---------------------------------------------------------------------------

async fn put_file(
    server: &TestServer,
    id: &str,
    v: &str,
    name: &str,
    body: Vec<u8>,
) -> reqwest::Response {
    server
        .client
        .put(server.url(&format!("/api/v2/files/{id}/{v}/{name}")))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn delete_file(server: &TestServer, id: &str, v: &str, name: &str) -> reqwest::Response {
    server
        .client
        .delete(server.url(&format!("/api/v2/files/{id}/{v}/{name}")))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap()
}

/// Every file under `dir`, recursively.
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[tokio::test]
async fn a_blob_stays_while_another_version_references_it() {
    let server = spawn_with(|c| c.hard_delete_enabled = true).await;
    for v in ["1.0.0", "2.0.0", "3.0.0"] {
        push(&server, build_nupkg("Shared.Img", v)).await;
    }
    let image = b"the same bytes, attached three times".to_vec();
    for v in ["1.0.0", "2.0.0", "3.0.0"] {
        let res = put_file(&server, "Shared.Img", v, "base.wim", image.clone()).await;
        assert_eq!(res.status(), reqwest::StatusCode::CREATED);
    }
    // Detached from one version and purged with another, the bytes stay for
    // the third.
    let res = delete_file(&server, "Shared.Img", "1.0.0", "base.wim").await;
    assert_eq!(res.status(), reqwest::StatusCode::NO_CONTENT);
    let res = server
        .client
        .delete(server.url("/api/v2/package/shared.img/2.0.0"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::NO_CONTENT);
    let got = download(&server, "shared.img", "3.0.0", "base.wim").await;
    assert_eq!(got.bytes().await.unwrap().as_ref(), image.as_slice());
    // The last reference takes the blob with it.
    delete_file(&server, "Shared.Img", "3.0.0", "base.wim").await;
    let left = walk(&server.data().join("packages/.blobs"));
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn detaching_from_one_version_never_deletes_a_blob_another_is_attaching() {
    let server = spawn().await;
    push(&server, build_nupkg("Race.Img", "1.0.0")).await;
    push(&server, build_nupkg("Race.Img", "2.0.0")).await;
    let image = vec![7u8; 64 * 1024];
    // Detaching the last reference from 1.0.0 races attaching the same bytes
    // to 2.0.0; whichever gets there first, 2.0.0 must end up with its file.
    for round in 0..20 {
        let res = put_file(&server, "Race.Img", "1.0.0", "a.wim", image.clone()).await;
        assert_eq!(res.status(), reqwest::StatusCode::CREATED);
        let (detached, attached) = tokio::join!(
            delete_file(&server, "Race.Img", "1.0.0", "a.wim"),
            put_file(&server, "Race.Img", "2.0.0", "b.wim", image.clone()),
        );
        assert_eq!(detached.status(), reqwest::StatusCode::NO_CONTENT);
        assert_eq!(attached.status(), reqwest::StatusCode::CREATED);
        let got = download(&server, "race.img", "2.0.0", "b.wim").await;
        assert_eq!(got.status(), reqwest::StatusCode::OK, "round {round}");
        assert_eq!(got.bytes().await.unwrap().len(), image.len());
        delete_file(&server, "Race.Img", "2.0.0", "b.wim").await;
    }
}
