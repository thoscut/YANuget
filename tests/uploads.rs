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
            // These are about the space of the whole volume; a feed's share
            // of it has tests of its own below.
            c.files.max_upload_space_percent = 100;
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

/// The server's database, opened beside it.
async fn database_of(server: &TestServer) -> SqliteDatabase {
    SqliteDatabase::connect(&server._dir.path().join("yanuget.db").to_string_lossy())
        .await
        .unwrap()
}

#[tokio::test]
async fn a_patch_that_brings_nothing_does_not_renew_an_uploads_hold() {
    let (server, length) = spawn_with_room_for_one().await;
    let (_, first) = create_tus(&server, "a.wim", length).await;
    let first = first.unwrap();
    let db = database_of(&server).await;
    let id = first.rsplit('/').next().unwrap();
    let held = || async {
        let since = db.get_upload(id).await.unwrap().unwrap().held_since;
        since.map(|t| t.timestamp())
    };

    // Half an hour in, an empty `PATCH`: accepted, and the hold is as old as
    // it was. Renewed, one such request an hour kept the space promised for
    // as long as the upload may stay open.
    let half_an_hour_ago = chrono::Utc::now() - chrono::Duration::minutes(30);
    db.set_upload_held(id, Some(half_an_hour_ago))
        .await
        .unwrap();
    let empty = patch_tus(&server, &first, 0, Vec::new()).await;
    assert_eq!(empty.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(empty.headers()["upload-offset"], "0");
    assert_eq!(held().await, Some(half_an_hour_ago.timestamp()));
    assert_eq!(
        create_tus(&server, "b.wim", length).await.0,
        reqwest::StatusCode::INSUFFICIENT_STORAGE,
        "inside the hour the upload still holds its space"
    );

    // Idle past the hold, an empty `PATCH` does not bring the hold back: the
    // space is free for another upload.
    let idle_since = chrono::Utc::now() - chrono::Duration::hours(2);
    db.set_upload_held(id, Some(idle_since)).await.unwrap();
    let empty = patch_tus(&server, &first, 0, Vec::new()).await;
    assert_eq!(empty.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(held().await, Some(idle_since.timestamp()));
    let (status, second) = create_tus(&server, "b.wim", length).await;
    assert_eq!(status, reqwest::StatusCode::CREATED);
    let deleted = tus(server.client.delete(second.unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);

    // Bytes do renew it: as many as are due in an hour for the upload to
    // arrive before it expires (see the next test for fewer).
    let before = chrono::Utc::now().timestamp();
    let an_hours_worth = (length / 72 + 1) as usize;
    let some = patch_tus(&server, &first, 0, vec![1u8; an_hours_worth]).await;
    assert_eq!(some.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(held().await.is_some_and(|t| t >= before));
    assert_eq!(
        create_tus(&server, "b.wim", length).await.0,
        reqwest::StatusCode::INSUFFICIENT_STORAGE
    );
}

#[tokio::test]
async fn a_trickle_of_bytes_does_not_keep_an_uploads_hold() {
    let (server, length) = spawn_with_room_for_one().await;
    let (_, first) = create_tus(&server, "a.wim", length).await;
    let first = first.unwrap();
    let db = database_of(&server).await;
    let id = first.rsplit('/').next().unwrap();
    let held = || async { db.get_upload(id).await.unwrap().unwrap().held_since };

    // A byte just before the hour is up buys the upload next to nothing:
    // a byte an hour no longer keeps the space promised for 72 hours.
    let almost_an_hour_ago = chrono::Utc::now() - chrono::Duration::minutes(59);
    db.set_upload_held(id, Some(almost_an_hour_ago))
        .await
        .unwrap();
    let byte = patch_tus(&server, &first, 0, vec![1u8]).await;
    assert_eq!(byte.status(), reqwest::StatusCode::NO_CONTENT);
    let since = held().await.unwrap();
    assert!(
        since < almost_an_hour_ago + chrono::Duration::seconds(30),
        "a byte renewed the hold to {since}"
    );
    // Once that has run out the space is free for another upload…
    db.set_upload_held(id, Some(since - chrono::Duration::minutes(2)))
        .await
        .unwrap();
    let (status, second) = create_tus(&server, "b.wim", length).await;
    assert_eq!(status, reqwest::StatusCode::CREATED);
    // …and the trickle has to be admitted again, which it no longer is.
    let refused = patch_tus(&server, &first, 1, vec![1u8]).await;
    assert_eq!(refused.status(), reqwest::StatusCode::INSUFFICIENT_STORAGE);
    let deleted = tus(server.client.delete(second.unwrap()))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), reqwest::StatusCode::NO_CONTENT);

    // Half an hour's worth of bytes buys half an hour.
    let an_hour_ago = chrono::Utc::now() - chrono::Duration::hours(1);
    db.set_upload_held(id, Some(an_hour_ago + chrono::Duration::minutes(1)))
        .await
        .unwrap();
    let half = (length / 72 / 2) as usize;
    let some = patch_tus(&server, &first, 1, vec![1u8; half]).await;
    assert_eq!(some.status(), reqwest::StatusCode::NO_CONTENT);
    let since = held().await.unwrap();
    let expected = an_hour_ago + chrono::Duration::minutes(31);
    assert!(
        (since - expected).num_seconds().abs() < 30,
        "held since {since}, expected about {expected}"
    );
}

/// The push key of the feed `prod` in [`spawn_two_feeds_sharing_room`];
/// `team` takes the global [`API_KEY`].
const PROD_KEY: &str = "prod-key";

fn key_of(feed: &str) -> &'static str {
    if feed == "prod" {
        PROD_KEY
    } else {
        API_KEY
    }
}

/// Two feeds, `team` and `prod`, on a volume with [`ROOM`] free, each with
/// `Res.Pkg` 1.0.0 to attach files to. `prod` has a push key of its own
/// ([`PROD_KEY`]), so the two do not share a part of the volume unless
/// `shared_key` gives `prod` the global one too.
async fn spawn_two_feeds_sharing_room(percent: Option<u8>) -> TestServer {
    spawn_two_feeds_with_keys(percent, false).await
}

async fn spawn_two_feeds_with_keys(percent: Option<u8>, shared_key: bool) -> TestServer {
    let server = spawn_feeds_with_free_space(
        |c| {
            c.api_key = Some(API_KEY.into());
            let mut prod = feed("prod");
            if !shared_key {
                prod.api_key = Some(PROD_KEY.into());
            }
            c.feeds = vec![feed("team"), prod];
            c.min_free_disk_bytes = 1;
            if let Some(percent) = percent {
                c.files.max_upload_space_percent = percent;
            }
        },
        Some(ROOM),
    )
    .await;
    for name in ["team", "prod"] {
        let key = if shared_key { API_KEY } else { key_of(name) };
        let pushed = push_to(
            &server,
            &format!("/{name}/api/v2/package"),
            key,
            build_nupkg("Res.Pkg", "1.0.0", b"x"),
        )
        .await;
        assert_eq!(pushed.status(), reqwest::StatusCode::CREATED);
    }
    server
}

/// [`create_tus`] in the feed `feed`, with its push key.
async fn create_tus_in(
    server: &TestServer,
    feed: &str,
    name: &str,
    length: u64,
) -> (reqwest::StatusCode, Option<String>) {
    create_tus_with(server, feed, key_of(feed), name, length).await
}

/// [`create_tus`] in the feed `feed`, with the push key `key`.
async fn create_tus_with(
    server: &TestServer,
    feed: &str,
    key: &str,
    name: &str,
    length: u64,
) -> (reqwest::StatusCode, Option<String>) {
    let created = no_redirect()
        .post(server.url(&format!("/{feed}/api/v2/uploads")))
        .header("Tus-Resumable", "1.0.0")
        .header("X-NuGet-ApiKey", key)
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

#[tokio::test]
async fn one_feed_cannot_promise_the_whole_volume_to_its_resumable_uploads() {
    use reqwest::StatusCode;
    // By default a feed's uploads may hold four fifths of the free space.
    let server = spawn_two_feeds_sharing_room(None).await;
    let (status, _) = create_tus_in(&server, "team", "a.wim", ROOM / 10 * 7).await;
    assert_eq!(status, StatusCode::CREATED);
    // Another two tenths would fit on the volume, but not in the feed's share
    // of it…
    let (status, _) = create_tus_in(&server, "team", "b.wim", ROOM / 10 * 2).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    // …which is there for the other feed.
    let (status, _) = create_tus_in(&server, "prod", "b.wim", ROOM / 10 * 2).await;
    assert_eq!(status, StatusCode::CREATED);

    // With no cap, the volume is the only limit, as it used to be.
    let server = spawn_two_feeds_sharing_room(Some(100)).await;
    let (status, _) = create_tus_in(&server, "team", "a.wim", ROOM / 10 * 7).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = create_tus_in(&server, "team", "b.wim", ROOM / 10 * 2).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = create_tus_in(&server, "prod", "b.wim", ROOM / 10 * 2).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
}

#[tokio::test]
async fn feeds_that_share_a_push_key_share_one_part_of_the_volume() {
    use reqwest::StatusCode;
    // Both feeds take the global key: whoever holds it holds four fifths of
    // the free space across the two, not four fifths in each.
    let server = spawn_two_feeds_with_keys(None, true).await;
    let (status, _) = create_tus_with(&server, "team", API_KEY, "a.wim", ROOM / 10 * 7).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = create_tus_with(&server, "prod", API_KEY, "b.wim", ROOM / 10 * 2).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    // A push of the same size would take the rest from the mirrors and the
    // inbox just as well.
    let refused = server
        .client
        .put(server.url("/prod/api/v2/files/Res.Pkg/1.0.0/c.wim"))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(vec![0u8; (ROOM / 10 * 2) as usize])
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::INSUFFICIENT_STORAGE);
    // What is under the share still goes.
    let (status, _) = create_tus_with(&server, "prod", API_KEY, "b.wim", ROOM / 100 * 5).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn uploads_in_one_request_count_towards_their_feeds_share() {
    use reqwest::StatusCode;
    let server = spawn_two_feeds_sharing_room(None).await;
    // A PUT that declares seven tenths and then sends nothing holds them
    // while it runs, in its feed's share as on the volume.
    let (tx, rx) = futures::channel::mpsc::channel::<std::io::Result<Vec<u8>>>(1);
    let put = server
        .client
        .put(server.url("/team/api/v2/files/Res.Pkg/1.0.0/big.wim"))
        .header("X-NuGet-ApiKey", API_KEY)
        .header("content-length", (ROOM / 10 * 7).to_string())
        .body(reqwest::Body::wrap_stream(rx))
        .send();
    let put = tokio::spawn(put);
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
    // Two more tenths fit on the volume, but not in the feed's share.
    let (status, _) = create_tus_in(&server, "team", "b.wim", ROOM / 10 * 2).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    let (status, _) = create_tus_in(&server, "prod", "b.wim", ROOM / 10 * 2).await;
    assert_eq!(status, StatusCode::CREATED);
    drop(tx);
    put.abort();

    // The other way round: a resumable upload holds the share, and a push
    // of what is left on the volume is refused.
    let server = spawn_two_feeds_sharing_room(None).await;
    let (status, _) = create_tus_in(&server, "team", "a.wim", ROOM / 10 * 7).await;
    assert_eq!(status, StatusCode::CREATED);
    let refused = server
        .client
        .put(server.url("/team/api/v2/files/Res.Pkg/1.0.0/c.wim"))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(vec![0u8; (ROOM / 10 * 2) as usize])
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), StatusCode::INSUFFICIENT_STORAGE);
    // A small push still fits in what is left of the share.
    let pushed = push_to(
        &server,
        "/team/api/v2/package",
        API_KEY,
        build_nupkg("Small.Pkg", "1.0.0", b"x"),
    )
    .await;
    assert_eq!(pushed.status(), StatusCode::CREATED);

    // Alone, a file larger than the share still goes in one `PUT`, though
    // not resumably: its promise ends with its request.
    let server = spawn_two_feeds_sharing_room(None).await;
    let (status, _) = create_tus_in(&server, "team", "a.wim", ROOM / 10 * 9).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    let put = server
        .client
        .put(server.url("/team/api/v2/files/Res.Pkg/1.0.0/a.wim"))
        .header("X-NuGet-ApiKey", API_KEY)
        .body(vec![0u8; (ROOM / 10 * 9) as usize])
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn an_upload_being_written_counts_towards_its_feeds_share() {
    use reqwest::StatusCode;
    let server = spawn_two_feeds_sharing_room(None).await;
    let (_, first) = create_tus_in(&server, "team", "a.wim", ROOM / 10 * 7).await;
    let first = first.unwrap();
    let db = database_of(&server).await;
    let id = first.rsplit('/').next().unwrap();

    // A `PATCH` that sends a few bytes and then nothing: while it runs, the
    // request holds the space, not the upload's row.
    let (mut tx, rx) = futures::channel::mpsc::channel::<std::io::Result<Vec<u8>>>(1);
    tx.try_send(Ok(vec![1u8; 10])).unwrap();
    let patch = tus(server.client.patch(&first))
        .header("Content-Type", "application/offset+octet-stream")
        .header("Upload-Offset", "0")
        .body(reqwest::Body::wrap_stream(rx))
        .send();
    let patch = tokio::spawn(patch);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while db
        .get_upload(id)
        .await
        .unwrap()
        .unwrap()
        .held_since
        .is_some()
    {
        assert!(
            std::time::Instant::now() < deadline,
            "the PATCH never started"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // A trickling request is held to the share like an idle upload: the feed
    // cannot open more beside it, and the other feed can.
    let (status, _) = create_tus_in(&server, "team", "b.wim", ROOM / 10 * 2).await;
    assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
    let (status, _) = create_tus_in(&server, "prod", "b.wim", ROOM / 10 * 2).await;
    assert_eq!(status, StatusCode::CREATED);
    drop(tx);
    patch.abort();
}

#[tokio::test]
async fn an_upload_whose_hold_lapsed_comes_back_into_its_feeds_share() {
    use reqwest::StatusCode;
    let server = spawn_two_feeds_sharing_room(None).await;
    let length = ROOM / 100 * 45;
    let (_, first) = create_tus_in(&server, "team", "a.wim", length).await;
    let first = first.unwrap();
    let db = database_of(&server).await;
    let id = first.rsplit('/').next().unwrap();
    let idle_since = chrono::Utc::now() - chrono::Duration::hours(2);
    db.set_upload_held(id, Some(idle_since)).await.unwrap();

    // Idle, it holds nothing, so the feed may open another as large.
    let (status, second) = create_tus_in(&server, "team", "b.wim", length).await;
    assert_eq!(status, StatusCode::CREATED);
    // Both fit on the volume, but together they are more than the feed's
    // share: the one that let go of its space does not get it back.
    let refused = patch_tus(&server, &first, 0, vec![1u8; 10]).await;
    assert_eq!(refused.status(), StatusCode::INSUFFICIENT_STORAGE);
    // The one that holds its space goes on, whatever else the feed has open.
    let second = second.unwrap();
    let went_on = patch_tus(&server, &second, 0, vec![1u8; 10]).await;
    assert_eq!(went_on.status(), StatusCode::NO_CONTENT);
    // And once it is gone, so does the first.
    let deleted = tus(server.client.delete(&second)).send().await.unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    let resumed = patch_tus(&server, &first, 0, vec![1u8; 10]).await;
    assert_eq!(resumed.status(), StatusCode::NO_CONTENT);
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

// ---------------------------------------------------------------------------
// Resuming after a restart, and expiry
// ---------------------------------------------------------------------------

/// The running SHA-256 of an upload lives only in memory. After a restart
/// the first request rebuilds it from the partial file, and the finished
/// file must have the hash of every byte, not only of those sent since.
#[cfg(unix)]
#[tokio::test]
async fn an_upload_resumed_after_a_restart_has_the_hash_of_all_its_bytes() {
    use common::binary::{server_command_with, start};
    use reqwest::StatusCode;
    let dir = tempfile::tempdir().unwrap();
    // The machine's disk is not the test's to judge.
    let config = format!("api_key = '{API_KEY}'\nmin_free_disk_bytes = 0\n");
    let client = reqwest::Client::new();
    // Over two of the buffers the hash is rebuilt with, and not a multiple.
    let bytes: Vec<u8> = (0..700_001u32)
        .map(|i| (i % 251) as u8 ^ (i >> 11) as u8)
        .collect();
    let half = 600_001;

    let server = start(server_command_with(dir.path(), &config)).await;
    let part = reqwest::multipart::Part::bytes(build_nupkg("Res.Pkg", "1.0.0", b"x"))
        .file_name("package.nupkg");
    let pushed = client
        .put(server.url("/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .multipart(reqwest::multipart::Form::new().part("package", part))
        .send()
        .await
        .unwrap();
    assert_eq!(pushed.status(), StatusCode::CREATED);
    let created = tus(no_redirect().post(server.url("/api/v2/uploads")))
        .header("Upload-Length", bytes.len().to_string())
        .header(
            "Upload-Metadata",
            tus_metadata(&[
                ("id", "Res.Pkg"),
                ("version", "1.0.0"),
                ("filename", "a.wim"),
                ("sha256", &sha256_hex(&bytes)),
            ]),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    // The port changes with the restart; the upload's path does not.
    let location = created.headers()["location"].to_str().unwrap().to_string();
    let path = location
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .map(|(_, path)| format!("/{path}"))
        .unwrap();
    let first = tus(client.patch(server.url(&path)))
        .header("Content-Type", "application/offset+octet-stream")
        .header("Upload-Offset", "0")
        .body(bytes[..half].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::NO_CONTENT);
    assert!(server
        .terminate(std::time::Duration::from_secs(30))
        .await
        .success());

    let server = start(server_command_with(dir.path(), &config)).await;
    let offset = tus(client.head(server.url(&path))).send().await.unwrap();
    assert_eq!(offset.headers()["upload-offset"], half.to_string().as_str());
    let rest = tus(client.patch(server.url(&path)))
        .header("Content-Type", "application/offset+octet-stream")
        .header("Upload-Offset", half.to_string())
        .body(bytes[half..].to_vec())
        .send()
        .await
        .unwrap();
    // With a hash of the second part only, the declared checksum fails.
    assert_eq!(
        rest.status(),
        StatusCode::NO_CONTENT,
        "{}",
        rest.text().await.unwrap()
    );
    let index: serde_json::Value = client
        .get(server.url("/files/Res.Pkg/1.0.0/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(index["files"][0]["sha256"], sha256_hex(&bytes));
    let served = client
        .get(server.url("/files/Res.Pkg/1.0.0/a.wim"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert!(served[..] == bytes[..], "the served file differs");
    assert!(server
        .terminate(std::time::Duration::from_secs(30))
        .await
        .success());
}

#[tokio::test]
async fn the_expiry_sweep_drops_expired_uploads_and_keeps_current_ones() {
    use yanuget::database::UploadSession;
    let dir = tempfile::tempdir().unwrap();
    let db = SqliteDatabase::connect(&dir.path().join("yanuget.db").to_string_lossy())
        .await
        .unwrap();
    let staging = dir.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    let now = chrono::Utc::now();
    let upload = |id: &str, expires| UploadSession {
        id: id.into(),
        feed: "default".into(),
        lower_id: "res.pkg".into(),
        normalized_version: "1.0.0".into(),
        name: format!("{id}.wim"),
        length: 100,
        received: 10,
        expected_sha256: None,
        expires,
        held_since: Some(now),
    };
    for (id, expires) in [
        ("expired", now - chrono::Duration::minutes(1)),
        ("current", now + chrono::Duration::hours(1)),
    ] {
        db.create_upload(&upload(id, expires)).await.unwrap();
        std::fs::write(staging.join(format!("{id}.part")), [0u8; 10]).unwrap();
    }

    assert_eq!(yanuget::web::sweep_expired_uploads(&db, &staging).await, 1);
    assert!(db.get_upload("expired").await.unwrap().is_none());
    assert!(!staging.join("expired.part").exists());
    assert!(db.get_upload("current").await.unwrap().is_some());
    assert!(staging.join("current.part").exists());
    // Nothing left to drop.
    assert_eq!(yanuget::web::sweep_expired_uploads(&db, &staging).await, 0);
}
