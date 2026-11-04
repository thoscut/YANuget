//! Migration from an upstream feed end to end.

mod common;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use yanuget::config::{Config, MirrorConfig};
use yanuget::database::{PackageDatabase, SqliteDatabase};
use yanuget::migrate::MigrateOptions;
use yanuget::storage::FilesystemStorage;
use yanuget::version::NuGetVersion;

use common::*;

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

/// Run `yanuget migrate` as the real binary, the way an operator or a script
/// would, into a data directory under `dir`.
async fn run_migrate_command(
    dir: &std::path::Path,
    source: &TestServer,
    extra: &[&str],
) -> std::process::Output {
    let config = dir.join("yanuget.toml");
    // A TOML literal string takes a Windows path's backslashes as they are.
    std::fs::write(
        &config,
        format!(
            "data_dir = '{}'\ntls_enabled = false\n",
            dir.join("data").display()
        ),
    )
    .unwrap();
    tokio::process::Command::new(env!("CARGO_BIN_EXE_yanuget"))
        .arg("--config")
        .arg(&config)
        .arg("migrate")
        .arg("--source")
        .arg(source.url("/v3/index.json"))
        .args(extra)
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn migrate_command_fails_when_any_version_failed() {
    // A partial copy has to fail the command. Scripts gate on its exit code
    // ("stop the old server once the copy is done"), and a real run that
    // reported "139 imported, 0 skipped, 20 failed" exited 0.
    let source = spawn().await;
    let small = build_nupkg("Exit.Small", "1.0.0", b"x");
    // Filler deflate cannot shrink much, so this package stays the larger one.
    let filler: Vec<u8> = (0..64 * 1024u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    let large = build_nupkg("Exit.Large", "1.0.0", &filler);
    let cap = (small.len() + large.len()) / 2;
    assert!(small.len() < cap && cap < large.len());
    for nupkg in [small, large] {
        let resp = push_multipart(&source, API_KEY, nupkg).await;
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    }

    // Over the size cap, the large package fails; the small one is imported.
    let target = tempfile::tempdir().unwrap();
    let cap = cap.to_string();
    let partial =
        run_migrate_command(target.path(), &source, &["--max-package-size-bytes", &cap]).await;
    let stderr = String::from_utf8_lossy(&partial.stderr);
    assert!(!partial.status.success(), "partial run exited 0: {stderr}");
    assert!(stderr.contains("migration incomplete"), "{stderr}");

    // Without the cap, a re-run retries only what failed, and succeeds.
    let rerun = run_migrate_command(target.path(), &source, &[]).await;
    let stdout = String::from_utf8_lossy(&rerun.stdout);
    assert!(
        rerun.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert!(
        stdout.contains("1 imported, 1 skipped, 0 failed"),
        "{stdout}"
    );
}

/// A source feed holding one package, `Slow.Package` 1.0.0, whose `.nupkg` it
/// sends in `chunks` pieces with `pause` between them: steady, but slow.
/// Returns the service-index URL.
async fn slow_upstream(chunks: usize, pause: std::time::Duration) -> String {
    use axum::body::{Body, Bytes};
    use axum::extract::Query;
    use axum::routing::get;
    use axum::{Json, Router};
    use futures::StreamExt;
    use std::collections::HashMap;

    let nupkg = build_nupkg("Slow.Package", "1.0.0", &[7u8; 16 * 1024]);
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let index = serde_json::json!({
        "version": "3.0.0",
        "resources": [
            {"@id": format!("{base}/flat/"), "@type": "PackageBaseAddress/3.0.0"},
            {"@id": format!("{base}/query"), "@type": "SearchQueryService"}
        ]
    });
    let app = Router::new()
        .route(
            "/v3/index.json",
            get(move || {
                let index = index.clone();
                async move { Json(index) }
            }),
        )
        .route(
            "/query",
            get(|Query(q): Query<HashMap<String, String>>| async move {
                let first = q.get("skip").is_none_or(|s| s == "0");
                let data = if first {
                    serde_json::json!([{"id": "Slow.Package", "version": "1.0.0"}])
                } else {
                    serde_json::json!([])
                };
                Json(serde_json::json!({"totalHits": 1, "data": data}))
            }),
        )
        .route(
            "/flat/slow.package/index.json",
            get(|| async { Json(serde_json::json!({"versions": ["1.0.0"]})) }),
        )
        .route(
            "/flat/slow.package/1.0.0/slow.package.1.0.0.nupkg",
            get(move || {
                let nupkg = nupkg.clone();
                async move {
                    let size = nupkg.len().div_ceil(chunks);
                    let pieces: Vec<Bytes> =
                        nupkg.chunks(size).map(Bytes::copy_from_slice).collect();
                    let body = futures::stream::iter(pieces).then(move |piece| async move {
                        tokio::time::sleep(pause).await;
                        Ok::<_, std::io::Error>(piece)
                    });
                    axum::response::Response::builder()
                        .header("content-length", nupkg.len())
                        .body(Body::from_stream(body))
                        .unwrap()
                }
            }),
        );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("{base}/v3/index.json")
}

#[tokio::test]
async fn migrate_downloads_packages_that_take_longer_than_the_timeout() {
    // A whole-transfer deadline of `timeout_secs` failed every package larger
    // than the source could send in that time: at ~2 MiB/s and the default
    // 60 s, anything past ~120 MB. Eight pieces 300 ms apart take 2.4 s against
    // a 1 s timeout, while no single pause comes near it.
    let upstream = slow_upstream(8, std::time::Duration::from_millis(300)).await;
    let target = migrate_target().await;
    let feeds = target.config.resolved_feeds().unwrap();
    let source = MirrorConfig {
        enabled: true,
        upstream,
        timeout_secs: 1,
        allow_private_upstream: true,
        ..Default::default()
    };
    let summary = yanuget::migrate::run(
        &target.storage,
        &target.db,
        &feeds[0],
        &target.temp_dir,
        source,
        MigrateOptions {
            quiet: true,
            ..Default::default()
        },
        indicatif::ProgressDrawTarget::hidden(),
    )
    .await
    .unwrap();
    assert_eq!(
        (summary.imported, summary.failed),
        (1, 0),
        "{:?}",
        summary.failures
    );
}

#[tokio::test]
async fn read_through_mirror_still_gives_up_on_a_slow_download() {
    // The read-through mirror is started by an anonymous request, so it keeps
    // its total deadline: an upstream that trickles must not hold the fetch.
    let upstream = slow_upstream(8, std::time::Duration::from_millis(300)).await;
    let target = migrate_target().await;
    let client = yanuget::mirror::MirrorClient::from_config(&MirrorConfig {
        enabled: true,
        upstream,
        timeout_secs: 1,
        download_timeout_secs: 1,
        allow_private_upstream: true,
        ..Default::default()
    })
    .unwrap();
    let mirrored = yanuget::mirror::ensure_package(
        &client,
        &target.storage,
        &target.db,
        "default",
        &target.temp_dir,
        "Slow.Package",
        &yanuget::mirror::MirrorOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(mirrored, 0);
}
