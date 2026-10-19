//! Publishing against a real YANuget server: push, attach, the version held
//! back until it is approved, and what a re-run does.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use yanuget::config::{Config, FeedConfig, FeedFilesConfig, OverwriteMode};
use yanuget::database::{PackageDatabase, SqliteDatabase};
use yanuget::storage::FilesystemStorage;
use yanuget::web::{self, AppState, FeedMeta};
use yanuget_internalize::assess::Attachment;
use yanuget_internalize::config::{Secret, TargetConfig};
use yanuget_internalize::publish::{
    Attached, PublishError, Publisher, Pushed, Release, Visibility,
};
use yanuget_internalize::report::Draft;
use yanuget_pkg::report::{
    Checksum, ChecksumStatus, Download, PackageRef, Report, Upstream, REPORT_PATH,
};
use yanuget_pkg::rewrite::Source;
use yanuget_pkg::NuGetVersion;

const PUSH_KEY: &str = "push-key-of-the-intake-feed";
const READ_KEY: &str = "read-key";
const ID: &str = "Tool.Install";
const VERSION: &str = "1.0.0";

struct Server {
    base: String,
    db: Arc<SqliteDatabase>,
    _dir: tempfile::TempDir,
}

impl Server {
    fn feed(&self, name: &str) -> String {
        format!("{}/{name}/", self.base)
    }

    async fn status(&self, path: &str) -> reqwest::StatusCode {
        reqwest::get(format!("{}{path}", self.base))
            .await
            .unwrap()
            .status()
    }

    async fn approve(&self, feed: &str) {
        let v = NuGetVersion::parse(VERSION).unwrap();
        assert!(self.db.approve_membership(feed, ID, &v).await.unwrap());
    }
}

/// A server with one feed. The same package goes to one feed per server:
/// two feeds of a server share a version's bytes.
async fn server_of(feed: FeedConfig) -> Server {
    server(vec![feed]).await
}

/// A server with `feeds`, monitoring on.
async fn server(feeds: Vec<FeedConfig>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: dir.path().to_path_buf(),
        host: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        tls_enabled: false,
        api_key: Some(PUSH_KEY.into()),
        feeds,
        ..Config::default()
    };
    config.monitoring.enabled = true;
    let storage = Arc::new(FilesystemStorage::new(config.storage_path()).await.unwrap());
    let db = Arc::new(
        SqliteDatabase::connect(&config.database_path())
            .await
            .unwrap(),
    );
    let config = Arc::new(config);
    let resolved = config.resolved_feeds().unwrap();
    let meta = Arc::new(
        resolved
            .iter()
            .map(FeedMeta::from_resolved)
            .collect::<Vec<_>>(),
    );
    let mut states = Vec::new();
    for f in &resolved {
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
    Server {
        base: format!("http://{addr}"),
        db,
        _dir: dir,
    }
}

/// An intake feed: approval required, `.exe` and `.zip` files.
fn intake(name: &str) -> FeedConfig {
    FeedConfig {
        name: name.into(),
        requires_approval: true,
        files: FeedFilesConfig {
            allowed_extensions: Some(vec!["exe".into(), "zip".into()]),
        },
        ..Default::default()
    }
}

fn publisher(feed: String, key: &str) -> Publisher {
    Publisher::new(&TargetConfig {
        url: Some(feed),
        api_key: Some(Secret::new(key)),
        timeout_secs: 30,
        ..TargetConfig::default()
    })
    .unwrap()
}

fn write_nupkg(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("upstream.nupkg");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("[Content_Types].xml", opts).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="utf-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="nuspec" ContentType="application/octet" /><Default Extension="ps1" ContentType="application/octet" /></Types>"#,
    )
    .unwrap();
    zip.start_file(format!("{ID}.nuspec"), opts).unwrap();
    write!(
        zip,
        r#"<?xml version="1.0"?><package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd"><metadata><id>{ID}</id><version>{VERSION}</version><authors>a</authors><description>d</description></metadata></package>"#
    )
    .unwrap();
    zip.start_file("tools/chocolateyInstall.ps1", opts).unwrap();
    zip.write_all(b"Install-ChocolateyPackage tool exe '/S' https://example.com/setup.exe")
        .unwrap();
    zip.finish().unwrap();
    path
}

fn file(dir: &Path, name: &str, bytes: &[u8]) -> Attachment {
    let path = dir.join(format!("blob-{name}"));
    std::fs::write(&path, bytes).unwrap();
    Attachment {
        name: name.into(),
        path,
        size: bytes.len() as u64,
        sha256: hex::encode(Sha256::digest(bytes)),
    }
}

