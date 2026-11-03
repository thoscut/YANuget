//! End-to-end HTTP tests exercising the real router over a TCP socket with a
//! real `reqwest` client, a filesystem store and an on-disk SQLite database.
//!
//! This file holds the core NuGet API: push, restore, delete, overwrite,
//! rate limiting, retention on push and concurrent pushes. The other areas
//! have files of their own, and the shared helpers live in `common`.

mod common;

use std::sync::Arc;

use common::*;

#[tokio::test]
async fn service_index_is_served() {
    let server = spawn().await;
    let resp = server
        .client
        .get(server.url("/v3/index.json"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["version"], "3.0.0");
    let types: Vec<&str> = body["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["@type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"PackagePublish/2.0.0"));
    assert!(types.contains(&"SearchQueryService"));
}

#[tokio::test]
async fn push_requires_api_key() {
    let server = spawn().await;
    let nupkg = build_nupkg("Auth.Test", "1.0.0", b"x");
    let resp = push_multipart(&server, "wrong-key", nupkg).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn any_configured_api_key_authenticates_push() {
    let server = spawn_with(|c| c.api_keys = vec!["team-key".into()]).await;
    // The primary key still works...
    let resp = push_multipart(&server, API_KEY, build_nupkg("Multi.Key", "1.0.0", b"a")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    // ...and so does an additional configured key.
    let resp = push_multipart(&server, "team-key", build_nupkg("Multi.Key", "2.0.0", b"b")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    // An unconfigured key is still rejected.
    let resp = push_multipart(&server, "nope", build_nupkg("Multi.Key", "3.0.0", b"c")).await;
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn full_publish_and_consume_flow() {
    let server = spawn().await;
    let payload = vec![0xABu8; 64 * 1024];
    let nupkg = build_nupkg("Contoso.Utils", "1.2.3", &payload);
    let expected_len = nupkg.len();

    // Push.
    let resp = push_multipart(&server, API_KEY, nupkg.clone()).await;
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    // Duplicate push is rejected.
    let dup = push_multipart(&server, API_KEY, nupkg.clone()).await;
    assert_eq!(dup.status(), reqwest::StatusCode::CONFLICT);

    // Flat-container version list.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/contoso.utils/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"][0], "1.2.3");

    // Registration index with dependency groups.
    let reg: serde_json::Value = server
        .client
        .get(server.url("/v3/registration/contoso.utils/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = &reg["items"][0]["items"][0]["catalogEntry"];
    assert_eq!(entry["id"], "Contoso.Utils");
    assert_eq!(entry["version"], "1.2.3");
    assert_eq!(
        entry["dependencyGroups"][0]["dependencies"][0]["id"],
        "Newtonsoft.Json"
    );

    // Search finds it.
    let search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=contoso"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search["totalHits"], 1);
    assert_eq!(search["data"][0]["id"], "Contoso.Utils");

    // Download returns the exact bytes that were pushed.
    let download = server
        .client
        .get(server.url("/v3/package/contoso.utils/1.2.3/contoso.utils.1.2.3.nupkg"))
        .send()
        .await
        .unwrap();
    assert!(download.status().is_success());
    assert_eq!(download.headers()["accept-ranges"], "bytes");
    let downloaded = download.bytes().await.unwrap();
    assert_eq!(downloaded.len(), expected_len);
    assert_eq!(&downloaded[..], &nupkg[..]);
}

#[tokio::test]
async fn range_request_returns_partial_content() {
    let server = spawn().await;
    let nupkg = build_nupkg("Range.Test", "1.0.0", &[7u8; 8192]);
    push_multipart(&server, API_KEY, nupkg.clone()).await;

    let resp = server
        .client
        .get(server.url("/v3/package/range.test/1.0.0/range.test.1.0.0.nupkg"))
        .header("Range", "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    assert_eq!(resp.headers()["content-length"], "10");
    let content_range = resp.headers()["content-range"].to_str().unwrap();
    assert!(content_range.starts_with("bytes 0-9/"));
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 10);
    assert_eq!(&body[..], &nupkg[..10]);
}

#[tokio::test]
async fn raw_body_push_is_supported() {
    // Some clients PUT the raw .nupkg instead of multipart form data.
    let server = spawn().await;
    let nupkg = build_nupkg("Raw.Push", "2.0.0", b"raw");
    let resp = server
        .client
        .put(server.url("/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .header("Content-Type", "application/octet-stream")
        .body(nupkg)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/raw.push/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"][0], "2.0.0");
}

#[tokio::test]
async fn push_accepts_trailing_slash() {
    // The NuGet client appends a trailing slash to the publish endpoint, so it
    // PUTs to `/api/v2/package/` rather than `/api/v2/package`. Both must work.
    let server = spawn().await;
    let nupkg = build_nupkg("Trailing.Slash", "1.0.0", b"slash");
    let part = reqwest::multipart::Part::bytes(nupkg)
        .file_name("package.nupkg")
        .mime_str("application/octet-stream")
        .unwrap();
    let form = reqwest::multipart::Form::new().part("package", part);
    let resp = server
        .client
        .put(server.url("/api/v2/package/"))
        .header("X-NuGet-ApiKey", API_KEY)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::CREATED);

    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/trailing.slash/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(versions["versions"][0], "1.0.0");
}

#[tokio::test]
async fn delete_unlists_package() {
    let server = spawn().await;
    let nupkg = build_nupkg("Unlist.Me", "1.0.0", b"data");
    push_multipart(&server, API_KEY, nupkg).await;

    // Unlist (default delete behaviour).
    let resp = server
        .client
        .delete(server.url("/api/v2/package/unlist.me/1.0.0"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);

    // No longer in search...
    let search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=unlist"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search["totalHits"], 0);

    // ...but the flat container must still list it. That endpoint is how a
    // client resolves a version it is about to restore, so omitting unlisted
    // versions makes a project pinned to one fail with NU1101 — which defeats
    // the entire point of unlisting rather than deleting. (Verified against the
    // real `dotnet restore`, which reported exactly that before this was fixed.)
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/unlist.me/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        versions["versions"],
        serde_json::json!(["1.0.0"]),
        "an unlisted version must stay resolvable through the flat container"
    );

    // ...and still downloadable by exact version (NuGet restore semantics).
    let download = server
        .client
        .get(server.url("/v3/package/unlist.me/1.0.0/unlist.me.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert!(download.status().is_success());

    // Search still hides it — discovery and resolution are different questions.
    let search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=unlist"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search["totalHits"], 0);

    // Relist restores it.
    let resp = server
        .client
        .post(server.url("/api/v2/package/unlist.me/1.0.0"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let search: serde_json::Value = server
        .client
        .get(server.url("/v3/search?q=unlist"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(search["totalHits"], 1);
}

#[tokio::test]
async fn nuspec_endpoint_serves_manifest() {
    let server = spawn().await;
    let nupkg = build_nupkg("Manifest.Test", "1.0.0", b"data");
    push_multipart(&server, API_KEY, nupkg).await;

    let resp = server
        .client
        .get(server.url("/v3/package/manifest.test/1.0.0/manifest.test.nuspec"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert!(resp.headers()["content-type"]
        .to_str()
        .unwrap()
        .contains("xml"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("<id>Manifest.Test</id>"));
}

#[tokio::test]
async fn missing_package_returns_404() {
    let server = spawn().await;
    let resp = server
        .client
        .get(server.url("/v3/package/does.not.exist/index.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn registration_leaf_serves_single_version() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Leaf.Pkg", "1.2.3", b"x")).await;
    let leaf: serde_json::Value = server
        .client
        .get(server.url("/v3/registration/leaf.pkg/1.2.3.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // The standalone leaf has the spec's leaf shape: `catalogEntry` is a URL
    // (there is no catalog, so it names the leaf), and `listed`/`published`
    // sit at the top level. It used to repeat a page item's shape.
    let leaf_url = leaf["@id"].as_str().unwrap();
    assert!(leaf_url.ends_with("/v3/registration/leaf.pkg/1.2.3.json"));
    assert_eq!(leaf["catalogEntry"], leaf_url);
    assert_eq!(leaf["listed"], true);
    assert!(leaf["published"].is_string());
    assert!(leaf["packageContent"]
        .as_str()
        .unwrap()
        .ends_with("/v3/package/leaf.pkg/1.2.3/leaf.pkg.1.2.3.nupkg"));
    assert!(leaf["registration"]
        .as_str()
        .unwrap()
        .ends_with("/v3/registration/leaf.pkg/index.json"));
}

#[tokio::test]
async fn autocomplete_ids_and_versions() {
    let server = spawn().await;
    push_multipart(&server, API_KEY, build_nupkg("Auto.Cli", "1.0.0", b"x")).await;
    push_multipart(&server, API_KEY, build_nupkg("Auto.Cli", "1.1.0", b"x")).await;
    push_multipart(&server, API_KEY, build_nupkg("Auto.Core", "1.0.0", b"x")).await;

    // Id autocomplete.
    let ids: serde_json::Value = server
        .client
        .get(server.url("/v3/autocomplete?q=auto"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let data = ids["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);

    // Version enumeration for one id.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/autocomplete?id=auto.cli"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let vs: Vec<&str> = versions["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(vs, vec!["1.0.0", "1.1.0"]);
}

#[tokio::test]
async fn overwrite_allows_republish() {
    let server = spawn_with(|c| c.allow_overwrite = yanuget::config::OverwriteMode::Enabled).await;
    let nupkg = build_nupkg("Over.Write", "1.0.0", b"x");
    assert_eq!(
        push_multipart(&server, API_KEY, nupkg.clone())
            .await
            .status(),
        reqwest::StatusCode::CREATED
    );
    // Re-pushing the same version succeeds instead of conflicting.
    assert_eq!(
        push_multipart(&server, API_KEY, nupkg).await.status(),
        reqwest::StatusCode::CREATED
    );
}

#[tokio::test]
async fn prerelease_only_overwrite_protects_stable_versions() {
    let server =
        spawn_with(|c| c.allow_overwrite = yanuget::config::OverwriteMode::PrereleaseOnly).await;

    // A stable version is immutable: the re-push conflicts.
    let stable = build_nupkg("Pre.Only", "1.0.0", b"a");
    assert_eq!(
        push_multipart(&server, API_KEY, stable.clone())
            .await
            .status(),
        reqwest::StatusCode::CREATED
    );
    assert_eq!(
        push_multipart(&server, API_KEY, stable).await.status(),
        reqwest::StatusCode::CONFLICT
    );

    // A pre-release version may be overwritten.
    let pre = build_nupkg("Pre.Only", "2.0.0-rc.1", b"a");
    assert_eq!(
        push_multipart(&server, API_KEY, pre.clone()).await.status(),
        reqwest::StatusCode::CREATED
    );
    assert_eq!(
        push_multipart(&server, API_KEY, pre).await.status(),
        reqwest::StatusCode::CREATED
    );
}

#[tokio::test]
async fn hard_delete_removes_payload() {
    let server = spawn_with(|c| c.hard_delete_enabled = true).await;
    push_multipart(&server, API_KEY, build_nupkg("Hard.Del", "1.0.0", b"x")).await;
    let resp = server
        .client
        .delete(server.url("/api/v2/package/hard.del/1.0.0"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
    // Gone for good — not downloadable.
    let dl = server
        .client
        .get(server.url("/v3/package/hard.del/1.0.0/hard.del.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(dl.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn oversized_upload_is_rejected() {
    let server = spawn_with(|c| c.max_package_size_bytes = Some(64)).await;
    let big = build_nupkg("Too.Big", "1.0.0", &[0u8; 4096]);
    let resp = server
        .client
        .put(server.url("/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .header("Content-Type", "application/octet-stream")
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn rate_limit_returns_429_after_threshold() {
    let server = spawn_with(|c| {
        c.rate_limit.enabled = true;
        c.rate_limit.max_requests = 3;
        c.rate_limit.window_secs = 60;
        // This test identifies its client by `X-Forwarded-For`, which is only
        // honoured from a trusted peer.
        c.trusted_proxies = vec!["private".into()];
    })
    .await;
    let url = server.url("/health");

    // Three requests from one client IP (carried in X-Forwarded-For) pass; the
    // fourth is throttled.
    for _ in 0..3 {
        let resp = server
            .client
            .get(&url)
            .header("X-Forwarded-For", "9.9.9.9")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
    }
    let resp = server
        .client
        .get(&url)
        .header("X-Forwarded-For", "9.9.9.9")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);

    // A different client IP has its own budget.
    let resp = server
        .client
        .get(&url)
        .header("X-Forwarded-For", "8.8.8.8")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prune_on_push_keeps_newest_versions() {
    let server = spawn_with(|c| {
        c.retention.enabled = true;
        c.retention.prune_on_push = true;
        c.retention.keep_latest_stable = Some(2);
    })
    .await;

    for v in ["1.0.0", "1.1.0", "1.2.0", "1.3.0"] {
        let resp = push_multipart(&server, API_KEY, build_nupkg("Keep.Me", v, b"x")).await;
        assert_eq!(resp.status(), reqwest::StatusCode::CREATED);
    }

    // Only the two newest stable versions remain.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/keep.me/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list: Vec<String> = versions["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert_eq!(list, vec!["1.2.0".to_string(), "1.3.0".to_string()]);

    // The pruned payload is gone from storage too.
    let gone = server
        .client
        .get(server.url("/v3/package/keep.me/1.0.0/keep.me.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), reqwest::StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_distinct_pushes_all_succeed() {
    let server = Arc::new(spawn().await);
    let mut handles = Vec::new();
    for i in 0..12 {
        let s = server.clone();
        handles.push(tokio::spawn(async move {
            let nupkg = build_nupkg(&format!("Conc.P{i}"), "1.0.0", b"x");
            push_multipart(s.as_ref(), API_KEY, nupkg).await.status()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), reqwest::StatusCode::CREATED);
    }
    // Every package is queryable afterwards.
    for i in 0..12 {
        let resp = server
            .client
            .get(server.url(&format!("/v3/package/conc.p{i}/index.json")))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "Conc.P{i} missing");
    }
}

#[tokio::test]
async fn concurrent_same_version_push_one_wins() {
    let server = Arc::new(spawn().await);
    let mut handles = Vec::new();
    for _ in 0..6 {
        let s = server.clone();
        handles.push(tokio::spawn(async move {
            push_multipart(s.as_ref(), API_KEY, build_nupkg("Race.Pkg", "1.0.0", b"x"))
                .await
                .status()
        }));
    }
    let mut created = 0;
    let mut conflict = 0;
    for h in handles {
        match h.await.unwrap() {
            reqwest::StatusCode::CREATED => created += 1,
            reqwest::StatusCode::CONFLICT => conflict += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    // The unique constraint guarantees exactly one winner under the race.
    assert_eq!(created, 1);
    assert_eq!(conflict, 5);

    // The winner's payload must survive: a loser's rollback must never delete
    // the shared version directory the winner owns.
    let download = server
        .client
        .get(server.url("/v3/package/race.pkg/1.0.0/race.pkg.1.0.0.nupkg"))
        .send()
        .await
        .unwrap();
    assert!(download.status().is_success(), "winner payload was deleted");
}

#[tokio::test]
async fn concurrent_pushes_with_prune_on_push_stay_consistent() {
    let server = Arc::new(
        spawn_with(|c| {
            c.retention.enabled = true;
            c.retention.prune_on_push = true;
            c.retention.keep_latest_stable = Some(2);
        })
        .await,
    );
    let mut handles = Vec::new();
    for i in 0..8 {
        let s = server.clone();
        handles.push(tokio::spawn(async move {
            let v = format!("1.{i}.0");
            push_multipart(s.as_ref(), API_KEY, build_nupkg("Race.Prune", &v, b"x"))
                .await
                .status()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), reqwest::StatusCode::CREATED);
    }

    // The server stayed responsive; the newest version is never pruned.
    let versions: serde_json::Value = server
        .client
        .get(server.url("/v3/package/race.prune/index.json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let list: Vec<String> = versions["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(
        list.contains(&"1.7.0".to_string()),
        "newest version was pruned: {list:?}"
    );
    assert!(list.len() <= 8);
}
