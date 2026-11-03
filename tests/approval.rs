//! Approving from the admin page end to end: the build and the files a
//! reviewer was shown, and the pending-versions report.

mod common;

use std::io::{Cursor, Write};

use yanuget::database::SqliteDatabase;
use yanuget::storage::FilesystemStorage;
use zip::write::SimpleFileOptions;

use common::*;

/// Under `pending-only` a push can replace a pending build while a reviewer
/// reads the admin page: approving from that page must not approve the build
/// it never showed.
#[tokio::test]
async fn approving_from_the_admin_page_approves_the_build_it_showed() {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut gated = feed("gated");
        gated.requires_approval = true;
        gated.allow_overwrite = Some(yanuget::config::OverwriteMode::PendingOnly);
        c.feeds = vec![gated];
    })
    .await;
    let push = |content: &'static [u8]| {
        push_to(
            &server,
            "/gated/api/v2/package",
            API_KEY,
            build_nupkg("Ring.Pkg", "1.0.0", content),
        )
    };
    assert_eq!(push(b"first").await.status(), reqwest::StatusCode::CREATED);

    let shown_build = || async {
        let page = server
            .client
            .get(server.url("/gated/admin/packages/ring.pkg"))
            .basic_auth("admin", Some(ADMIN_KEY))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let marker = "name=\"build\" value=\"";
        let start = page.find(marker).expect("the page names the build") + marker.len();
        let shown = page[start..].split('"').next().unwrap().to_string();
        assert!(shown.starts_with("1.0.0:"), "{shown}");
        format!(
            "build={}",
            shown
                .replace('+', "%2B")
                .replace('/', "%2F")
                .replace('=', "%3D")
                .replace(':', "%3A")
        )
    };
    let shown_field = shown_build().await;

    // A new build arrives while the page is open.
    assert_eq!(push(b"second").await.status(), reqwest::StatusCode::CREATED);
    let approve = |body: String| {
        no_redirect()
            .post(server.url("/gated/admin/packages/ring.pkg/1.0.0/approve"))
            .basic_auth("admin", Some(ADMIN_KEY))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
    };
    let refused = approve(format!("{}&{shown_field}", admin_body(ADMIN_KEY)))
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);
    let bulk = admin_bulk(
        &server,
        "/gated",
        "ring.pkg",
        ADMIN_KEY,
        &format!("op=approve&v=1.0.0&{shown_field}"),
    )
    .await;
    assert_eq!(bulk.status(), reqwest::StatusCode::CONFLICT);
    let index = server.url("/gated/v3/package/ring.pkg/index.json");
    assert_eq!(
        server.client.get(&index).send().await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND,
        "still pending"
    );

    // The page as it is now shows the new build, and approves it.
    let approved = approve(format!("{}&{}", admin_body(ADMIN_KEY), shown_build().await))
        .await
        .unwrap();
    assert_eq!(approved.status(), reqwest::StatusCode::SEE_OTHER);
    assert!(server
        .client
        .get(&index)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

/// A gated `pending-only` feed `gated` holding `Ring.Pkg` 1.0.0, pending.
async fn spawn_gated_pending_only() -> TestServer {
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        c.admin_api_key = Some(ADMIN_KEY.into());
        let mut gated = feed("gated");
        gated.requires_approval = true;
        gated.allow_overwrite = Some(yanuget::config::OverwriteMode::PendingOnly);
        c.feeds = vec![gated];
    })
    .await;
    let pushed = push_to(
        &server,
        "/gated/api/v2/package",
        API_KEY,
        build_nupkg("Ring.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
    server
}

/// Attach `body` as `name` to `Ring.Pkg` 1.0.0 in `gated`, with the push key
/// and, if `admin`, the admin key too.
async fn put_gated_file(
    server: &TestServer,
    name: &str,
    body: Vec<u8>,
    admin: bool,
) -> reqwest::StatusCode {
    let mut req = server
        .client
        .put(server.url(&format!("/gated/api/v2/files/Ring.Pkg/1.0.0/{name}")))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(body);
    if admin {
        req = req.basic_auth("admin", Some(ADMIN_KEY));
    }
    req.send().await.unwrap().status()
}

/// The `build` and `files` fields the admin page of `Ring.Pkg` shows for its
/// pending version, form-encoded.
async fn shown_review_fields(server: &TestServer) -> String {
    shown_review_fields_in(server, "/gated", ADMIN_KEY).await
}

/// [`shown_review_fields`] on the admin page of the feed at `prefix`.
async fn shown_review_fields_in(server: &TestServer, prefix: &str, admin_key: &str) -> String {
    let page = server
        .client
        .get(server.url(&format!("{prefix}/admin/packages/ring.pkg")))
        .basic_auth("admin", Some(admin_key))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let field = |name: &str| {
        let marker = format!("name=\"{name}\" value=\"");
        let start = page.find(&marker).expect("the page shows the field") + marker.len();
        let value = page[start..].split('"').next().unwrap();
        assert!(value.starts_with("1.0.0:"), "{value}");
        format!(
            "{name}={}",
            value
                .replace('+', "%2B")
                .replace('/', "%2F")
                .replace('=', "%3D")
                .replace(':', "%3A")
        )
    };
    format!("{}&{}", field("build"), field("files"))
}

/// Under `pending-only` a version approved anywhere stays as it is, and the
/// package hash does not cover its files: the push key can no longer change
/// them; the admin key still can.
#[tokio::test]
async fn the_files_of_an_approved_version_change_only_with_the_admin_key() {
    let server = spawn_gated_pending_only().await;
    // While pending, the push key attaches and detaches as it likes.
    assert_eq!(
        put_gated_file(&server, "setup.zip", image_bytes(1), false).await,
        reqwest::StatusCode::CREATED
    );
    let approved = no_redirect()
        .post(server.url("/gated/admin/packages/ring.pkg/1.0.0/approve"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(ADMIN_KEY))
        .send()
        .await
        .unwrap();
    assert_eq!(approved.status(), reqwest::StatusCode::SEE_OTHER);

    // Approved: no new file, no detach, no resumable upload with the push key.
    assert_eq!(
        put_gated_file(&server, "other.zip", image_bytes(2), false).await,
        reqwest::StatusCode::FORBIDDEN
    );
    let file = server.url("/gated/api/v2/files/Ring.Pkg/1.0.0/setup.zip");
    let detach = |admin: bool| {
        let mut req = server
            .client
            .delete(&file)
            .header("X-NuGet-ApiKey", API_KEY);
        if admin {
            req = req.basic_auth("admin", Some(ADMIN_KEY));
        }
        req.send()
    };
    assert_eq!(
        detach(false).await.unwrap().status(),
        reqwest::StatusCode::FORBIDDEN
    );
    let created = tus(no_redirect().post(server.url("/gated/api/v2/uploads")))
        .header("Upload-Length", "10")
        .header(
            "Upload-Metadata",
            tus_metadata(&[
                ("id", "Ring.Pkg"),
                ("version", "1.0.0"),
                ("filename", "other.zip"),
            ]),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), reqwest::StatusCode::FORBIDDEN);
    let served = server.url("/gated/files/ring.pkg/1.0.0/setup.zip");
    assert_eq!(
        server.client.get(&served).send().await.unwrap().status(),
        reqwest::StatusCode::OK,
        "the file is still there"
    );

    // With the admin key as well, both go through.
    assert_eq!(
        detach(true).await.unwrap().status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert_eq!(
        put_gated_file(&server, "setup.zip", image_bytes(3), true).await,
        reqwest::StatusCode::CREATED
    );
}

/// Files hang off the version, not the feed: once `prod` approved a version
/// promoted from `staging`, neither staging's push key nor its admin key may
/// change its files, whatever staging's `allow_overwrite`; prod's admin key
/// may (`SEC-33`).
#[tokio::test]
async fn the_files_of_a_version_approved_elsewhere_change_only_with_that_feeds_admin_key() {
    const STAGING_ADMIN: &str = "staging-admin";
    const PROD_ADMIN: &str = "prod-admin";
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut staging = feed("staging");
        staging.admin_api_key = Some(STAGING_ADMIN.into());
        staging.promotes_to = Some("prod".into());
        let mut prod = feed("prod");
        prod.admin_api_key = Some(PROD_ADMIN.into());
        prod.requires_approval = true;
        c.feeds = vec![staging, prod];
    })
    .await;
    let pushed = push_to(
        &server,
        "/staging/api/v2/package",
        API_KEY,
        build_nupkg("Ring.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
    let file = |name: &str| server.url(&format!("/staging/api/v2/files/Ring.Pkg/1.0.0/{name}"));
    let put = |name: &str, body: Vec<u8>, admin: Option<&'static str>| {
        let mut req = server
            .client
            .put(file(name))
            .header("X-NuGet-ApiKey", API_KEY)
            .body(body);
        if let Some(key) = admin {
            req = req.basic_auth("admin", Some(key));
        }
        req.send()
    };
    let delete = |name: &str, admin: Option<&'static str>| {
        let mut req = server
            .client
            .delete(file(name))
            .header("X-NuGet-ApiKey", API_KEY);
        if let Some(key) = admin {
            req = req.basic_auth("admin", Some(key));
        }
        req.send()
    };
    // Approved on arrival in staging, which reviews nothing: the push key
    // attaches as it likes.
    assert_eq!(
        put("setup.zip", image_bytes(1), None)
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::CREATED
    );
    let promoted = no_redirect()
        .post(server.url("/staging/admin/packages/ring.pkg/1.0.0/promote"))
        .basic_auth("admin", Some(STAGING_ADMIN))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(STAGING_ADMIN))
        .send()
        .await
        .unwrap();
    assert_eq!(promoted.status(), reqwest::StatusCode::SEE_OTHER);
    // Pending in prod: nothing reviewed yet, so still changeable.
    assert_eq!(
        put("notes.zip", image_bytes(2), None)
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::CREATED
    );
    let approve_fields = shown_review_fields_in(&server, "/prod", PROD_ADMIN).await;
    let approved = no_redirect()
        .post(server.url("/prod/admin/packages/ring.pkg/1.0.0/approve"))
        .basic_auth("admin", Some(PROD_ADMIN))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("{}&{approve_fields}", admin_body(PROD_ADMIN)))
        .send()
        .await
        .unwrap();
    assert_eq!(approved.status(), reqwest::StatusCode::SEE_OTHER);

    // Approved in prod: staging's push key, alone or with staging's admin
    // key, changes nothing, and neither does staging's admin page.
    for admin in [None, Some(STAGING_ADMIN)] {
        let refused = delete("setup.zip", admin).await.unwrap();
        assert_eq!(refused.status(), reqwest::StatusCode::FORBIDDEN);
        assert!(refused.text().await.unwrap().contains("prod"));
        assert_eq!(
            put("other.zip", image_bytes(3), admin)
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::FORBIDDEN
        );
    }
    let from_page = no_redirect()
        .post(server.url("/staging/admin/packages/ring.pkg/1.0.0/files/setup.zip/delete"))
        .basic_auth("admin", Some(STAGING_ADMIN))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body(STAGING_ADMIN))
        .send()
        .await
        .unwrap();
    assert_eq!(from_page.status(), reqwest::StatusCode::FORBIDDEN);
    let served = server.url("/prod/files/ring.pkg/1.0.0/setup.zip");
    assert_eq!(
        server.client.get(&served).send().await.unwrap().status(),
        reqwest::StatusCode::OK,
        "the file is still there"
    );

    // Prod's admin key sets prod's approval aside.
    assert_eq!(
        delete("setup.zip", Some(PROD_ADMIN))
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::NO_CONTENT
    );
}