fn report(files: &[Attachment]) -> Report {
    let mut d = Draft::new(
        PackageRef {
            id: ID.into(),
            version: VERSION.into(),
        },
        Upstream {
            source: "https://community.chocolatey.org/api/v2/".into(),
            sha512: None,
        },
    );
    for f in files {
        d.download(Download {
            url: format!("https://example.com/{}", f.name),
            helper: Some("Install-ChocolateyPackage".into()),
            file: Some(f.name.clone()),
            size: Some(f.size),
            sha256: Some(f.sha256.clone()),
            checksum: Checksum {
                status: ChecksumStatus::TrustOnFirstUse,
                algorithm: None,
            },
        });
    }
    d.prelude("tools/internalizer-prelude.ps1");
    d.finish("2026-10-19T12:00:00Z")
}

fn release<'a>(nupkg: &'a Path, report: &'a Report, files: &'a [Attachment]) -> Release<'a> {
    Release {
        nupkg,
        report,
        entries: vec![(
            "tools/internalizer-prelude.ps1".into(),
            Source::Bytes(b"# prelude".to_vec()),
        )],
        files,
    }
}

#[tokio::test]
async fn a_package_waits_for_approval_with_its_report_and_files() {
    let server = server(vec![intake("intake")]).await;
    let dir = tempfile::tempdir().unwrap();
    let nupkg = write_nupkg(dir.path());
    let files = [
        file(dir.path(), "setup.exe", b"MZ an installer"),
        file(dir.path(), "data.zip", b"PK an archive"),
    ];
    let report = report(&files);
    let p = publisher(server.feed("intake"), PUSH_KEY);
    p.check_replace_pending().await.unwrap();

    let work = dir.path().join("work");
    let published = p
        .publish(release(&nupkg, &report, &files), &work, false)
        .await
        .unwrap();
    assert_eq!(published.pushed, Pushed::New);
    assert_eq!(published.visibility, Visibility::Withheld);
    assert_eq!(
        published.attached,
        [
            ("setup.exe".to_string(), Attached::New),
            ("data.zip".to_string(), Attached::New)
        ]
    );
    assert!(published.errors.is_empty() && published.warnings.is_empty());
    assert_eq!(
        std::fs::read_dir(&work).unwrap().count(),
        0,
        "the rewritten package is gone once pushed"
    );

    // Held back until approved, files included.
    let file_path = "/intake/files/tool.install/1.0.0/setup.exe";
    let package_path = "/intake/v3/package/tool.install/1.0.0/tool.install.1.0.0.nupkg";
    assert_eq!(server.status(file_path).await, 404);
    assert_eq!(server.status(package_path).await, 404);
    server.approve("intake").await;
    let got = reqwest::get(format!("{}{file_path}", server.base))
        .await
        .unwrap();
    assert_eq!(got.status(), 200);
    assert_eq!(got.bytes().await.unwrap().as_ref(), b"MZ an installer");

    // The package carries the report and the new entries.
    let bytes = reqwest::get(format!("{}{package_path}", server.base))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    let mut inside = Vec::new();
    zip.by_name(REPORT_PATH)
        .unwrap()
        .read_to_end(&mut inside)
        .unwrap();
    assert_eq!(Report::from_slice(&inside).unwrap(), report);
    assert!(zip.by_name("tools/internalizer-prelude.ps1").is_ok());
    assert!(zip.by_name("tools/chocolateyInstall.ps1").is_ok());

    // A re-run finds it approved and changes nothing.
    let more = [
        file(dir.path(), "setup.exe", b"MZ an installer"),
        file(dir.path(), "extra.zip", b"PK more"),
    ];
    let again = p
        .publish(release(&nupkg, &report, &more), &work, false)
        .await
        .unwrap();
    assert_eq!(again.pushed, Pushed::AlreadyThere);
    assert_eq!(again.visibility, Visibility::Served);
    assert!(again.attached.is_empty());
    assert_eq!(
        again.warnings,
        ["the feed serves the version already; nothing was attached to it"]
    );
    assert_eq!(
        server
            .status("/intake/files/tool.install/1.0.0/extra.zip")
            .await,
        404
    );
    // Asked to replace it, the run fails instead.
    let err = p
        .publish(release(&nupkg, &report, &more), &work, true)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("was not replaced"), "{err}");
}

