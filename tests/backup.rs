//! `yanuget backup` and `yanuget restore` end to end: a backup taken from a
//! running server restores into a server that serves the same packages.

mod common;

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
    let space = Arc::new(yanuget::space::SpaceGuard::for_store(&config, db.clone()));
    let mut states = Vec::new();
    for f in &feeds {
        states.push(
            AppState::for_feed(
                storage.clone(),
                db.clone(),
                config.clone(),
                f,
                meta.clone(),
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
    put(base, "/api/v2/package", build_nupkg(id, version)).await;
}

/// PUT `body` to `path` with the push key, which must succeed.
async fn put(base: &str, path: &str, body: Vec<u8>) {
    let status = common::client()
        .put(format!("{base}{path}"))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(body)
        .send()
        .await
        .unwrap()
        .status();
    assert!(status.is_success(), "PUT {path}: {status}");
}

/// GET `path`, and the body when it succeeds.
async fn get(base: &str, path: &str) -> Option<Vec<u8>> {
    let resp = common::client()
        .get(format!("{base}{path}"))
        .send()
        .await
        .unwrap();
    if !resp.status().is_success() {
        return None;
    }
    Some(resp.bytes().await.unwrap().to_vec())
}

async fn download(base: &str, id: &str, version: &str) -> Option<Vec<u8>> {
    let lower = id.to_ascii_lowercase();
    let resp = common::client()
        .get(format!(
            "{base}/v3/package/{lower}/{version}/{lower}.{version}.nupkg"
        ))
        .send()
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

/// Same size, other bytes: what an overwrite during the copy can leave. The
/// size check passes it; the hash of a file the run copied does not.
#[tokio::test]
async fn a_copied_file_with_the_right_size_but_other_bytes_fails_verification() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    let backups = tempfile::tempdir().unwrap();
    yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();

    let rel = "alpha/1.0.0/alpha.1.0.0.nupkg";
    let copy = backups.path().join("packages").join(rel);
    let len = std::fs::metadata(&copy).unwrap().len() as usize;
    std::fs::write(&copy, vec![b'x'; len]).unwrap();

    let (db, store) = (
        backups.path().join("yanuget.db"),
        backups.path().join("packages"),
    );
    let skipped = std::collections::HashSet::new();
    assert!(yanuget::backup::verify(&db, &store, &skipped)
        .await
        .unwrap()
        .is_empty());
    let copied = [std::path::PathBuf::from(rel)].into();
    assert_eq!(
        yanuget::backup::verify(&db, &store, &copied).await.unwrap(),
        [rel]
    );
}

/// A run into an existing backup skips what has the live file's size and
/// modification time, and does not hash it: verifying covers what the run
/// copied, not every byte the backup holds (`TEST-16`). A copy damaged in the
/// backup with its size and time intact goes unnoticed, and so does a live
/// file rewritten in place with both kept.
#[tokio::test]
async fn a_second_run_does_not_hash_what_it_skips() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    push(&base, "Beta", "1.0.0").await;
    let backups = tempfile::tempdir().unwrap();
    let first = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    assert!(first.manifest.missing.is_empty());

    // Same length and time, other bytes: once in the backup, once live.
    let rewrite = |path: &Path| {
        let meta = std::fs::metadata(path).unwrap();
        std::fs::write(path, vec![b'x'; meta.len() as usize]).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(meta.modified().unwrap())
            .unwrap();
    };
    let alpha = "alpha/1.0.0/alpha.1.0.0.nupkg";
    let copy = backups.path().join("packages").join(alpha);
    rewrite(&copy);
    rewrite(&live.path().join("packages/beta/1.0.0/beta.1.0.0.nupkg"));

    let second = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    assert!(
        second.manifest.missing.is_empty(),
        "{:?}",
        second.manifest.missing
    );
    assert!(
        std::fs::read(&copy).unwrap().iter().all(|b| *b == b'x'),
        "the damaged copy was skipped, not copied again"
    );
}

/// A live file whose bytes no longer match its row is copied, hashed, and
/// reported like a missing one.
#[tokio::test]
async fn a_live_file_that_does_not_match_its_row_is_reported() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    push(&base, "Beta", "1.0.0").await;
    let file = live.path().join("packages/beta/1.0.0/beta.1.0.0.nupkg");
    let len = std::fs::metadata(&file).unwrap().len() as usize;
    std::fs::write(&file, vec![b'x'; len]).unwrap();

    let backups = tempfile::tempdir().unwrap();
    let summary = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    assert_eq!(summary.manifest.missing, ["beta/1.0.0/beta.1.0.0.nupkg"]);
    assert_eq!(summary.attempts, 3, "it retried before settling");
}

