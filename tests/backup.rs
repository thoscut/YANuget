//! `yanuget backup` and `yanuget restore` end to end: a backup taken from a
//! running server restores into a server that serves the same packages.

use std::io::{Cursor, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

use indicatif::ProgressDrawTarget;
use yanuget::config::Config;
use yanuget::database::SqliteDatabase;
use yanuget::storage::FilesystemStorage;
use yanuget::web::{self, AppState, FeedMeta};
use zip::write::SimpleFileOptions;

const API_KEY: &str = "test-key";

fn config_for(dir: &Path) -> Config {
    Config {
        data_dir: dir.to_path_buf(),
        host: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        tls_enabled: false,
        api_key: Some(API_KEY.into()),
        ..Config::default()
    }
}

/// Serve `config` and return its base URL.
async fn serve(config: Config) -> String {
    let storage = Arc::new(FilesystemStorage::new(config.storage_path()).await.unwrap());
    let db = Arc::new(
        SqliteDatabase::connect(&config.database_path())
            .await
            .unwrap(),
    );
    let config = Arc::new(config);
    let feeds = config.resolved_feeds().unwrap();
    let meta = Arc::new(
        feeds
            .iter()
            .map(FeedMeta::from_resolved)
            .collect::<Vec<_>>(),
    );
    let mut states = Vec::new();
    for f in &feeds {
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
    format!("http://{addr}")
}

fn build_nupkg(id: &str, version: &str) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Test Author</authors>
    <description>A backup test package.</description>
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
        zip.write_all(format!("{id} {version}").as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

async fn push(base: &str, id: &str, version: &str) {
    let status = reqwest::Client::new()
        .put(format!("{base}/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(build_nupkg(id, version))
        .send()
        .await
        .unwrap()
        .status();
    assert!(status.is_success(), "push {id} {version}: {status}");
}

async fn download(base: &str, id: &str, version: &str) -> Option<Vec<u8>> {
    let lower = id.to_ascii_lowercase();
    let resp = reqwest::get(format!(
        "{base}/v3/package/{lower}/{version}/{lower}.{version}.nupkg"
    ))
    .await
    .unwrap();
    resp.status()
        .is_success()
        .then_some(resp.bytes().await.unwrap().to_vec())
}

fn hidden() -> ProgressDrawTarget {
    ProgressDrawTarget::hidden()
}

#[tokio::test]
async fn a_backup_of_a_running_server_restores_into_one_that_serves_the_same_packages() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    push(&base, "Beta", "2.0.0").await;

    let backups = tempfile::tempdir().unwrap();
    let dest = backups.path().join("nightly");
    let first = yanuget::backup::backup(&config_for(live.path()), &dest, hidden())
        .await
        .unwrap();
    assert_eq!(first.manifest.packages, 2);
    assert!(
        first.manifest.missing.is_empty(),
        "{:?}",
        first.manifest.missing
    );
    assert!(first.copied > 0);
    assert!(dest.join("backup.json").is_file());
    assert!(!dest.join("packages/.uploads").exists());

    // Again, after one more push: only the new version's files are copied.
    push(&base, "Gamma", "3.0.0").await;
    let second = yanuget::backup::backup(&config_for(live.path()), &dest, hidden())
        .await
        .unwrap();
    assert_eq!(second.manifest.packages, 3);
    assert!(second.copied > 0 && second.copied < first.copied + 3);
    assert_eq!(second.attempts, 1);

    // Restore into an empty data directory and serve it.
    let restored = tempfile::tempdir().unwrap();
    let summary = yanuget::backup::restore(&config_for(restored.path()), &dest, false, hidden())
        .await
        .unwrap();
    assert_eq!(summary.manifest.packages, 3);
    let base2 = serve(config_for(restored.path())).await;
    for (id, version) in [("Alpha", "1.0.0"), ("Beta", "2.0.0"), ("Gamma", "3.0.0")] {
        assert_eq!(
            download(&base2, id, version).await,
            download(&base, id, version).await,
            "{id} {version}"
        );
        assert!(download(&base2, id, version).await.is_some());
    }
}

#[tokio::test]
async fn restore_refuses_to_overwrite_data_unless_forced() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    let backups = tempfile::tempdir().unwrap();
    yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();

    // A target that already has a different package.
    let target = tempfile::tempdir().unwrap();
    let other = serve(config_for(target.path())).await;
    push(&other, "Stale", "9.0.0").await;

    let err = yanuget::backup::restore(&config_for(target.path()), backups.path(), false, hidden())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("--force"), "{err}");

    let summary =
        yanuget::backup::restore(&config_for(target.path()), backups.path(), true, hidden())
            .await
            .unwrap();
    assert!(summary.removed > 0, "the stale package's files go");
    assert!(!target.path().join("packages/stale").exists());
    assert!(target
        .path()
        .join("packages/alpha/1.0.0/alpha.1.0.0.nupkg")
        .is_file());
}

#[tokio::test]
async fn a_file_missing_from_the_live_store_is_reported_not_hidden() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    push(&base, "Beta", "1.0.0").await;
    std::fs::remove_file(live.path().join("packages/beta/1.0.0/beta.1.0.0.nupkg")).unwrap();

    let backups = tempfile::tempdir().unwrap();
    let summary = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    assert_eq!(summary.manifest.missing, ["beta/1.0.0/beta.1.0.0.nupkg"]);
    assert_eq!(summary.attempts, 3, "it retried before settling");
}