/// The inbox holds no credentials, so it is held like a push key: a file
/// dropped for `staging` is not attached to a version `prod`, which reviews
/// what it serves, has approved (`SEC-33`).
#[tokio::test]
async fn the_inbox_leaves_the_files_of_a_version_approved_elsewhere_alone() {
    const PROD_ADMIN: &str = "prod-admin";
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut staging = feed("staging");
        staging.admin_api_key = Some("staging-admin".into());
        staging.promotes_to = Some("prod".into());
        let mut prod = feed("prod");
        prod.admin_api_key = Some(PROD_ADMIN.into());
        prod.requires_approval = true;
        c.feeds = vec![staging, prod];
    })
    .await;
    let pushed = push_to(
        &server,
        "/staging/api/v2/package",
        API_KEY,
        build_nupkg("Ring.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
    let promoted = no_redirect()
        .post(server.url("/staging/admin/packages/ring.pkg/1.0.0/promote"))
        .basic_auth("admin", Some("staging-admin"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(admin_body("staging-admin"))
        .send()
        .await
        .unwrap();
    assert_eq!(promoted.status(), reqwest::StatusCode::SEE_OTHER);
    let approve_fields = shown_review_fields_in(&server, "/prod", PROD_ADMIN).await;
    let approved = no_redirect()
        .post(server.url("/prod/admin/packages/ring.pkg/1.0.0/approve"))
        .basic_auth("admin", Some(PROD_ADMIN))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!("{}&{approve_fields}", admin_body(PROD_ADMIN)))
        .send()
        .await
        .unwrap();
    assert_eq!(approved.status(), reqwest::StatusCode::SEE_OTHER);

    let data = server._dir.path();
    let inbox = tempfile::tempdir().unwrap();
    let dir = inbox.path().join("staging/Ring.Pkg/1.0.0");
    std::fs::create_dir_all(&dir).unwrap();
    let bytes = image_bytes(3);
    std::fs::write(dir.join("setup.zip"), &bytes).unwrap();
    std::fs::write(
        dir.join("setup.zip.sha256"),
        format!("{}  setup.zip\n", sha256_hex(&bytes)),
    )
    .unwrap();
    let storage = FilesystemStorage::new(data.join("packages")).await.unwrap();
    let db = SqliteDatabase::connect(&data.join("yanuget.db").to_string_lossy())
        .await
        .unwrap();
    let files = yanuget::config::FilesConfig::default();
    let feeds = vec!["staging".to_string(), "prod".to_string()];
    let reviewing = vec!["prod".to_string()];
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
        reviewing: &reviewing,
        space: &space,
    };
    assert_eq!(
        scan.scan().await,
        yanuget::inbox::ScanReport {
            imported: 0,
            failed: 1
        }
    );
    let why = std::fs::read_to_string(dir.join("setup.zip.error")).unwrap();
    assert!(why.contains("prod"), "{why}");
    let got = server
        .client
        .get(server.url("/staging/files/ring.pkg/1.0.0/setup.zip"))
        .send()
        .await
        .unwrap();
    assert_eq!(got.status(), reqwest::StatusCode::NOT_FOUND);
}

/// Approving from the admin page approves the files it showed, too: under
/// `pending-only` the push key can change them while a reviewer reads it.
#[tokio::test]
async fn approving_from_the_admin_page_approves_the_files_it_showed() {
    let server = spawn_gated_pending_only().await;
    assert_eq!(
        put_gated_file(&server, "setup.zip", image_bytes(1), false).await,
        reqwest::StatusCode::CREATED
    );
    let shown = shown_review_fields(&server).await;

    // Another file arrives while the page is open; the build is the same.
    assert_eq!(
        put_gated_file(&server, "extra.zip", image_bytes(2), false).await,
        reqwest::StatusCode::CREATED
    );
    let approve = |fields: String| {
        no_redirect()
            .post(server.url("/gated/admin/packages/ring.pkg/1.0.0/approve"))
            .basic_auth("admin", Some(ADMIN_KEY))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("{}&{fields}", admin_body(ADMIN_KEY)))
            .send()
    };
    assert_eq!(
        approve(shown.clone()).await.unwrap().status(),
        reqwest::StatusCode::CONFLICT
    );
    let bulk = admin_bulk(
        &server,
        "/gated",
        "ring.pkg",
        ADMIN_KEY,
        &format!("op=approve&v=1.0.0&{shown}"),
    )
    .await;
    assert_eq!(bulk.status(), reqwest::StatusCode::CONFLICT);
    let index = server.url("/gated/v3/package/ring.pkg/index.json");
    assert_eq!(
        server.client.get(&index).send().await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND,
        "still pending"
    );

    // The page as it is now shows both files, and approves them.
    let approved = approve(shown_review_fields(&server).await).await.unwrap();
    assert_eq!(approved.status(), reqwest::StatusCode::SEE_OTHER);
    assert!(server
        .client
        .get(&index)
        .send()
        .await
        .unwrap()
        .status()
        .is_success());
}