/// `sudo -u yanuget yanuget restore` runs wherever the operator stands, not in
/// the service's working directory, so a relative `data_dir` would restore
/// there and report success. It is refused before anything is written.
#[tokio::test]
async fn a_restore_into_a_relative_path_is_refused() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    let backups = tempfile::tempdir().unwrap();
    yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();

    let relative = std::path::PathBuf::from(format!("restore-target-{}", std::process::id()));
    let config = Config {
        data_dir: relative.clone(),
        ..config_for(live.path())
    };
    let err = yanuget::backup::restore(&config, backups.path(), true, hidden())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("data_dir is the relative path"), "{err}");
    assert!(err.contains("--config"), "{err}");
    assert!(!relative.exists(), "it wrote into the current directory");

    // An override of its own is named as such.
    let restored = tempfile::tempdir().unwrap();
    let config = Config {
        storage_path: Some(relative.join("packages")),
        ..config_for(restored.path())
    };
    let err = yanuget::backup::restore(&config, backups.path(), true, hidden())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("storage_path is the relative path"), "{err}");
    assert!(!relative.exists());
    assert_eq!(
        yanuget::backup::target(&config_for(restored.path())),
        restored.path().display().to_string()
    );
}

/// The package store can live outside the data directory. A backup inside it
/// — or one that contains it — used to be refused only relative to the data
/// directory, and the copy then recursed into itself.
#[tokio::test]
async fn a_backup_and_a_store_outside_the_data_directory_must_not_contain_each_other() {
    let live = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let store = elsewhere.path().join("store");
    let config = Config {
        storage_path: Some(store.clone()),
        ..config_for(live.path())
    };
    let base = serve(config.clone()).await;
    push(&base, "Alpha", "1.0.0").await;

    for dest in [store.join("backup"), elsewhere.path().to_path_buf()] {
        let err = yanuget::backup::backup(&config, &dest, hidden())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("must not contain each other"), "{err}");
    }

    let backups = tempfile::tempdir().unwrap();
    yanuget::backup::backup(&config, backups.path(), hidden())
        .await
        .unwrap();
    // Restoring from a copy inside the store is refused too: the prune after
    // the copy would delete the backup being restored.
    let inside = store.join("copy");
    std::fs::create_dir_all(&inside).unwrap();
    for name in ["backup.json", "yanuget.db"] {
        std::fs::copy(backups.path().join(name), inside.join(name)).unwrap();
    }
    let err = yanuget::backup::restore(&config, &inside, true, hidden())
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("must not contain each other"), "{err}");
}

/// `restore --force` makes the store match the backup exactly, so a live file
/// damaged in place — same size, same modification time — is replaced rather
/// than taken for the backup's copy.
#[tokio::test]
async fn a_forced_restore_replaces_a_file_damaged_in_place() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    let backups = tempfile::tempdir().unwrap();
    yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();

    let file = live.path().join("packages/alpha/1.0.0/alpha.1.0.0.nupkg");
    let good = std::fs::read(&file).unwrap();
    let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
    let mut bad = good.clone();
    bad[0] ^= 0xff;
    std::fs::write(&file, &bad).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(modified)
        .unwrap();

    let summary =
        yanuget::backup::restore(&config_for(live.path()), backups.path(), true, hidden())
            .await
            .unwrap();
    assert!(summary.copied > 0);
    assert_eq!(std::fs::read(&file).unwrap(), good);
}

