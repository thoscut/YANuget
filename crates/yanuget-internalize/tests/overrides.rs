//! An override's script replaces the package's own, under the name the
//! package gives it, and every field of an override takes effect on the way
//! from the capture job to the files and the prelude.

use std::io::{Read, Write};
use std::path::Path;

use axum::http::StatusCode;
use axum::routing::get;
use axum::Router;
use sha2::{Digest, Sha256};
use yanuget_internalize::assess::{assess, fetch};
use yanuget_internalize::config::{DownloadsConfig, SandboxConfig};
use yanuget_internalize::download::Downloader;
use yanuget_internalize::overrides::{with_script, Overrides};
use yanuget_internalize::prelude::{self, UPSTREAM_SCRIPT};
use yanuget_internalize::report::{self, Draft};
use yanuget_internalize::sandbox::fake::FakeSandbox;
use yanuget_internalize::sandbox::{self, RawCapture, Sandbox};
use yanuget_pkg::report::{ChecksumStatus, PackageRef, Status, Upstream};
use yanuget_pkg::rewrite::Source;
use yanuget_pkg::NuGetVersion;

fn nupkg(path: &Path, id: &str, script: Option<&str>) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("[Content_Types].xml", opts).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="utf-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="nuspec" ContentType="application/octet" /><Default Extension="ps1" ContentType="application/octet" /></Types>"#,
    )
    .unwrap();
    zip.start_file(format!("{id}.nuspec"), opts).unwrap();
    zip.write_all(
        format!("<package><metadata><id>{id}</id><version>1.0.0</version></metadata></package>")
            .as_bytes(),
    )
    .unwrap();
    if let Some(name) = script {
        zip.start_file(name, opts).unwrap();
        zip.write_all(b"Install-ChocolateyPackage upstream")
            .unwrap();
    }
    zip.finish().unwrap();
}

fn entries(path: &Path) -> Vec<(String, String)> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut out = Vec::new();
    for i in 0..zip.len() {
        let mut f = zip.by_index(i).unwrap();
        let mut text = String::new();
        f.read_to_string(&mut text).unwrap();
        out.push((f.name().unwrap().to_string(), text));
    }
    out
}

#[tokio::test]
async fn the_script_is_replaced_in_place_or_added() {
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.nupkg"), dir.path().join("out.nupkg"));

    nupkg(&input, "tool", Some("tools/ChocolateyInstall.ps1"));
    with_script(&input, &output, b"Write-Host internal".to_vec())
        .await
        .unwrap();
    let scripts: Vec<_> = entries(&output)
        .into_iter()
        .filter(|(n, _)| n.ends_with(".ps1"))
        .collect();
    assert_eq!(
        scripts,
        [(
            "tools/ChocolateyInstall.ps1".to_string(),
            "Write-Host internal".to_string()
        )]
    );

    nupkg(&input, "tool", None);
    with_script(&input, &output, b"Write-Host added".to_vec())
        .await
        .unwrap();
    assert!(entries(&output).contains(&(
        "tools/chocolateyInstall.ps1".to_string(),
        "Write-Host added".to_string()
    )));
}

const INSTALLER: &[u8] = b"MZ\x90\x00 from the mirror";
const CABINET: &[u8] = b"MSCF an extra cabinet";
const SCRIPT: &str = "Install-ChocolateyPackage internal";

