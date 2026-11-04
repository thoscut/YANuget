//! Files attached to versions end to end: direct and resumable uploads,
//! the inbox, and per-feed file types.

mod common;

use yanuget::database::SqliteDatabase;
use yanuget::storage::FilesystemStorage;

use common::*;

// ---------------------------------------------------------------------------
// Files attached to versions
// ---------------------------------------------------------------------------

async fn put_file(
    server: &TestServer,
    id: &str,
    v: &str,
    name: &str,
    body: Vec<u8>,
    sha: Option<&str>,
) -> reqwest::Response {
    let mut req = server
        .client
        .put(server.url(&format!("/api/v2/files/{id}/{v}/{name}")))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(body);
    if let Some(sha) = sha {
        req = req.header("x-checksum-sha256", sha);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn a_file_attached_in_one_request_downloads_resumably() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Img.Pkg", "1.0.0", b"x")).await;
    let image = image_bytes(1);
    let sha = sha256_hex(&image);

    let created = put_file(
        &server,
        "Img.Pkg",
        "1.0.0",
        "base.wim",
        image.clone(),
        Some(&sha),
    )
    .await;
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);
    let body: serde_json::Value = created.json().await.unwrap();
    assert_eq!(body["sha256"], sha.as_str());
    assert_eq!(body["size"], image.len());
    // The same file again changes nothing; different bytes under the name are refused.
    let same = put_file(&server, "Img.Pkg", "1.0.0", "base.wim", image.clone(), None).await;
    assert_eq!(same.status(), reqwest::StatusCode::OK);
    let other = put_file(
        &server,
        "Img.Pkg",
        "1.0.0",
        "base.wim",
        image_bytes(2),
        None,
    )
    .await;
    assert_eq!(other.status(), reqwest::StatusCode::CONFLICT);
    // A body that is not what the client said is refused, and nothing stays.
    let corrupt = put_file(
        &server,
        "Img.Pkg",
        "1.0.0",
        "other.wim",
        image_bytes(3),
        Some(&sha),
    )
    .await;
    assert_eq!(corrupt.status(), reqwest::StatusCode::BAD_REQUEST);
    // So are names that could be anything but an image.
    for name in ["page.html", "image.svg", "..%2Fx.wim"] {
        let r = put_file(&server, "Img.Pkg", "1.0.0", name, b"x".to_vec(), None).await;
        assert!(r.status().is_client_error(), "{name}: {}", r.status());
    }
    // Only to a version the feed holds.
    let nowhere = put_file(&server, "Img.Pkg", "9.9.9", "base.wim", b"x".to_vec(), None).await;
    assert_eq!(nowhere.status(), reqwest::StatusCode::NOT_FOUND);

    let url = server.url("/files/img.pkg/1.0.0/base.wim");
    let head = server.client.head(&url).send().await.unwrap();
    assert_eq!(head.status(), reqwest::StatusCode::OK);
    assert_eq!(
        head.headers()["content-length"],
        image.len().to_string().as_str()
    );
    assert_eq!(head.headers()["etag"], format!("\"{sha}\"").as_str());
    assert!(head.headers()["repr-digest"]
        .to_str()
        .unwrap()
        .starts_with("sha-256=:"));
    let full = server.client.get(&url).send().await.unwrap();
    assert_eq!(full.headers()["content-type"], "application/octet-stream");
    assert!(full.headers()["content-disposition"]
        .to_str()
        .unwrap()
        .starts_with("attachment; filename=\"base.wim\""));
    assert_eq!(
        full.headers()["content-security-policy"],
        "default-src 'none'"
    );
    assert_eq!(full.bytes().await.unwrap().as_ref(), image.as_slice());
    let rest = server
        .client
        .get(&url)
        .header("range", "bytes=100000-")
        .header("if-range", format!("\"{sha}\""))
        .send()
        .await
        .unwrap();
    assert_eq!(rest.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(rest.bytes().await.unwrap().as_ref(), &image[100_000..]);

    let index: serde_json::Value = server
        .client
        .get(server.url("/files/img.pkg/1.0.0/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(index["files"].as_array().unwrap().len(), 1);
    assert_eq!(index["files"][0]["url"], url.as_str());

    // The package page shows it, with the script that fetches and checks it.
    let page = server
        .client
        .get(server.url("/packages/img.pkg"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(page.contains("<h2>Files</h2>"), "{page}");
    assert!(
        page.contains(&format!("Start-BitsTransfer -Source &#39;{url}&#39;")),
        "{page}"
    );
    assert!(
        page.contains(&format!("-Checksum &#39;{sha}&#39; -ChecksumType sha256")),
        "{page}"
    );

    // Deleting it frees the name.
    let gone = server
        .client
        .delete(server.url("/api/v2/files/img.pkg/1.0.0/base.wim"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        status_of(&server, "/files/img.pkg/1.0.0/base.wim").await,
        reqwest::StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_feed_open_for_pushes_is_not_open_for_files() {
    let server = spawn_with(|c| c.api_key = None).await;
    push_multipart(&server, "anything", build_nupkg("Open.Pkg", "1.0.0", b"x")).await;
    let r = put_file(
        &server,
        "Open.Pkg",
        "1.0.0",
        "base.wim",
        b"x".to_vec(),
        None,
    )
    .await;
    assert_eq!(r.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_blob_shared_by_two_versions_outlives_the_first() {
    let server = spawn_with(|c| c.admin_api_key = Some(ADMIN_KEY.to_string())).await;
    let image = image_bytes(9);
    for v in ["1.0.0", "2.0.0"] {
        push_multipart(&server, API_KEY, build_nupkg("Shared.Img", v, b"x")).await;
        let r = put_file(&server, "Shared.Img", v, "base.wim", image.clone(), None).await;
        assert_eq!(r.status(), reqwest::StatusCode::CREATED);
    }
    let blob = server
        ._dir
        .path()
        .join("packages/.blobs/sha256")
        .join(&sha256_hex(&image)[..2])
        .join(sha256_hex(&image));
    assert!(blob.exists(), "one blob for both: {}", blob.display());

    let delete = |v: &'static str| {
        admin_bulk(
            &server,
            "",
            "shared.img",
            ADMIN_KEY,
            if v == "1.0.0" {
                "op=delete&v=1.0.0"
            } else {
                "op=delete&v=2.0.0"
            },
        )
    };
    assert_eq!(
        delete("1.0.0").await.status(),
        reqwest::StatusCode::SEE_OTHER
    );
    assert!(blob.exists(), "still referenced by 2.0.0");
    assert!(status_of(&server, "/files/shared.img/2.0.0/base.wim")
        .await
        .is_success());
    assert_eq!(
        delete("2.0.0").await.status(),
        reqwest::StatusCode::SEE_OTHER
    );
    assert!(!blob.exists(), "the last reference took the blob with it");
}

#[tokio::test]
async fn a_resumable_upload_continues_where_it_stopped() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Tus.Pkg", "1.0.0", b"x")).await;
    let image = image_bytes(5);
    let sha = sha256_hex(&image);
    let no_redirect = no_redirect();

    // Without the protocol header, a client is told what is spoken here.
    let bare = no_redirect
        .post(server.url("/api/v2/uploads"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(bare.status(), reqwest::StatusCode::PRECONDITION_FAILED);
    assert_eq!(bare.headers()["tus-version"], "1.0.0");

    let created = tus(no_redirect.post(server.url("/api/v2/uploads")))
        .header("Upload-Length", image.len().to_string())
        .header(
            "Upload-Metadata",
            tus_metadata(&[
                ("id", "Tus.Pkg"),
                ("version", "1.0.0"),
                ("filename", "base.wim"),
                ("sha256", &sha),
            ]),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), reqwest::StatusCode::CREATED);
    let location = created.headers()["location"].to_str().unwrap().to_string();
    assert!(created.headers().contains_key("upload-expires"));

    let patch = |offset: usize, bytes: &[u8]| {
        tus(server.client.patch(&location))
            .header("Content-Type", "application/offset+octet-stream")
            .header("Upload-Offset", offset.to_string())
            .body(bytes.to_vec())
            .send()
    };
    let half = image.len() / 2;
    let first = patch(0, &image[..half]).await.unwrap();
    assert_eq!(first.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(first.headers()["upload-offset"], half.to_string().as_str());

    // The connection dropped here; the client asks where to go on from.
    let head = tus(server.client.head(&location)).send().await.unwrap();
    assert_eq!(head.headers()["upload-offset"], half.to_string().as_str());
    assert_eq!(
        head.headers()["upload-length"],
        image.len().to_string().as_str()
    );
    // Sending from the wrong place is refused, with the right place.
    let wrong = patch(0, &image[..10]).await.unwrap();
    assert_eq!(wrong.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(wrong.headers()["upload-offset"], half.to_string().as_str());
    // Nothing is attached until the last byte.
    assert_eq!(
        status_of(&server, "/files/tus.pkg/1.0.0/base.wim").await,
        reqwest::StatusCode::NOT_FOUND
    );

    let last = patch(half, &image[half..]).await.unwrap();
    assert_eq!(last.status(), reqwest::StatusCode::NO_CONTENT);
    let got = server
        .client
        .get(server.url("/files/tus.pkg/1.0.0/base.wim"))
        .send()
        .await
        .unwrap();
    assert_eq!(got.bytes().await.unwrap().as_ref(), image.as_slice());
    // The finished upload is gone.
    let after = tus(server.client.head(&location)).send().await.unwrap();
    assert_eq!(after.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_resumable_upload_that_does_not_match_its_checksum_is_discarded() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Tus.Bad", "1.0.0", b"x")).await;
    let image = image_bytes(6);
    let created = tus(no_redirect().post(server.url("/api/v2/uploads")))
        .header("Upload-Length", image.len().to_string())
        .header(
            "Upload-Metadata",
            tus_metadata(&[
                ("id", "Tus.Bad"),
                ("version", "1.0.0"),
                ("filename", "base.wim"),
                ("sha256", &"0".repeat(64)),
            ]),
        )
        .send()
        .await
        .unwrap();
    let location = created.headers()["location"].to_str().unwrap().to_string();
    let done = tus(server.client.patch(&location))
        .header("Content-Type", "application/offset+octet-stream")
        .header("Upload-Offset", "0")
        .body(image)
        .send()
        .await
        .unwrap();
    assert_eq!(done.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(
        status_of(&server, "/files/tus.bad/1.0.0/base.wim").await,
        reqwest::StatusCode::NOT_FOUND
    );

    // An abandoned upload can be ended by the client.
    let created = tus(no_redirect().post(server.url("/api/v2/uploads")))
        .header("Upload-Length", "10")
        .header(
            "Upload-Metadata",
            tus_metadata(&[
                ("id", "Tus.Bad"),
                ("version", "1.0.0"),
                ("filename", "later.wim"),
            ]),
        )
        .send()
        .await
        .unwrap();
    let location = created.headers()["location"].to_str().unwrap().to_string();
    let ended = tus(server.client.delete(&location)).send().await.unwrap();
    assert_eq!(ended.status(), reqwest::StatusCode::NO_CONTENT);
    let head = tus(server.client.head(&location)).send().await.unwrap();
    assert_eq!(head.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn files_dropped_in_the_inbox_are_verified_and_attached() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Drop.Pkg", "1.0.0", b"x")).await;
    let data = server._dir.path();
    let inbox = tempfile::tempdir().unwrap();
    let dir = inbox.path().join("default/Drop.Pkg/1.0.0");
    std::fs::create_dir_all(&dir).unwrap();
    let good = image_bytes(7);
    std::fs::write(dir.join("base.wim"), &good).unwrap();
    std::fs::write(
        dir.join("base.wim.sha256"),
        format!("{}  base.wim\n", sha256_hex(&good)),
    )
    .unwrap();
    // A second file whose checksum does not match…
    std::fs::write(dir.join("bad.wim"), b"not what was promised").unwrap();
    std::fs::write(dir.join("bad.wim.sha256"), "0".repeat(64)).unwrap();
    // …and a third still being copied: no checksum file yet.
    std::fs::write(dir.join("partial.wim"), b"half").unwrap();

    let storage = FilesystemStorage::new(data.join("packages")).await.unwrap();
    let db = SqliteDatabase::connect(&data.join("yanuget.db").to_string_lossy())
        .await
        .unwrap();
    let files = yanuget::config::FilesConfig::default();
    let feeds = vec!["default".to_string()];
    let space = yanuget::space::SpaceGuard::new(data.join("packages/.uploads"), 0);
    let scan = yanuget::inbox::Inbox {
        dir: inbox.path(),
        storage: &storage,
        db: &db,
        files: &files,
        feed_extensions: &Default::default(),
        max_file_size: None,
        feeds: &feeds,
        pending_only: &[],
        reviewing: &[],
        space: &space,
    };
    let report = scan.scan().await;
    assert_eq!(
        report,
        yanuget::inbox::ScanReport {
            imported: 1,
            failed: 1
        }
    );

    // The good one is attached and gone from the inbox.
    let got = server
        .client
        .get(server.url("/files/drop.pkg/1.0.0/base.wim"))
        .send()
        .await
        .unwrap();
    assert_eq!(got.bytes().await.unwrap().as_ref(), good.as_slice());
    assert!(!dir.join("base.wim").exists() && !dir.join("base.wim.sha256").exists());
    // The bad one is back where it was, with the reason next to it.
    assert!(dir.join("bad.wim").exists());
    let why = std::fs::read_to_string(dir.join("bad.wim.error")).unwrap();
    assert!(why.contains("checksum mismatch"), "{why}");
    // The partial one waits.
    assert!(dir.join("partial.wim").exists());
    // A second scan leaves the failed one alone until its .error is removed.
    assert_eq!(scan.scan().await, yanuget::inbox::ScanReport::default());
}

/// `INT-03`: a feed's `[feeds.files] allowed_extensions` replaces the global
/// list for that feed only.
#[tokio::test]
async fn a_feed_can_accept_file_types_the_others_refuse() {
    let server = spawn_intake().await;
    for prefix in ["/intake", "/prod"] {
        let pushed = push_to(
            &server,
            &format!("{prefix}/api/v2/package"),
            API_KEY,
            build_nupkg("Types.Pkg", "1.0.0", b"x"),
        )
        .await;
        assert_eq!(pushed.status(), reqwest::StatusCode::CREATED, "{prefix}");
    }

    let exe = |prefix: &'static str| {
        put_feed_file(
            &server,
            prefix,
            "Types.Pkg",
            "1.0.0",
            "setup.exe",
            image_bytes(1),
        )
    };
    assert_eq!(exe("/intake").await.status(), reqwest::StatusCode::CREATED);
    assert_eq!(
        put_feed_file(
            &server,
            "/intake",
            "Types.Pkg",
            "1.0.0",
            "Setup.msi",
            image_bytes(2)
        )
        .await
        .status(),
        reqwest::StatusCode::CREATED,
        "case and a leading dot in the list do not matter"
    );
    let refused = exe("/prod").await;
    assert_eq!(refused.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(refused.text().await.unwrap().contains("wim"));
    // The feed's list replaces the global one rather than adding to it.
    let wim = put_feed_file(
        &server,
        "/intake",
        "Types.Pkg",
        "1.0.0",
        "base.wim",
        image_bytes(3),
    )
    .await;
    assert_eq!(wim.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(wim.text().await.unwrap().contains("exe, .MSI"));
    assert_eq!(
        put_feed_file(
            &server,
            "/prod",
            "Types.Pkg",
            "1.0.0",
            "base.wim",
            image_bytes(3)
        )
        .await
        .status(),
        reqwest::StatusCode::CREATED
    );

    let settings = server
        .client
        .get(server.url("/intake/settings"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(settings.contains("exe, .MSI"), "{settings}");
}

/// `INT-05`: the intake flow the internalizer relies on. A file attached to a
/// pending version is withheld until the version is approved, in the intake
/// feed and again in the feed it is promoted to.
#[tokio::test]
async fn files_of_a_pending_version_are_withheld_until_approval() {
    let server = spawn_intake().await;
    let pushed = push_to(
        &server,
        "/intake/api/v2/package",
        API_KEY,
        build_nupkg("Intake.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);

    let installer = image_bytes(7);
    let attached = put_feed_file(
        &server,
        "/intake",
        "Intake.Pkg",
        "1.0.0",
        "setup.exe",
        installer.clone(),
    )
    .await;
    assert_eq!(
        attached.status(),
        reqwest::StatusCode::CREATED,
        "attach while pending"
    );

    let file = "files/intake.pkg/1.0.0/setup.exe";
    let nupkg = "v3/package/intake.pkg/1.0.0/intake.pkg.1.0.0.nupkg";
    for path in [file, nupkg] {
        assert_eq!(
            status_of(&server, &format!("/intake/{path}")).await,
            reqwest::StatusCode::NOT_FOUND,
            "{path} is withheld while pending"
        );
    }

    let approve = admin_action(&server, "/intake/admin/packages/intake.pkg/1.0.0/approve").await;
    assert_eq!(approve.status(), reqwest::StatusCode::SEE_OTHER);
    let download = server
        .client
        .get(server.url(&format!("/intake/{file}")))
        .send()
        .await
        .unwrap();
    assert!(download.status().is_success());
    assert_eq!(
        download.bytes().await.unwrap().as_ref(),
        installer.as_slice()
    );

    // Promoted into a gated feed, the version and its file are pending there,
    // even though that feed would not accept the file type itself.
    let promote = admin_action(&server, "/intake/admin/packages/intake.pkg/1.0.0/promote").await;
    assert_eq!(promote.status(), reqwest::StatusCode::SEE_OTHER);
    for path in [file, nupkg] {
        assert_eq!(
            status_of(&server, &format!("/prod/{path}")).await,
            reqwest::StatusCode::NOT_FOUND,
            "{path} is withheld in prod until approved there"
        );
    }
    let approve = admin_action(&server, "/prod/admin/packages/intake.pkg/1.0.0/approve").await;
    assert_eq!(approve.status(), reqwest::StatusCode::SEE_OTHER);
    let download = server
        .client
        .get(server.url(&format!("/prod/{file}")))
        .send()
        .await
        .unwrap();
    assert!(download.status().is_success());
    assert_eq!(
        download.bytes().await.unwrap().as_ref(),
        installer.as_slice()
    );
}