/// The files a reviewer saw are what an approval approves, whichever way
/// they changed since: added to none, replaced under the same name with
/// other bytes, or detached (`TEST-10`).
#[tokio::test]
async fn approving_refuses_files_replaced_detached_or_added_to_none() {
    let server = spawn_gated_pending_only().await;
    let approve = |fields: String| {
        no_redirect()
            .post(server.url("/gated/admin/packages/ring.pkg/1.0.0/approve"))
            .basic_auth("admin", Some(ADMIN_KEY))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(format!("{}&{fields}", admin_body(ADMIN_KEY)))
            .send()
    };
    let detach = |name: &'static str| {
        server
            .client
            .delete(server.url(&format!("/gated/api/v2/files/Ring.Pkg/1.0.0/{name}")))
            .header("X-NuGet-ApiKey", API_KEY)
            .send()
    };
    let conflict = reqwest::StatusCode::CONFLICT;

    // The page showed no files; one arrives.
    let none = shown_review_fields(&server).await;
    assert_eq!(
        put_gated_file(&server, "setup.zip", image_bytes(1), false).await,
        reqwest::StatusCode::CREATED
    );
    assert_eq!(approve(none).await.unwrap().status(), conflict);

    // The page showed setup.zip; it is replaced with other bytes.
    let one = shown_review_fields(&server).await;
    assert_eq!(
        detach("setup.zip").await.unwrap().status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert_eq!(
        put_gated_file(&server, "setup.zip", image_bytes(2), false).await,
        reqwest::StatusCode::CREATED
    );
    assert_eq!(approve(one).await.unwrap().status(), conflict);

    // The page showed setup.zip; it is detached.
    let replaced = shown_review_fields(&server).await;
    assert_eq!(
        detach("setup.zip").await.unwrap().status(),
        reqwest::StatusCode::NO_CONTENT
    );
    assert_eq!(approve(replaced).await.unwrap().status(), conflict);

    // What the page shows now is approved.
    let now = shown_review_fields(&server).await;
    assert_eq!(
        approve(now).await.unwrap().status(),
        reqwest::StatusCode::SEE_OTHER
    );
}

