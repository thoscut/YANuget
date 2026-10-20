//! The prelude in the package: the install script becomes a loader, the
//! package's own script moves beside it, and, where PowerShell is installed,
//! an install from a real YANuget server fetches the attached file instead
//! of the package's URL.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use reqwest::Url;
use sha2::{Digest, Sha256};
use yanuget::config::{Config, FeedConfig, FeedFilesConfig};
use yanuget::database::SqliteDatabase;
use yanuget::storage::FilesystemStorage;
use yanuget::web::{self, AppState, FeedMeta};
use yanuget_internalize::assess::Attachment;
use yanuget_internalize::config::{Secret, TargetConfig};
use yanuget_internalize::prelude::{self, PRELUDE_PATH};
use yanuget_internalize::publish::{Publisher, Release};
use yanuget_internalize::report::Draft;
use yanuget_pkg::report::{Checksum, ChecksumStatus, Download, PackageRef, Upstream};
use yanuget_pkg::rewrite::{self, Rewrite};

const PUSH_KEY: &str = "push-key";
const ID: &str = "Tool.Install";
const VERSION: &str = "1.0.0";
const INSTALLER: &[u8] = b"MZ the installer";

/// What the package's own script does: an install with a URL and a weak
/// checksum, positionally, as community packages write it.
const UPSTREAM: &str = "$toolsDir = Split-Path -Parent $MyInvocation.MyCommand.Definition\r\n\
Install-ChocolateyPackage 'tool' 'exe' '/S' 'https://example.com/setup.exe' -Checksum 'd41d8cd98f00b204e9800998ecf8427e' -ChecksumType md5\r\n\
\"toolsDir=$(Split-Path -Leaf $toolsDir)\"\r\n";

fn write_nupkg(path: &Path, entries: &[(&str, &str)]) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
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
    for (name, text) in entries {
        zip.start_file(*name, opts).unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

fn read_entries(path: &Path) -> Vec<(String, String)> {
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

fn downloads() -> Vec<Download> {
    vec![Download {
        url: "https://example.com/setup.exe".into(),
        helper: Some("Install-ChocolateyPackage".into()),
        file: Some("setup.exe".into()),
        size: Some(INSTALLER.len() as u64),
        sha256: Some(hex::encode(Sha256::digest(INSTALLER))),
        checksum: Checksum {
            status: ChecksumStatus::TrustOnFirstUse,
            algorithm: Some("md5".into()),
        },
    }]
}

fn feed() -> Url {
    Url::parse("https://nuget.example.com/choco/").unwrap()
}

#[tokio::test]
async fn the_install_script_loads_the_prelude_and_then_the_package_s_own() {
    let dir = tempfile::tempdir().unwrap();
    let (input, output) = (dir.path().join("in.nupkg"), dir.path().join("out.nupkg"));
    write_nupkg(
        &input,
        &[
            ("tools/ChocolateyInstall.ps1", UPSTREAM),
            ("tools/chocolateyUninstall.ps1", "# uninstall"),
        ],
    );
    let built = prelude::build(&input, ID, VERSION, &downloads(), &feed())
        .await
        .unwrap()
        .unwrap();
    let mut plan = Rewrite::new();
    for (name, source) in built.entries {
        plan = plan.put(name, source);
    }
    rewrite::rewrite(&input, &output, plan).await.unwrap();

    let entries = read_entries(&output);
    let get = |name: &str| {
        entries
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| panic!("{name} is missing"))
    };
    let loader = get("tools/ChocolateyInstall.ps1");
    assert!(loader.contains(". (Join-Path $PSScriptRoot '..\\internalizer\\prelude.ps1')"));
    assert!(loader.contains(". (Join-Path $PSScriptRoot 'chocolateyInstall.upstream.ps1')"));
    assert_eq!(get("tools/chocolateyInstall.upstream.ps1"), UPSTREAM);
    assert_eq!(get("tools/chocolateyUninstall.ps1"), "# uninstall");
    let text = get(PRELUDE_PATH);
    assert!(text.contains("files/tool.install/1.0.0/setup.exe"));
    assert!(!text.contains("example.com/setup.exe"));
    // No second install script for Chocolatey to pick up.
    let scripts = entries
        .iter()
        .filter(|(n, _)| n.to_ascii_lowercase().ends_with("chocolateyinstall.ps1"))
        .count();
    assert_eq!(scripts, 1);
}

#[tokio::test]
async fn a_package_the_prelude_cannot_go_into_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let nupkg = dir.path().join("in.nupkg");

    write_nupkg(&nupkg, &[("tools/readme.txt", "no script")]);
    let err = prelude::build(&nupkg, ID, VERSION, &downloads(), &feed())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("no chocolateyInstall.ps1"),
        "{err}"
    );

    for taken in [
        "internalizer/prelude.ps1",
        "tools/chocolateyInstall.upstream.ps1",
    ] {
        write_nupkg(
            &nupkg,
            &[("tools/chocolateyInstall.ps1", UPSTREAM), (taken, "x")],
        );
        let err = prelude::build(&nupkg, ID, VERSION, &downloads(), &feed())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("would replace"), "{taken}: {err}");
    }

    // Nothing attached, nothing to change: the package installs as it is.
    write_nupkg(&nupkg, &[("tools/readme.txt", "no script")]);
    assert!(prelude::build(&nupkg, ID, VERSION, &[], &feed())
        .await
        .unwrap()
        .is_none());
}

