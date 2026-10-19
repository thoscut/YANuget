//! From a capture to reports and files: the fake sandbox's capture, the
//! downloads it asks for from a local host, and the names they get.

use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use sha2::{Digest, Sha256};
use yanuget_internalize::assess::{assess, fetch};
use yanuget_internalize::config::DownloadsConfig;
use yanuget_internalize::download::Downloader;
use yanuget_internalize::report::{self, Draft};
use yanuget_internalize::sandbox::fake::FakeSandbox;
use yanuget_internalize::sandbox::{CaptureJob, RawCapture, Sandbox};
use yanuget_pkg::report::{ChecksumStatus, Environment, PackageRef, Status, Upstream};
use yanuget_pkg::NuGetVersion;

const INSTALLER: &[u8] = b"MZ\x90\x00 an installer";
const ARCHIVE: &[u8] = b"PK\x03\x04 an archive";

async fn host() -> String {
    let app = Router::new()
        .route("/dl/Setup%201.0.exe", get(|| async { INSTALLER }))
        .route("/dl/tool.zip", get(|| async { ARCHIVE }))
        .route(
            "/dl/tool.msi",
            get(|| async { b"\xD0\xCF\x11\xE0 msi" as &[u8] }),
        )
        .route("/dl/gone.exe", get(|| async { StatusCode::GONE }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn draft(id: &str, version: &str) -> Draft {
    Draft::new(
        PackageRef {
            id: id.into(),
            version: version.into(),
        },
        Upstream {
            source: "https://community.chocolatey.org/api/v2/".into(),
            sha512: None,
        },
    )
}

fn call(package: &str, url: &str, extra: &str) -> String {
    format!(
        r#"{{"schema":1,"package":"{package}","version":"1.0.0","helper":"Install-ChocolateyPackage","url":"{url}","caller":"C:\\ProgramData\\chocolatey\\lib\\{package}\\tools\\chocolateyInstall.ps1"{extra}}}"#
    )
}

#[tokio::test]
async fn a_capture_becomes_reports_and_files() {
    let base = host().await;
    let sha = hex::encode(Sha256::digest(INSTALLER));
    let lines = [
        // Renamed: the URL's name has a space. url64 is the same download.
        call(
            "app",
            &format!("{base}/dl/Setup%201.0.exe"),
            &format!(
                r#","url64":"{base}/dl/Setup%201.0.exe","checksum":"{sha}","checksumType":"sha256","checksum64":"{sha}","checksumType64":"sha256""#
            ),
        ),
        call("app", &format!("{base}/dl/tool.zip"), ""),
        // The feed takes no .msi files here.
        call("other", &format!("{base}/dl/tool.msi"), ""),
        call("broken", &format!("{base}/dl/gone.exe"), ""),
    ];
    let sandbox = FakeSandbox::new().with(
        "app",
        "1.0.0",
        RawCapture {
            exit_code: Some(0),
            calls: lines.join("\r\n").into_bytes(),
            environment: Environment {
                image_sha256: Some("11".repeat(32)),
                chocolatey_version: Some("2.4.3".into()),
                tool_version: None,
            },
            ..RawCapture::default()
        },
    );
    let capture = sandbox
        .capture(&CaptureJob {
            package: PackageRef {
                id: "app".into(),
                version: "1.0.0".into(),
            },
            sources: Vec::new(),
            timeout: std::time::Duration::from_secs(60),
            max_capture_bytes: 1 << 20,
            package_parameters: None,
        })
        .await;

    let v = NuGetVersion::parse("1.0.0").unwrap();
    let closure = [("app", &v), ("other", &v), ("broken", &v), ("meta", &v)];
    let mut drafts = [
        draft("app", "1.0.0"),
        draft("other", "1.0.0"),
        draft("broken", "1.0.0"),
        draft("meta", "1.0.0"),
    ];
    let wanted = assess(capture, &closure, &mut drafts, 60);
    assert_eq!(
        wanted.iter().map(Vec::len).collect::<Vec<_>>(),
        [2, 1, 1, 0]
    );

    let cache = tempfile::tempdir().unwrap();
    let downloader = Downloader::new(
        &DownloadsConfig {
            allow_private_hosts: vec!["127.0.0.1".into()],
            timeout_secs: 5,
            ..DownloadsConfig::default()
        },
        cache.path(),
    )
    .unwrap();
    let allowed = ["exe".to_string(), "zip".to_string()];
    let files = fetch(wanted, &mut drafts, &downloader, &allowed, 2).await;
    let names: Vec<Vec<&str>> = files
        .iter()
        .map(|fs| fs.iter().map(|f| f.name.as_str()).collect())
        .collect();
    assert_eq!(
        names,
        [vec!["Setup_1.0.exe", "tool.zip"], vec![], vec![], vec![]]
    );
    assert_eq!(files[0][0].sha256, sha);
    assert_eq!(std::fs::read(&files[0][0].path).unwrap(), INSTALLER);

    let reports: Vec<_> = drafts.iter().map(|d| d.finish(report::now())).collect();
    let app = &reports[0];
    assert_eq!(app.status, Status::Internalized, "{:?}", app.reasons);
    assert_eq!(app.downloads.len(), 2);
    assert_eq!(app.downloads[0].checksum.status, ChecksumStatus::Verified);
    assert_eq!(
        app.downloads[1].checksum.status,
        ChecksumStatus::TrustOnFirstUse
    );
    assert_eq!(
        app.environment.as_ref().unwrap().image_sha256.as_deref(),
        Some("11".repeat(32).as_str())
    );

    let other = &reports[1];
    assert_eq!(other.status, Status::NeedsOverride);
    assert!(
        other.reasons[0].contains("is not attached"),
        "{:?}",
        other.reasons
    );
    assert_eq!(other.downloads[0].file, None);
    assert!(other.downloads[0].sha256.is_some());

    let broken = &reports[2];
    assert_eq!(broken.status, Status::Failed);
    assert!(broken.reasons[0].contains("410"), "{:?}", broken.reasons);
    assert!(broken.downloads.is_empty());

    assert_eq!(reports[3].status, Status::Unchanged);

    // As a pipeline reads it.
    let doc: serde_json::Value = serde_json::from_slice(&report::json(&reports)).unwrap();
    assert_eq!(doc["outcome"], "failed");
    assert_eq!(doc["packages"].as_array().unwrap().len(), 4);
}