/// A package carrying extra entries next to its manifest, such as an
/// internalizer report and its prelude.
fn build_nupkg_with_entries(id: &str, version: &str, entries: &[(&str, &[u8])]) -> Vec<u8> {
    let base = build_nupkg(id, version, b"x");
    let mut archive = zip::ZipArchive::new(Cursor::new(base)).unwrap();
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        for i in 0..archive.len() {
            zip.raw_copy_file(archive.by_index(i).unwrap()).unwrap();
        }
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, bytes) in entries {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

/// `INT-30`: a pending version's internalizer report is on its admin page,
/// escaped, and a file it lists but the version lacks holds up the approval.
/// Characters that hide or reorder text are written out (`SEC-40`).
#[tokio::test]
async fn the_admin_page_shows_a_pending_versions_report() {
    let server = spawn_intake().await;
    let setup = image_bytes(7);
    let report = format!(
        r#"{{"schema": 1, "package": {{"id": "Report.Pkg", "version": "1.0"}},
            "status": "internalized",
            "upstream": {{"source": "https://community.chocolatey.org/api/v2/"}},
            "environment": {{"chocolatey_version": "2.4.3"}},
            "downloads": [
              {{"url": "https://vendor.example/setup.exe?a=1&b=<2>", "helper": "Install-ChocolateyPackage",
                "file": "setup.exe", "size": {size}, "sha256": "{sha}",
                "checksum": {{"status": "verified", "algorithm": "sha256"}}}},
              {{"url": "https://vendor.example/extra.msi", "file": "extra.msi",
                "checksum": {{"status": "trust-on-first-use"}}}}
            ],
            "warnings": ["<script>alert(1)</script>", "evil\u202eexe.txt", "zero\u200bwidth"],
            "overrides": ["download <b>setup.exe</b> replaced"],
            "prelude": "tools/prelude.ps1",
            "a_later_field": true}}"#,
        size = setup.len(),
        sha = sha256_hex(&setup),
    );
    let nupkg = build_nupkg_with_entries(
        "Report.Pkg",
        "1.0.0",
        &[
            ("internalizer/report.json", report.as_bytes()),
            ("tools/prelude.ps1", b"$base = 'https://feed' # <b>"),
        ],
    );
    let pushed = push_to(&server, "/intake/api/v2/package", API_KEY, nupkg).await;
    assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
    // A version without a report renders as before.
    let plain = push_to(
        &server,
        "/intake/api/v2/package",
        API_KEY,
        build_nupkg("Report.Pkg", "0.9.0", b"y"),
    )
    .await;
    assert_eq!(plain.status(), reqwest::StatusCode::CREATED);
    let put = put_feed_file(
        &server,
        "/intake",
        "Report.Pkg",
        "1.0.0",
        "setup.exe",
        setup,
    )
    .await;
    assert_eq!(put.status(), reqwest::StatusCode::CREATED);

    let page = || async {
        let resp = server
            .client
            .get(server.url("/intake/admin/packages/report.pkg"))
            .basic_auth("admin", Some(ADMIN_KEY))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        resp.text().await.unwrap()
    };
    let html = page().await;
    assert!(html.contains("Internalizer report: 1.0.0"), "{html}");
    assert!(!html.contains("Internalizer report: 0.9.0"));
    assert!(html.contains("setup.exe?a=1&amp;b=&lt;2&gt;"));
    assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
    assert!(!html.contains("<script>alert(1)"));
    assert!(html.contains("<li>evil[U+202E]exe.txt</li>"), "{html}");
    assert!(html.contains("<li>zero[U+200B]width</li>"));
    assert!(!html.contains(['\u{202E}', '\u{200B}']));
    assert!(html
        .contains("<h3>Overrides</h3><ul><li>download &lt;b&gt;setup.exe&lt;/b&gt; replaced</li>"));
    assert!(html.contains("tools/prelude.ps1"));
    assert!(html.contains("https://feed") && html.contains("# &lt;b&gt;"));
    assert!(html.contains("2.4.3"));
    assert!(html.contains("trust on first use"));
    assert!(html.contains("extra.msi is not attached"), "{html}");
    assert!(html.contains("Approve it anyway?"));

    // Attached, nothing stands in the way any more.
    let put = put_feed_file(
        &server,
        "/intake",
        "Report.Pkg",
        "1.0.0",
        "extra.msi",
        image_bytes(8),
    )
    .await;
    assert_eq!(put.status(), reqwest::StatusCode::CREATED);
    let html = page().await;
    assert!(!html.contains("is not attached"), "{html}");
    assert!(!html.contains("Approve it anyway?"));
    assert!(html.contains("Internalized; the report is below."));
}

/// `INT-30`: a report the server cannot read says so instead of hiding.
#[tokio::test]
async fn an_unreadable_report_is_shown_as_such() {
    let server = spawn_intake().await;
    let nupkg = build_nupkg_with_entries(
        "Broken.Report",
        "1.0.0",
        &[(
            "internalizer/report.json",
            br#"{"schema": 2, "anything": "new"}"#,
        )],
    );
    let pushed = push_to(&server, "/intake/api/v2/package", API_KEY, nupkg).await;
    assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
    let html = server
        .client
        .get(server.url("/intake/admin/packages/broken.report"))
        .basic_auth("admin", Some(ADMIN_KEY))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(html.contains("schema version 2"), "{html}");
    assert!(html.contains("Approve it anyway?"));
}