/// A server with one feed that serves what it is pushed, `.exe` files
/// allowed.
async fn server(dir: &Path) -> String {
    let config = Config {
        data_dir: dir.to_path_buf(),
        host: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        tls_enabled: false,
        api_key: Some(PUSH_KEY.into()),
        feeds: vec![FeedConfig {
            name: "choco".into(),
            files: FeedFilesConfig {
                allowed_extensions: Some(vec!["exe".into()]),
            },
            ..Default::default()
        }],
        ..Config::default()
    };
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
    format!("http://{addr}/choco/")
}

/// `pwsh` from `YANUGET_TEST_PWSH` or the `PATH`, if it runs.
fn pwsh() -> Option<PathBuf> {
    let exe =
        std::env::var_os("YANUGET_TEST_PWSH").map_or_else(|| PathBuf::from("pwsh"), PathBuf::from);
    let ok = Command::new(&exe)
        .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
        .output()
        .is_ok_and(|o| o.status.success());
    ok.then_some(exe)
}

/// A stand-in for Chocolatey's `chocolateyInstaller` module: the helper
/// downloads its URL and checks the checksum it is given, as the real one
/// does, and says what it got.
const MODULE: &str = r#"
function Install-ChocolateyPackage {
  param(
    [parameter(Mandatory=$true, Position=0)][string] $packageName,
    [parameter(Mandatory=$false, Position=1)][alias("installerType","installType")][string] $fileType = 'exe',
    [parameter(Mandatory=$false, Position=2)][string[]] $silentArgs = '',
    [parameter(Mandatory=$false, Position=3)][string] $url = '',
    [parameter(Mandatory=$false, Position=4)][alias("url64")][string] $url64bit = '',
    [parameter(Mandatory=$false)][string] $checksum = '',
    [parameter(Mandatory=$false)][string] $checksumType = '',
    [parameter(Mandatory=$false)][string] $checksum64 = '',
    [parameter(Mandatory=$false)][string] $checksumType64 = '',
    [parameter(ValueFromRemainingArguments = $true)][Object[]] $ignoredArguments
  )
  $out = Join-Path $env:YANUGET_TEST_DIR 'download.bin'
  Invoke-WebRequest -Uri $url -OutFile $out -UseBasicParsing
  $hash = (Get-FileHash -LiteralPath $out -Algorithm $checksumType).Hash.ToLowerInvariant()
  if ($hash -ne $checksum) { throw "checksum mismatch: $hash" }
  "installed $url ($checksumType)"
}
Export-ModuleMember -Function *
"#;

/// What Chocolatey does with a package's install script: import the helpers,
/// then run the script.
const RUNNER: &str = r#"
$ErrorActionPreference = 'Stop'
$env:ChocolateyPackageName = 'tool.install'
$env:ChocolateyPackageVersion = '1.0.0'
New-Module -Name chocolateyInstaller -ScriptBlock ([scriptblock]::Create((Get-Content -Raw (Join-Path $env:YANUGET_TEST_DIR 'module.ps1')))) | Import-Module
# The message on one line: PowerShell's own error view wraps it at the
# console's width.
try {
    & (Join-Path $env:YANUGET_TEST_DIR 'pkg/tools/chocolateyInstall.ps1')
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
}
"#;