/// `COR-46`: a run into an existing backup used to check the copy before it
/// pruned it. A file the live store had lost passed the check on the earlier
/// run's copy, the prune then deleted that copy, and the manifest said
/// nothing was missing. The copy is the last one there is, so it stays.
#[tokio::test]
async fn a_second_run_keeps_the_last_copy_of_a_file_the_live_store_lost() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    push(&base, "Beta", "1.0.0").await;
    let backups = tempfile::tempdir().unwrap();
    let first = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    assert!(first.manifest.missing.is_empty());

    let rel = "beta/1.0.0/beta.1.0.0.nupkg";
    let beta = download(&base, "Beta", "1.0.0").await.unwrap();
    std::fs::remove_file(live.path().join("packages").join(rel)).unwrap();

    let second = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    let copy = backups.path().join("packages").join(rel);
    assert!(copy.is_file(), "the last copy was pruned");
    assert!(
        second.manifest.missing.is_empty(),
        "{:?}",
        second.manifest.missing
    );
    assert_eq!(second.kept, [rel]);
    assert_eq!(second.attempts, 1);
    assert_eq!(second.manifest.files, first.manifest.files);

    // The backup restores a server that has the version, file and all.
    let restored = tempfile::tempdir().unwrap();
    yanuget::backup::restore(
        &config_for(restored.path()),
        backups.path(),
        false,
        hidden(),
    )
    .await
    .unwrap();
    let base2 = serve(config_for(restored.path())).await;
    assert_eq!(download(&base2, "Beta", "1.0.0").await, Some(beta));

    // Gone from the backup as well, it is reported.
    std::fs::remove_file(&copy).unwrap();
    let third = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    assert_eq!(third.manifest.missing, [rel]);
    assert!(third.kept.is_empty());
}

/// `COR-54`: a copy the backup keeps because the live store lost the file is
/// from an earlier run, and the version may have been overwritten since with
/// a build of the same size. It is hashed, and reported when it holds the old
/// build's bytes; it is kept all the same, being the last copy there is.
#[tokio::test]
async fn a_kept_copy_of_an_overwritten_build_is_hashed_and_reported() {
    let config = |dir: &Path| Config {
        allow_overwrite: yanuget::config::OverwriteMode::Enabled,
        ..config_for(dir)
    };
    // Stored, not deflated: two payloads of one length make two builds of
    // one size.
    let build = |payload: u8| {
        let mut cursor = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut cursor);
            let opts = SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .last_modified_time(zip::DateTime::default());
            zip.start_file("Alpha.nuspec", opts).unwrap();
            zip.write_all(
                br#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>Alpha</id>
    <version>1.0.0</version>
    <authors>Test Author</authors>
    <description>A backup test package.</description>
  </metadata>
</package>"#,
            )
            .unwrap();
            zip.start_file("lib/net8.0/Lib.dll", opts).unwrap();
            zip.write_all(&[payload; 64]).unwrap();
            zip.finish().unwrap();
        }
        cursor.into_inner()
    };
    let (old, new) = (build(b'a'), build(b'b'));
    assert_eq!(old.len(), new.len());
    let live = tempfile::tempdir().unwrap();
    let base = serve(config(live.path())).await;
    put(&base, "/api/v2/package", old.clone()).await;
    let backups = tempfile::tempdir().unwrap();
    let first = yanuget::backup::backup(&config(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    assert!(first.manifest.missing.is_empty());

    // Overwritten, then lost from the live store before the next run.
    put(&base, "/api/v2/package", new).await;
    let rel = "alpha/1.0.0/alpha.1.0.0.nupkg";
    std::fs::remove_file(live.path().join("packages").join(rel)).unwrap();

    let second = yanuget::backup::backup(&config(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    assert_eq!(second.manifest.missing, [rel]);
    assert_eq!(second.kept, [rel]);
    let copy = backups.path().join("packages").join(rel);
    assert_eq!(
        std::fs::read(copy).unwrap(),
        old,
        "the last copy was pruned"
    );
}

/// `SEC-54`: the backup directory may be a share others write to. A link
/// laid where `backup` stages the manifest, or in place of its lock file, is
/// not followed: whatever it points at stays as it was.
#[cfg(unix)]
#[tokio::test]
async fn a_backup_does_not_write_through_links_to_files_in_its_destination() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    let outside = tempfile::tempdir().unwrap();
    let victim = outside.path().join("victim.db");
    std::fs::write(&victim, b"precious").unwrap();

    let backups = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(&victim, backups.path().join("backup.json.backup-tmp")).unwrap();
    let summary = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    assert!(summary.manifest.missing.is_empty());
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    yanuget::backup::read_manifest(backups.path()).unwrap();
    // Nothing of the run is left at the top but the backup itself.
    let mut names: Vec<String> = std::fs::read_dir(backups.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [".backup.lock", "backup.json", "packages", "yanuget.db"]
    );

    // A lock file that is a link to a file not yet there is not created.
    let elsewhere = outside.path().join("created");
    let lock = backups.path().join(".backup.lock");
    std::fs::remove_file(&lock).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &lock).unwrap();
    yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap_err();
    assert!(!elsewhere.exists(), "the lock file's link was followed");
}

/// `SEC-54`: a directory link under `packages/`, or in its place, would put
/// the files of the package store (whose names and bytes a push key picks)
/// wherever it points. The backup refuses it and writes nothing there.
#[cfg(unix)]
#[tokio::test]
async fn a_backup_does_not_write_through_directory_links_in_its_destination() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    push(&base, "Alpha", "1.0.0").await;
    let backups = tempfile::tempdir().unwrap();
    yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    push(&base, "Alpha", "2.0.0").await;

    let outside = tempfile::tempdir().unwrap();
    let alpha = backups.path().join("packages/alpha");
    std::fs::remove_dir_all(&alpha).unwrap();
    std::os::unix::fs::symlink(outside.path(), &alpha).unwrap();
    let err = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("symbolic link"), "{err:#}");
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);

    // The same for `packages/` itself.
    let packages = backups.path().join("packages");
    std::fs::remove_dir_all(&packages).unwrap();
    std::os::unix::fs::symlink(outside.path(), &packages).unwrap();
    let err = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("symbolic link"), "{err:#}");
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

