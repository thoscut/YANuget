//! Hardening end to end: overwrites, resumable downloads, stalled uploads
//! and a full disk.

mod common;

use yanuget::database::{PackageDatabase, SqliteDatabase};

use common::*;

// ---------------------------------------------------------------------------
// Hardening: overwrites, resumable downloads, stalled uploads, a full disk
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refused_overwrite_leaves_the_version_in_its_feed() {
    // `dev` may overwrite; `stable` holds the same version, which pins its
    // payload. A different build under that id/version must be refused — and
    // refused *without* first taking the version out of `dev`.
    let server = spawn_feeds(|c| {
        c.api_key = Some(API_KEY.into());
        let mut dev = feed("dev");
        dev.allow_overwrite = Some(yanuget::config::OverwriteMode::Enabled);
        c.feeds = vec![dev, feed("stable")];
    })
    .await;
    let original = build_nupkg("Shared.Pkg", "1.0.0", b"first build");
    for path in ["/dev/api/v2/package", "/stable/api/v2/package"] {
        assert_eq!(
            push_to(&server, path, API_KEY, original.clone())
                .await
                .status(),
            reqwest::StatusCode::CREATED
        );
    }
    let rebuilt = build_nupkg("Shared.Pkg", "1.0.0", b"second build, other bytes");
    assert_eq!(
        push_to(&server, "/dev/api/v2/package", API_KEY, rebuilt)
            .await
            .status(),
        reqwest::StatusCode::CONFLICT
    );
    assert!(status_of(&server, "/dev/v3/package/shared.pkg/index.json")
        .await
        .is_success());
    // An identical re-push is still an overwrite dev allows.
    assert_eq!(
        push_to(&server, "/dev/api/v2/package", API_KEY, original)
            .await
            .status(),
        reqwest::StatusCode::CREATED
    );
}

#[tokio::test]
async fn a_delete_that_only_unlists_says_so_and_a_refused_push_says_why() {
    // The default feed: a delete unlists, overwrites are off.
    let server = spawn().await;
    let nupkg = build_nupkg("Fixed.Pkg", "11.3.0", b"first build");
    push_multipart(&server, API_KEY, nupkg).await;

    let deleted = server
        .client
        .delete(server.url("/api/v2/package/Fixed.Pkg/11.3.0"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);
    let warning = deleted.headers()["x-nuget-warning"].to_str().unwrap();
    assert!(warning.contains("was unlisted, not deleted"), "{warning}");

    // The corrected build under the same version: refused, and the status
    // line — all NuGet and Chocolatey print — says what and why.
    let refused = push_multipart(
        &server,
        API_KEY,
        build_nupkg("Fixed.Pkg", "11.3.0", b"fixed"),
    )
    .await;
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);
    let reason = refused
        .extensions()
        .get::<hyper::ext::ReasonPhrase>()
        .map(|r| String::from_utf8_lossy(r.as_bytes()).into_owned())
        .expect("a reason phrase");
    assert!(
        reason.starts_with("Fixed.Pkg 11.3.0 already exists in this feed (unlisted"),
        "{reason}"
    );
    assert!(reason.contains("allow_overwrite = false"), "{reason}");
    assert!(
        reason.contains("Push it as a new version, such as 11.3.0."),
        "{reason}"
    );
}

#[tokio::test]
async fn downloads_resume_safely_and_count_once() {
    let server = spawn().await;
    push_multipart(
        &server,
        API_KEY,
        build_nupkg("Resume.Pkg", "1.0.0", &[7u8; 4096]),
    )
    .await;
    let url = server.url("/v3/package/resume.pkg/1.0.0/resume.pkg.1.0.0.nupkg");

    // BITS opens with a HEAD: the size, validators and range support, no body.
    let head = server.client.head(&url).send().await.unwrap();
    assert_eq!(head.status(), reqwest::StatusCode::OK);
    let total: u64 = head.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert!(total > 0);
    assert_eq!(head.headers()["accept-ranges"], "bytes");
    let etag = head.headers()["etag"].to_str().unwrap().to_string();
    let last_modified = head.headers()["last-modified"]
        .to_str()
        .unwrap()
        .to_string();
    assert!(last_modified.ends_with(" GMT"), "{last_modified}");

    // The first chunk, then a resume that proves it is the same file.
    let first = server
        .client
        .get(&url)
        .header("range", "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    for validator in [etag.as_str(), last_modified.as_str()] {
        let rest = server
            .client
            .get(&url)
            .header("range", "bytes=100-")
            .header("if-range", validator)
            .send()
            .await
            .unwrap();
        assert_eq!(rest.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            rest.headers()["content-range"],
            format!("bytes 100-{}/{total}", total - 1).as_str()
        );
    }
    // A resume against a different file gets the whole current one.
    let stale = server
        .client
        .get(&url)
        .header("range", "bytes=100-")
        .header("if-range", "\"some-other-build\"")
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), reqwest::StatusCode::OK);
    assert_eq!(stale.bytes().await.unwrap().len() as u64, total);

    // HEAD, the continuations and the refused resume are one download.
    await_downloads(&server, "resume.pkg", 1).await;
    // And nothing else lands late.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    await_downloads(&server, "resume.pkg", 1).await;
}