#[tokio::test(flavor = "multi_thread")]
async fn an_install_fetches_the_attached_file() {
    let Some(pwsh) = pwsh() else {
        eprintln!("skipped: no pwsh (set YANUGET_TEST_PWSH)");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let feed = server(&dir.path().join("server")).await;

    // Publish the package with its prelude, as a run would.
    let nupkg = dir.path().join("upstream.nupkg");
    write_nupkg(&nupkg, &[("tools/chocolateyInstall.ps1", UPSTREAM)]);
    let blob = dir.path().join("blob");
    std::fs::write(&blob, INSTALLER).unwrap();
    let files = [Attachment {
        name: "setup.exe".into(),
        path: blob,
        size: INSTALLER.len() as u64,
        sha256: hex::encode(Sha256::digest(INSTALLER)),
    }];
    let mut draft = Draft::new(
        PackageRef {
            id: ID.into(),
            version: VERSION.into(),
        },
        Upstream {
            source: "https://community.chocolatey.org/api/v2/".into(),
            sha512: None,
        },
    );
    for d in downloads() {
        draft.download(d);
    }
    draft.prelude(PRELUDE_PATH);
    let report = draft.finish("2026-10-20T12:00:00Z");
    let built = prelude::build(
        &nupkg,
        ID,
        VERSION,
        &report.downloads,
        &Url::parse(&feed).unwrap(),
    )
    .await
    .unwrap()
    .unwrap();
    let publisher = Publisher::new(&TargetConfig {
        url: Some(feed.clone()),
        api_key: Some(Secret::new(PUSH_KEY)),
        timeout_secs: 30,
        ..TargetConfig::default()
    })
    .unwrap();
    let published = publisher
        .publish(
            Release {
                nupkg: &nupkg,
                report: &report,
                entries: built.entries,
                files: &files,
            },
            &dir.path().join("work"),
            false,
        )
        .await
        .unwrap();
    assert!(published.errors.is_empty(), "{:?}", published.errors);

    // Install what the feed serves.
    let bytes = reqwest::get(format!(
        "{feed}v3/package/tool.install/1.0.0/tool.install.1.0.0.nupkg"
    ))
    .await
    .unwrap()
    .bytes()
    .await
    .unwrap();
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    zip.extract(dir.path().join("pkg")).unwrap();
    std::fs::write(dir.path().join("module.ps1"), MODULE).unwrap();
    std::fs::write(dir.path().join("runner.ps1"), RUNNER).unwrap();

    let run = |extra: Vec<(&'static str, String)>| {
        let (pwsh, dir) = (pwsh.clone(), dir.path().to_path_buf());
        tokio::task::spawn_blocking(move || {
            let mut cmd = Command::new(pwsh);
            cmd.args(["-NoProfile", "-NonInteractive", "-File"])
                .arg(dir.join("runner.ps1"))
                .env("YANUGET_TEST_DIR", &dir)
                .env_remove("YANUGET_INTERNALIZED_FEED_URL");
            for (k, v) in extra {
                cmd.env(k, v);
            }
            cmd.output().unwrap()
        })
    };
    let out = run(vec![]).await.unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout.contains(&format!(
            "installed {feed}files/tool.install/1.0.0/setup.exe (sha256)"
        )),
        "{stdout}"
    );
    // The package's own script runs from its own folder.
    assert!(stdout.contains("toolsDir=tools"), "{stdout}");
    assert_eq!(
        std::fs::read(dir.path().join("download.bin")).unwrap(),
        INSTALLER
    );

    // After a host move, the client says where the feed is now; an address
    // that does not answer fails the install rather than reaching anywhere
    // else.
    let out = run(vec![(
        "YANUGET_INTERNALIZED_FEED_URL",
        "http://127.0.0.1:9/choco".into(),
    )])
    .await
    .unwrap();
    assert!(!out.status.success());

    // A URL the package did not have at capture time stops the install.
    let script = dir.path().join("pkg/tools/chocolateyInstall.upstream.ps1");
    std::fs::write(
        &script,
        "Install-ChocolateyPackage 'tool' -Url 'https://example.com/other.exe'\r\n",
    )
    .unwrap();
    let out = run(vec![]).await.unwrap();
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success());
    assert!(all.contains("which it does not carry"), "{all}");
    assert!(!all.contains("installed"), "{all}");
}