/// `TEST-26`: the check builds the paths of attached files and symbols on its
/// own, apart from the store. A backup of both restores into a server that
/// serves them, and one that is gone from the live store is reported.
#[tokio::test]
async fn attached_files_and_symbols_are_backed_up_checked_and_restored() {
    let live = tempfile::tempdir().unwrap();
    let base = serve(config_for(live.path())).await;
    let pdb = common::build_portable_pdb(&[7; 16]);
    let key = yanuget::pdb::portable_pdb_signature(&pdb).unwrap();
    put(
        &base,
        "/api/v2/package",
        common::build_nupkg_for_pdb("Sym.Lib", "1.0.0", "sym.lib.pdb", &pdb),
    )
    .await;
    put(
        &base,
        "/api/v2/symbol",
        common::build_snupkg("Sym.Lib", "1.0.0", "sym.lib.pdb", &pdb),
    )
    .await;
    push(&base, "Img.Pkg", "1.0.0").await;
    let image = common::image_bytes(1);
    put(&base, "/api/v2/files/Img.Pkg/1.0.0/base.wim", image.clone()).await;
    let (symbol_path, file_path) = (
        format!("/download/symbols/sym.lib.pdb/{key}/sym.lib.pdb"),
        "/files/img.pkg/1.0.0/base.wim",
    );
    assert_eq!(get(&base, &symbol_path).await.as_deref(), Some(&pdb[..]));

    let backups = tempfile::tempdir().unwrap();
    let summary = yanuget::backup::backup(&config_for(live.path()), backups.path(), hidden())
        .await
        .unwrap();
    assert!(
        summary.manifest.missing.is_empty(),
        "{:?}",
        summary.manifest.missing
    );

    let restored = tempfile::tempdir().unwrap();
    yanuget::backup::restore(
        &config_for(restored.path()),
        backups.path(),
        false,
        hidden(),
    )
    .await
    .unwrap();
    let base2 = serve(config_for(restored.path())).await;
    assert_eq!(get(&base2, file_path).await, Some(image.clone()));
    assert_eq!(get(&base2, &symbol_path).await, Some(pdb));

    // Taken from the live store, each is reported by a backup into a new
    // directory, under the path the store keeps it at.
    let sha = common::sha256_hex(&image);
    let blob = format!(".blobs/sha256/{}/{sha}", &sha[..2]);
    let symbol = format!(".symbols/{}/sym.lib.pdb", key.to_ascii_lowercase());
    for rel in [&blob, &symbol] {
        let path = live.path().join("packages").join(rel);
        assert!(path.is_file(), "{rel} is not where the store keeps it");
        std::fs::remove_file(path).unwrap();
    }
    let fresh = tempfile::tempdir().unwrap();
    let summary = yanuget::backup::backup(&config_for(live.path()), fresh.path(), hidden())
        .await
        .unwrap();
    let mut missing = summary.manifest.missing.clone();
    missing.sort();
    let mut want = vec![blob, symbol];
    want.sort();
    assert_eq!(missing, want);
}