#[tokio::test]
async fn a_stalled_upload_is_cut_off() {
    use futures::StreamExt;
    let server = spawn_with(|c| c.upload_idle_timeout_secs = 1).await;
    // A real package, split in two with a pause longer than the limit in the
    // middle. Were the stall not cut off, the second half would arrive and the
    // package would be published, which is what the checks below would catch.
    let nupkg = build_nupkg("Stalled.Upload", "1.0.0", b"x");
    let (head, tail) = nupkg.split_at(nupkg.len() / 2);
    let (head, tail) = (
        bytes::Bytes::from(head.to_vec()),
        bytes::Bytes::from(tail.to_vec()),
    );
    let stall = std::time::Duration::from_secs(4);
    let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(head) }).chain(
        futures::stream::once(async move {
            tokio::time::sleep(stall).await;
            Ok(tail)
        }),
    );
    let started = std::time::Instant::now();
    let result = server
        .client
        .put(server.url("/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .header("Content-Type", "application/octet-stream")
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await;
    // The server answers 408 and drops the connection; depending on timing the
    // client sees the answer or only the dropped connection.
    match result {
        Ok(resp) => assert_eq!(resp.status(), reqwest::StatusCode::REQUEST_TIMEOUT),
        Err(e) => assert!(
            !e.is_builder() && !e.is_status(),
            "the upload failed for another reason than a dropped connection: {e}"
        ),
    }
    // The answer came before the client would have finished on its own.
    assert!(
        started.elapsed() < stall,
        "the server waited out the stall: {:?}",
        started.elapsed()
    );
    // Nothing was published, and the half-received upload is not left behind.
    assert_eq!(
        status_of(&server, "/v3/package/stalled.upload/index.json").await,
        reqwest::StatusCode::NOT_FOUND
    );
    let staging = server._dir.path().join("packages/.uploads");
    let staged = || -> Vec<std::path::PathBuf> {
        std::fs::read_dir(&staging)
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default()
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !staged().is_empty() && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(staged(), Vec::<std::path::PathBuf>::new());
}

#[tokio::test]
async fn an_upload_that_would_fill_the_disk_is_refused() {
    let server = spawn_with(|c| c.min_free_disk_bytes = u64::MAX / 2).await;
    let resp = push_multipart(&server, API_KEY, build_nupkg("Full.Disk", "1.0.0", b"x")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(
        status_of(&server, "/v3/package/full.disk/index.json").await,
        reqwest::StatusCode::NOT_FOUND
    );
}

/// Start a resumable upload of `length` bytes for `Res.Pkg` 1.0.0, under
/// `name`: the status and, when created, its location.
async fn create_tus(
    server: &TestServer,
    name: &str,
    length: u64,
) -> (reqwest::StatusCode, Option<String>) {
    let created = tus(no_redirect().post(server.url("/api/v2/uploads")))
        .header("Upload-Length", length.to_string())
        .header(
            "Upload-Metadata",
            tus_metadata(&[("id", "Res.Pkg"), ("version", "1.0.0"), ("filename", name)]),
        )
        .send()
        .await
        .unwrap();
    let location = created
        .headers()
        .get("location")
        .map(|l| l.to_str().unwrap().to_string());
    (created.status(), location)
}

/// The free space of the volume [`spawn_with_room_for_one`] serves from.
const ROOM: u64 = 1_000_000;

/// A server whose free space holds one upload of the returned size, and not
/// two: each passes the free-space check alone. The free space is a fixed
/// [`ROOM`], so nothing else writing to the test machine's disk moves it.
async fn spawn_with_room_for_one() -> (TestServer, u64) {
    let server = spawn_with_free_space(
        |c| {
            c.min_free_disk_bytes = 1;
            c.max_package_size_bytes = None;
            c.files.max_file_size_bytes = None;
        },
        Some(ROOM),
    )
    .await;
    push_multipart(&server, API_KEY, build_nupkg("Res.Pkg", "1.0.0", b"x")).await;
    (server, ROOM / 10 * 6)
}

#[tokio::test]
async fn open_resumable_uploads_reserve_the_space_they_declared() {
    let (server, length) = spawn_with_room_for_one().await;
    let (status, first) = create_tus(&server, "a.wim", length).await;
    assert_eq!(status, reqwest::StatusCode::CREATED);
    // Nothing is written yet, but the first upload was promised the space.
    let (status, _) = create_tus(&server, "b.wim", length).await;
    assert_eq!(status, reqwest::StatusCode::INSUFFICIENT_STORAGE);
    // Abandoned, it gives the space back.
    let deleted = tus(server.client.delete(first.unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);
    let (status, _) = create_tus(&server, "b.wim", length).await;
    assert_eq!(status, reqwest::StatusCode::CREATED);
}

#[tokio::test]
async fn an_upload_in_progress_reserves_its_content_length_until_it_ends() {
    let (server, length) = spawn_with_room_for_one().await;
    // A PUT that declares `length` and then sends nothing.
    let (tx, rx) = futures::channel::mpsc::channel::<std::io::Result<Vec<u8>>>(1);
    let put = server
        .client
        .put(server.url("/api/v2/files/Res.Pkg/1.0.0/big.wim"))
        .header("X-NuGet-ApiKey", API_KEY)
        .header("content-length", length.to_string())
        .body(reqwest::Body::wrap_stream(rx))
        .send();
    let put = tokio::spawn(put);

    // Admitted, it has a temp file; from then on an upload of the same size
    // no longer fits. Without a reservation it always does.
    let staging = server._dir.path().join("packages/.uploads");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !std::fs::read_dir(&staging)
        .unwrap()
        .any(|e| e.unwrap().path().extension().is_some_and(|x| x == "tmp"))
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the PUT never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let (status, _) = create_tus(&server, "probe.wim", length).await;
    assert_eq!(status, reqwest::StatusCode::INSUFFICIENT_STORAGE);

    // The client gives up; the server's end of the upload drops, and with it
    // the reservation.
    drop(tx);
    put.abort();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (status, _) = create_tus(&server, "after.wim", length).await;
        if status == reqwest::StatusCode::CREATED {
            break;
        }
        assert_eq!(status, reqwest::StatusCode::INSUFFICIENT_STORAGE);
        assert!(
            std::time::Instant::now() < deadline,
            "the space was never released"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_feed_holds_a_bounded_number_of_open_resumable_uploads() {
    let server = spawn_with(|c| c.files.max_open_uploads = 2).await;
    push_multipart(&server, API_KEY, build_nupkg("Res.Pkg", "1.0.0", b"x")).await;
    let (status, first) = create_tus(&server, "a.wim", 10).await;
    assert_eq!(status, reqwest::StatusCode::CREATED);
    assert_eq!(
        create_tus(&server, "b.wim", 10).await.0,
        reqwest::StatusCode::CREATED
    );
    let (status, _) = create_tus(&server, "c.wim", 10).await;
    assert_eq!(status, reqwest::StatusCode::TOO_MANY_REQUESTS);
    // One finished makes room for another.
    let done = tus(server.client.patch(first.unwrap()))
        .header("Content-Type", "application/offset+octet-stream")
        .header("Upload-Offset", "0")
        .body(vec![7u8; 10])
        .send()
        .await
        .unwrap();
    assert_eq!(done.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        create_tus(&server, "c.wim", 10).await.0,
        reqwest::StatusCode::CREATED
    );
}

#[tokio::test]
async fn concurrent_creations_cannot_overrun_max_open_uploads() {
    let server = spawn_with(|c| c.files.max_open_uploads = 3).await;
    push_multipart(&server, API_KEY, build_nupkg("Res.Pkg", "1.0.0", b"x")).await;
    let names: Vec<String> = (0..4).map(|i| format!("{i}.wim")).collect();
    let statuses =
        futures::future::join_all(names.iter().map(|name| create_tus(&server, name, 10))).await;
    let count = |want| statuses.iter().filter(|(s, _)| *s == want).count();
    assert_eq!(count(reqwest::StatusCode::CREATED), 3, "{statuses:?}");
    assert_eq!(
        count(reqwest::StatusCode::TOO_MANY_REQUESTS),
        1,
        "{statuses:?}"
    );
}

#[tokio::test]
async fn max_open_uploads_of_zero_is_no_limit() {
    let server = spawn_with(|c| c.files.max_open_uploads = 0).await;
    push_multipart(&server, API_KEY, build_nupkg("Res.Pkg", "1.0.0", b"x")).await;
    for i in 0..5 {
        let (status, _) = create_tus(&server, &format!("{i}.wim"), 10).await;
        assert_eq!(status, reqwest::StatusCode::CREATED, "upload {i}");
    }
}

/// `PATCH` `body` onto the resumable upload at `location`, from `offset`.
async fn patch_tus(
    server: &TestServer,
    location: &str,
    offset: u64,
    body: Vec<u8>,
) -> reqwest::Response {
    tus(server.client.patch(location))
        .header("Content-Type", "application/offset+octet-stream")
        .header("Upload-Offset", offset.to_string())
        .body(body)
        .send()
        .await
        .unwrap()
}

#[tokio::test]
async fn a_partial_patch_no_longer_promises_what_it_wrote() {
    let (server, length) = spawn_with_room_for_one().await;
    let (_, first) = create_tus(&server, "a.wim", length).await;
    let first = first.unwrap();
    // The free space is fixed here, so what fits besides the first upload is
    // exactly what it no longer promises.
    let rest = ROOM - length / 2 - 10;
    assert_eq!(
        create_tus(&server, "b.wim", rest).await.0,
        reqwest::StatusCode::INSUFFICIENT_STORAGE
    );
    let half = patch_tus(&server, &first, 0, vec![1u8; (length / 2) as usize]).await;
    assert_eq!(half.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        create_tus(&server, "b.wim", rest).await.0,
        reqwest::StatusCode::CREATED
    );
}

#[tokio::test]
async fn an_abandoned_resumable_upload_stops_holding_its_space() {
    let (server, length) = spawn_with_room_for_one().await;
    let (_, first) = create_tus(&server, "a.wim", length).await;
    let first = first.unwrap();
    assert_eq!(
        create_tus(&server, "b.wim", length).await.0,
        reqwest::StatusCode::INSUFFICIENT_STORAGE
    );

    // The client went away without a `DELETE`. Past the hold, the upload is
    // still open, but holds nothing.
    let db = SqliteDatabase::connect(&server._dir.path().join("yanuget.db").to_string_lossy())
        .await
        .unwrap();
    let id = first.rsplit('/').next().unwrap();
    let idle_since = chrono::Utc::now() - chrono::Duration::hours(2);
    db.set_upload_held(id, Some(idle_since)).await.unwrap();
    let (status, second) = create_tus(&server, "b.wim", length).await;
    assert_eq!(status, reqwest::StatusCode::CREATED);

    // Coming back, it is admitted again like a new upload of what it has
    // left, and the space went to the other one meanwhile.
    let refused = patch_tus(&server, &first, 0, vec![1u8; 10]).await;
    assert_eq!(refused.status(), reqwest::StatusCode::INSUFFICIENT_STORAGE);
    // Refused, it keeps the hold it had.
    let held = db.get_upload(id).await.unwrap().unwrap().held_since;
    assert_eq!(held.map(|t| t.timestamp()), Some(idle_since.timestamp()));

    // Once the other one is abandoned, it goes on where it was.
    let deleted = tus(server.client.delete(second.unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);
    let resumed = patch_tus(&server, &first, 0, vec![1u8; 10]).await;
    assert_eq!(resumed.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(resumed.headers()["upload-offset"], "10");
}

#[tokio::test]
async fn a_push_declaring_more_than_the_size_limit_reserves_nothing() {
    let server = spawn_with_free_space(
        |c| {
            c.min_free_disk_bytes = 1;
            c.max_package_size_bytes = Some(10_000);
        },
        Some(ROOM),
    )
    .await;
    // Bigger than the volume: reserved first, it was refused as a full disk,
    // and while it trickled in it kept every other upload out.
    let resp = server
        .client
        .put(server.url("/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(vec![0u8; 2 * ROOM as usize])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    // A multipart push of a package under the limit is not refused for its
    // boundaries.
    let pushed = push_multipart(&server, API_KEY, build_nupkg("Small.Pkg", "1.0.0", b"x")).await;
    assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
}