#[tokio::test]
async fn a_rerun_completes_a_pending_version() {
    let server = server(vec![intake("intake")]).await;
    let dir = tempfile::tempdir().unwrap();
    let nupkg = write_nupkg(dir.path());
    let setup = file(dir.path(), "setup.exe", b"MZ an installer");
    let data = file(dir.path(), "data.zip", b"PK an archive");
    let report = report(&[setup.clone(), data.clone()]);
    let p = publisher(server.feed("intake"), PUSH_KEY);
    let work = dir.path().join("work");

    // The first run stopped after one file.
    let first = p
        .publish(
            release(&nupkg, &report, std::slice::from_ref(&setup)),
            &work,
            false,
        )
        .await
        .unwrap();
    assert_eq!(first.attached, [("setup.exe".to_string(), Attached::New)]);

    let files = [setup.clone(), data];
    let second = p
        .publish(release(&nupkg, &report, &files), &work, false)
        .await
        .unwrap();
    assert_eq!(second.pushed, Pushed::AlreadyThere);
    assert_eq!(second.visibility, Visibility::Withheld);
    assert_eq!(
        second.attached,
        [
            ("setup.exe".to_string(), Attached::Same),
            ("data.zip".to_string(), Attached::New)
        ]
    );

    // A file the feed does not take, and other bytes under a taken name.
    let refused = file(dir.path(), "setup.msi", b"\xD0\xCF\x11\xE0");
    let changed = Attachment {
        name: "setup.exe".into(),
        ..file(dir.path(), "changed.exe", b"MZ another installer")
    };
    let third = p
        .publish(release(&nupkg, &report, &[refused, changed]), &work, false)
        .await
        .unwrap();
    assert!(third.attached.is_empty());
    assert_eq!(third.errors.len(), 2, "{:?}", third.errors);
    assert!(
        third.errors[0].starts_with("attach setup.msi: the feed answered 400 Bad Request"),
        "{:?}",
        third.errors
    );
    assert!(
        third.errors[1].starts_with("attach setup.exe: the feed answered 409 Conflict"),
        "{:?}",
        third.errors
    );
}

#[tokio::test]
async fn pending_only_replaces_and_read_gated_feeds_are_not_attached_to_blind() {
    let mut replacing = intake("replacing");
    replacing.allow_overwrite = Some(OverwriteMode::PendingOnly);
    let mut gated = intake("gated");
    gated.read_api_key = Some(READ_KEY.into());
    let mut open = intake("open");
    open.requires_approval = false;
    let dir = tempfile::tempdir().unwrap();
    let nupkg = write_nupkg(dir.path());
    let files = [file(dir.path(), "setup.exe", b"MZ an installer")];
    let report = report(&files);
    let work = dir.path().join("work");

    // A pending version is replaced, with or without the flag.
    let server = server_of(replacing).await;
    let p = publisher(server.feed("replacing"), PUSH_KEY);
    for replace in [false, true] {
        let r = p
            .publish(release(&nupkg, &report, &files), &work, replace)
            .await
            .unwrap();
        assert_eq!(r.pushed, Pushed::New);
        assert_eq!(r.attached.len(), 1);
    }

    // A read-gated feed cannot tell: a new push is attached to, a version it
    // holds already is not.
    let server = server_of(gated).await;
    let p = publisher(server.feed("gated"), PUSH_KEY);
    let r = p
        .publish(release(&nupkg, &report, &files), &work, false)
        .await
        .unwrap();
    assert_eq!(
        (r.pushed, r.visibility, r.attached.len()),
        (Pushed::New, Visibility::Unknown, 1)
    );
    let r = p
        .publish(release(&nupkg, &report, &files), &work, false)
        .await
        .unwrap();
    assert_eq!(
        (r.pushed, r.visibility, r.attached.len()),
        (Pushed::AlreadyThere, Visibility::Unknown, 0)
    );
    assert!(r.warnings[0].contains("read-gated"), "{:?}", r.warnings);

    // A feed without approval serves the version at once, and says so.
    let server = server_of(open).await;
    let p = publisher(server.feed("open"), PUSH_KEY);
    let r = p
        .publish(release(&nupkg, &report, &files), &work, false)
        .await
        .unwrap();
    assert_eq!((r.pushed, r.visibility), (Pushed::New, Visibility::Served));
    assert_eq!(
        r.warnings,
        ["the feed served the version at once: it does not require approval"]
    );

    // The wrong key is named as such.
    let p = publisher(server.feed("open"), "not-the-key");
    let err = p
        .publish(release(&nupkg, &report, &files), &work, false)
        .await
        .unwrap_err();
    assert!(matches!(err, PublishError::Feed(_)), "{err}");
    assert!(err.to_string().contains("401"), "{err}");
    assert!(err.to_string().contains("target.api_key"), "{err}");
}

#[tokio::test]
async fn replace_pending_needs_a_server_that_can() {
    use axum::routing::get;
    let app = axum::Router::new().route(
        "/health/status",
        get(|| async { axum::Json(serde_json::json!({"status": "ok", "version": "0.7.0"})) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let p = publisher(format!("http://{addr}/intake/"), PUSH_KEY);
    let err = p.check_replace_pending().await.unwrap_err();
    assert!(err.to_string().contains("YANuget 0.8.0"), "{err}");
    assert!(err.to_string().contains("0.7.0"), "{err}");

    // A server that does not say passes.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, axum::Router::new()).await.unwrap() });
    let p = publisher(format!("http://{addr}/"), PUSH_KEY);
    p.check_replace_pending().await.unwrap();
}