async fn host() -> String {
    let app = Router::new()
        .route("/gone/setup.exe", get(|| async { StatusCode::GONE }))
        .route("/mirror/setup.exe", get(|| async { INSTALLER }))
        .route("/extra.cab", get(|| async { CABINET }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn draft(id: &str) -> Draft {
    Draft::new(
        PackageRef {
            id: id.into(),
            version: "1.0.0".into(),
        },
        Upstream {
            source: "https://community.chocolatey.org/api/v2/".into(),
            sha512: None,
        },
    )
}

#[tokio::test]
async fn every_field_of_an_override_takes_effect() {
    let base = host().await;
    let dir = tempfile::tempdir().unwrap();
    let overrides_dir = dir.path().join("overrides");
    std::fs::create_dir(&overrides_dir).unwrap();
    std::fs::write(overrides_dir.join("app.ps1"), SCRIPT).unwrap();
    std::fs::write(
        overrides_dir.join("app.toml"),
        format!(
            r#"
package_parameters = "/Quiet"
script = "app.ps1"

[[download]]
url = "{base}/extra.cab"
sha256 = "{}"
file_type = "cab"

[[rewrite]]
from = "{base}/gone/setup.exe"
to = "{base}/mirror/setup.exe"
"#,
            hex::encode(Sha256::digest(CABINET))
        ),
    )
    .unwrap();
    // Only the root's install gets --params.
    std::fs::write(
        overrides_dir.join("dep.toml"),
        "package_parameters = \"/Ignored\"",
    )
    .unwrap();
    let overrides = Overrides::load(&overrides_dir).unwrap();

    let (app, dep) = (dir.path().join("app.nupkg"), dir.path().join("dep.nupkg"));
    nupkg(&app, "app", Some("tools/chocolateyInstall.ps1"));
    nupkg(&dep, "dep", Some("tools/chocolateyInstall.ps1"));
    let v = NuGetVersion::parse("1.0.0").unwrap();
    let job = sandbox::job(
        &SandboxConfig::default(),
        &overrides,
        &[("app", &v, &app), ("dep", &v, &dep)],
        &dir.path().join("work"),
    )
    .await
    .unwrap();

    // The root is installed with its parameters, from a copy that holds the
    // override's script; the dependency and the original stay as they are.
    assert_eq!(job.package.id, "app");
    assert_eq!(job.package_parameters.as_deref(), Some("/Quiet"));
    assert_ne!(job.sources[0], app);
    let script = |path: &Path| {
        entries(path)
            .into_iter()
            .find(|(n, _)| n == "tools/chocolateyInstall.ps1")
            .map(|(_, text)| text)
    };
    assert_eq!(script(&job.sources[0]).as_deref(), Some(SCRIPT));
    assert_eq!(
        script(&app).as_deref(),
        Some("Install-ChocolateyPackage upstream")
    );
    assert_eq!(job.sources[1], dep);

    let gone = format!("{base}/gone/setup.exe");
    let sandbox = FakeSandbox::new().with(
        "app",
        "1.0.0",
        RawCapture {
            exit_code: Some(0),
            calls: format!(
                r#"{{"schema":1,"package":"app","version":"1.0.0","helper":"Install-ChocolateyPackage","url":"{gone}","caller":"C:\\ProgramData\\chocolatey\\lib\\app\\tools\\chocolateyInstall.ps1"}}"#
            )
            .into_bytes(),
            ..RawCapture::default()
        },
    );
    let capture = sandbox.capture(&job).await;
    assert_eq!(sandbox.jobs(), std::slice::from_ref(&job));

    let mut drafts = [draft("app"), draft("dep")];
    let wanted = assess(
        capture,
        &[("app", &v), ("dep", &v)],
        &mut drafts,
        60,
        &overrides,
    );
    let downloader = Downloader::new(
        &DownloadsConfig {
            allow_private_hosts: vec!["127.0.0.1".into()],
            timeout_secs: 5,
            ..DownloadsConfig::default()
        },
        &dir.path().join("cache"),
    )
    .unwrap();
    let allowed = ["exe".to_string(), "cab".to_string()];
    let files = fetch(wanted, &mut drafts, &downloader, &allowed, 2).await;

    // The captured URL is gone; the rewrite fetched it from the mirror.
    let names: Vec<&str> = files[0].iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["setup.exe", "extra.cab"]);
    assert_eq!(std::fs::read(&files[0][0].path).unwrap(), INSTALLER);
    assert_eq!(std::fs::read(&files[0][1].path).unwrap(), CABINET);

    let reports: Vec<_> = drafts.iter().map(|d| d.finish(report::now())).collect();
    let r = &reports[0];
    assert_eq!(r.status, Status::Internalized, "{:?}", r.reasons);
    assert_eq!(
        r.overrides,
        [
            "chocolateyInstall.ps1 replaced by app.ps1".to_string(),
            "package parameters \"/Quiet\"".to_string(),
            format!("{gone} fetched from {base}/mirror/setup.exe"),
            format!("download {base}/extra.cab added"),
        ]
    );
    // The package keeps asking for the captured URL; the added download is
    // verified by the override's SHA-256.
    assert_eq!(r.downloads[0].url, gone);
    assert_eq!(r.downloads[1].helper.as_deref(), Some("override"));
    assert_eq!(r.downloads[1].checksum.status, ChecksumStatus::Verified);
    let dep_report = &reports[1];
    assert!(dep_report.overrides.is_empty());
    assert!(
        dep_report
            .warnings
            .iter()
            .any(|w| w.contains("\"/Ignored\" were not passed")),
        "{:?}",
        dep_report.warnings
    );

    // The feed gets the script the capture ran, behind the prelude, and the
    // prelude maps the captured URL to the attached file.
    let feed = reqwest::Url::parse("https://nuget.example.com/choco/").unwrap();
    let built = prelude::build(&job.sources[0], "app", "1.0.0", &r.downloads, &feed)
        .await
        .unwrap()
        .unwrap();
    let upstream = built
        .entries
        .iter()
        .find(|(n, _)| *n == format!("tools/{UPSTREAM_SCRIPT}"))
        .map(|(_, s)| s);
    assert!(
        matches!(upstream, Some(Source::Bytes(b)) if b == SCRIPT.as_bytes()),
        "{upstream:?}"
    );
}
