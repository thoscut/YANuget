//! The upstream client against local feeds: v2 paging and hashes, v3
//! registrations, the cache, retries, credentials and the address policy.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::routing::get;
use axum::Router;
use base64::Engine;
use sha2::{Digest, Sha512};
use yanuget_internalize::config::{Secret, UpstreamConfig};
use yanuget_internalize::upstream::http::RetryPolicy;
use yanuget_internalize::upstream::{Upstream, UpstreamError};
use yanuget_pkg::NuGetVersion;

/// A minimal package whose manifest says `id` `version`.
fn nupkg(id: &str, version: &str) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(&mut buf);
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file(format!("{id}.nuspec"), opts).unwrap();
    write!(
        zip,
        r#"<?xml version="1.0"?><package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd"><metadata><id>{id}</id><version>{version}</version><authors>a</authors><description>d</description></metadata></package>"#
    )
    .unwrap();
    zip.start_file("tools/chocolateyInstall.ps1", opts).unwrap();
    zip.write_all(b"Write-Host hi").unwrap();
    zip.finish().unwrap();
    buf.into_inner()
}

fn sha512_b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
}

#[derive(Default)]
struct Feed {
    /// Bodies by request path, for the package downloads.
    packages: HashMap<String, Vec<u8>>,
    /// Requests seen, by path.
    hits: HashMap<String, usize>,
    /// `429`s still to give before answering.
    throttle: usize,
    /// The Authorization header of each request, by path.
    auth: HashMap<String, Option<String>>,
}

type Shared = Arc<Mutex<Feed>>;

fn record(state: &Shared, path: &str, headers: &HeaderMap) {
    let mut f = state.lock().unwrap();
    *f.hits.entry(path.to_string()).or_default() += 1;
    let auth = headers
        .get("authorization")
        .map(|v| v.to_str().unwrap().to_string());
    f.auth.insert(path.to_string(), auth);
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn v2_entry(base: &str, id: &str, version: &str, hash: Option<&str>, listed: bool) -> String {
    let published = if listed {
        "2025-01-01T00:00:00"
    } else {
        "1900-01-01T00:00:00"
    };
    let hash = hash
        .map(|h| format!("<d:PackageHash>{h}</d:PackageHash><d:PackageHashAlgorithm>SHA512</d:PackageHashAlgorithm>"))
        .unwrap_or_default();
    format!(
        r#"<entry><title type="text">{id}</title><content type="application/zip" src="{base}/api/v2/package/{id}/{version}"/><m:properties><d:Version>{version}</d:Version><d:Published>{published}</d:Published>{hash}<d:Dependencies>dep.a:[1.0]:</d:Dependencies></m:properties></entry>"#
    )
}

fn atom(entries: &str, next: Option<&str>) -> String {
    let next = next
        .map(|n| format!(r#"<link rel="next" href="{}"/>"#, n.replace('&', "&amp;")))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><feed xmlns="http://www.w3.org/2005/Atom" xmlns:d="http://schemas.microsoft.com/ado/2007/08/dataservices" xmlns:m="http://schemas.microsoft.com/ado/2007/08/dataservices/metadata">{entries}{next}</feed>"#
    )
}

/// A v2 feed of `git`: 1.0.0 and 2.0.0 on the first page, 2.1.0-beta and an
/// unlisted 3.0.0 on the second. `bad_hash` states a wrong hash for 2.0.0,
/// `wrong_identity` serves another package's manifest under 2.0.0.
async fn v2_feed(bad_hash: bool, wrong_identity: bool) -> (String, Shared) {
    let state: Shared = Arc::default();
    let app = Router::new()
        .route(
            "/api/v2/FindPackagesById()",
            get(
                |State(s): State<Shared>,
                 Query(q): Query<HashMap<String, String>>,
                 headers: HeaderMap| async move {
                    record(&s, "find", &headers);
                    let base = s.lock().unwrap().packages["base"].clone();
                    let base = String::from_utf8(base).unwrap();
                    {
                        let mut f = s.lock().unwrap();
                        if f.throttle > 0 {
                            f.throttle -= 1;
                            return (
                                StatusCode::TOO_MANY_REQUESTS,
                                [("retry-after", "0")],
                                String::new(),
                            )
                                .into_response();
                        }
                    }
                    assert_eq!(q.get("id").map(String::as_str), Some("'git'"));
                    let hash = |v: &str| {
                        let f = s.lock().unwrap();
                        f.packages
                            .get(&format!("hash/{v}"))
                            .map(|h| String::from_utf8(h.clone()).unwrap())
                    };
                    let body = if q.contains_key("page2") {
                        atom(
                            &(v2_entry(
                                &base,
                                "git",
                                "2.1.0-beta",
                                hash("2.1.0-beta").as_deref(),
                                true,
                            ) + &v2_entry(&base, "git", "3.0.0", None, false)),
                            None,
                        )
                    } else {
                        atom(
                            &(v2_entry(&base, "git", "1.0.0", hash("1.0.0").as_deref(), true)
                                + &v2_entry(&base, "Git", "2.0.0", hash("2.0.0").as_deref(), true)),
                            Some(&format!(
                                "{base}/api/v2/FindPackagesById()?id='git'&page2=1"
                            )),
                        )
                    };
                    ([("content-type", "application/atom+xml")], body).into_response()
                },
            ),
        )
        .route(
            "/api/v2/package/{id}/{version}",
            get(
                |State(s): State<Shared>,
                 Path((id, version)): Path<(String, String)>,
                 headers: HeaderMap| async move {
                    let key = format!("{id}/{version}");
                    record(&s, &key, &headers);
                    match s.lock().unwrap().packages.get(&key) {
                        Some(body) => body.clone().into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            ),
        )
        .with_state(state.clone());
    let base = serve(app).await;
    {
        let mut f = state.lock().unwrap();
        f.packages.insert("base".into(), base.clone().into_bytes());
        for v in ["1.0.0", "2.0.0", "2.1.0-beta"] {
            let body = if wrong_identity && v == "2.0.0" {
                nupkg("evil", "2.0.0")
            } else {
                nupkg("git", v)
            };
            let hash = if bad_hash && v == "2.0.0" {
                sha512_b64(b"something else")
            } else {
                sha512_b64(&body)
            };
            f.packages.insert(format!("hash/{v}"), hash.into_bytes());
            f.packages.insert(format!("git/{v}"), body.clone());
            f.packages.insert(format!("Git/{v}"), body);
        }
    }
    (base, state)
}

fn config(url: String) -> UpstreamConfig {
    UpstreamConfig {
        url,
        allow_private: true,
        timeout_secs: 5,
        ..UpstreamConfig::default()
    }
}

fn client(config: &UpstreamConfig, cache: &std::path::Path) -> Upstream {
    let mut u = Upstream::new(config, cache).unwrap();
    u.http_mut().set_retry(RetryPolicy {
        attempts: 3,
        base: std::time::Duration::from_millis(10),
        max_wait: std::time::Duration::from_millis(50),
    });
    u
}

#[tokio::test]
async fn v2_versions_resolve_and_download_into_the_cache() {
    let (base, state) = v2_feed(false, false).await;
    let cache = tempfile::tempdir().unwrap();
    let up = client(&config(format!("{base}/api/v2/")), cache.path());

    let versions = up.versions("git").await.unwrap();
    let names: Vec<String> = versions.iter().map(|v| v.version.normalized()).collect();
    assert_eq!(names, ["1.0.0", "2.0.0", "2.1.0-beta", "3.0.0"]);
    assert_eq!(versions[0].dependencies[0].id, "dep.a");

    // The newest listed stable version, as choco picks it.
    let info = up.resolve("git", None).await.unwrap();
    assert_eq!(info.version.normalized(), "2.0.0");
    let pinned = up
        .resolve("git", Some(&NuGetVersion::parse("2.1.0-beta").unwrap()))
        .await
        .unwrap();
    assert_eq!(pinned.version.normalized(), "2.1.0-beta");
    assert!(matches!(
        up.resolve("git", Some(&NuGetVersion::parse("9.9.9").unwrap()))
            .await,
        Err(UpstreamError::NotFound(_))
    ));

    let first = up.fetch(&info).await.unwrap();
    assert!(first.verified);
    assert_eq!(
        first.sha512,
        hex::encode(Sha512::digest(std::fs::read(&first.path).unwrap()))
    );
    assert!(first
        .path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("sha512-"));
    // The second fetch is served from the cache.
    let second = up.fetch(&info).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(state.lock().unwrap().hits["Git/2.0.0"], 1);

    // A corrupted cache entry is noticed and fetched again.
    std::fs::write(&first.path, b"corrupt").unwrap();
    let third = up.fetch(&info).await.unwrap();
    assert_eq!(third.sha512, first.sha512);
    assert_eq!(state.lock().unwrap().hits["Git/2.0.0"], 2);
}

#[tokio::test]
async fn a_wrong_hash_or_identity_never_enters_the_cache() {
    for (bad_hash, wrong_identity, expect) in
        [(true, false, "hash"), (false, true, "manifest says evil")]
    {
        let (base, _) = v2_feed(bad_hash, wrong_identity).await;
        let cache = tempfile::tempdir().unwrap();
        let up = client(&config(format!("{base}/api/v2/")), cache.path());
        let info = up.resolve("git", None).await.unwrap();
        match up.fetch(&info).await {
            Err(UpstreamError::Integrity(m)) => assert!(m.contains(expect), "{m}"),
            other => panic!("{other:?}"),
        }
        let left: Vec<_> = std::fs::read_dir(cache.path().join("packages"))
            .unwrap()
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
}

#[tokio::test]
async fn too_many_requests_is_waited_out_and_size_is_capped() {
    let (base, state) = v2_feed(false, false).await;
    state.lock().unwrap().throttle = 2;
    let cache = tempfile::tempdir().unwrap();
    let mut cfg = config(format!("{base}/api/v2/"));
    let up = client(&cfg, cache.path());
    let info = up.resolve("git", None).await.unwrap();
    // Two 429s and the answer, then the second page.
    assert_eq!(state.lock().unwrap().hits["find"], 4);

    // Three 429s in a row exhaust three attempts.
    state.lock().unwrap().throttle = 3;
    let e = up.versions("git").await.unwrap_err();
    assert!(e.to_string().contains("429"), "{e}");
    assert!(up.http().is_down());

    cfg.max_package_bytes = 100;
    let small = client(&cfg, cache.path());
    match small.fetch(&info).await {
        Err(UpstreamError::Integrity(m)) => assert!(m.contains("max_package_bytes"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn private_addresses_are_refused_unless_allowed() {
    let (base, state) = v2_feed(false, false).await;
    let cache = tempfile::tempdir().unwrap();
    let cfg = UpstreamConfig {
        allow_private: false,
        ..config(format!("{base}/api/v2/"))
    };
    let up = client(&cfg, cache.path());
    match up.versions("git").await {
        Err(UpstreamError::Refused(m)) => assert!(m.contains("allow_private"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(state.lock().unwrap().hits.is_empty());
}

/// A v3 feed: a registration whose index points at a remote page, a flat
/// container for the download, and no hashes. `git` 1.0.0 is listed, 2.0.0
/// is not.
async fn v3_feed() -> (String, Shared) {
    let state: Shared = Arc::default();
    let app = Router::new()
        .route(
            "/v3/index.json",
            get(|State(s): State<Shared>| async move {
                let base = String::from_utf8(s.lock().unwrap().packages["base"].clone()).unwrap();
                axum::Json(serde_json::json!({"version": "3.0.0", "resources": [
                    {"@id": format!("{base}/v3/registration/"), "@type": "RegistrationsBaseUrl/3.6.0"},
                    {"@id": format!("{base}/v3/flat/"), "@type": "PackageBaseAddress/3.0.0"},
                ]}))
            }),
        )
        .route(
            "/v3/registration/git/index.json",
            get(|State(s): State<Shared>| async move {
                let base = String::from_utf8(s.lock().unwrap().packages["base"].clone()).unwrap();
                axum::Json(serde_json::json!({"items": [
                    {"@id": format!("{base}/v3/registration/git/page.json")}
                ]}))
            }),
        )
        .route(
            "/v3/registration/git/page.json",
            get(|| async {
                axum::Json(serde_json::json!({"items": [
                    {"catalogEntry": {"id": "git", "version": "1.0.0"}},
                    {"catalogEntry": {"id": "git", "version": "2.0.0", "listed": false}},
                ]}))
            }),
        )
        .route(
            "/v3/flat/{id}/{version}/{file}",
            get(
                |State(s): State<Shared>, Path((id, version, _)): Path<(String, String, String)>, headers: HeaderMap| async move {
                    let key = format!("{id}/{version}");
                    record(&s, &key, &headers);
                    match s.lock().unwrap().packages.get(&key) {
                        Some(body) => body.clone().into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            ),
        )
        .with_state(state.clone());
    let base = serve(app).await;
    {
        let mut f = state.lock().unwrap();
        f.packages.insert("base".into(), base.clone().into_bytes());
        f.packages.insert("git/1.0.0".into(), nupkg("git", "1.0.0"));
        f.packages.insert("git/2.0.0".into(), nupkg("git", "2.0.0"));
    }
    (base, state)
}

#[tokio::test]
async fn v3_registrations_resolve_and_unverified_downloads_are_said_to_be() {
    let (base, state) = v3_feed().await;
    let cache = tempfile::tempdir().unwrap();
    let up = client(&config(format!("{base}/v3/index.json")), cache.path());
    let info = up.resolve("Git", None).await.unwrap();
    assert_eq!(info.version.normalized(), "1.0.0");
    let got = up.fetch(&info).await.unwrap();
    assert!(!got.verified);
    assert!(got
        .path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("unverified-"));
    up.fetch(&info).await.unwrap();
    assert_eq!(state.lock().unwrap().hits["git/1.0.0"], 1);
    assert!(up.versions("nothing").await.unwrap().is_empty());
    up.probe().await.unwrap();

    // A v2 base that answers 404 is no feed.
    let (v2, _) = v2_feed(false, false).await;
    let up = client(&config(format!("{v2}/nothing/")), cache.path());
    assert!(matches!(up.probe().await, Err(UpstreamError::NotFound(_))));
}

#[tokio::test]
async fn credentials_go_to_the_upstream_only() {
    // The package sits on a second server (another origin), reached through
    // a redirect from a third.
    let (cdn, cdn_state) = v2_feed(false, false).await;
    let state: Shared = Arc::default();
    let app = Router::new()
        .route(
            "/api/v2/FindPackagesById()",
            get(|State(s): State<Shared>, headers: HeaderMap| async move {
                record(&s, "find", &headers);
                let base = String::from_utf8(s.lock().unwrap().packages["base"].clone()).unwrap();
                let entry = v2_entry(&base, "git", "1.0.0", None, true).replace(
                    &format!("{base}/api/v2/package/git/1.0.0"),
                    &format!("{base}/dl"),
                );
                (
                    [("content-type", "application/atom+xml")],
                    atom(&entry, None),
                )
            }),
        )
        .route(
            "/dl",
            get(|State(s): State<Shared>, headers: HeaderMap| async move {
                record(&s, "dl", &headers);
                let cdn = String::from_utf8(s.lock().unwrap().packages["cdn"].clone()).unwrap();
                Redirect::temporary(&format!("{cdn}/api/v2/package/git/1.0.0"))
            }),
        )
        .with_state(state.clone());
    let base = serve(app).await;
    {
        let mut f = state.lock().unwrap();
        f.packages.insert("base".into(), base.clone().into_bytes());
        f.packages.insert("cdn".into(), cdn.into_bytes());
    }

    let cache = tempfile::tempdir().unwrap();
    let cfg = UpstreamConfig {
        username: Some("me".into()),
        password: Some(Secret::new("pw")),
        ..config(format!("{base}/api/v2/"))
    };
    let up = client(&cfg, cache.path());
    let info = up.resolve("git", None).await.unwrap();
    up.fetch(&info).await.unwrap();

    let basic = Some("Basic bWU6cHc=".to_string());
    assert_eq!(state.lock().unwrap().auth["find"], basic);
    assert_eq!(state.lock().unwrap().auth["dl"], basic);
    // The hop to the other origin went without them.
    assert_eq!(cdn_state.lock().unwrap().auth["git/1.0.0"], None);
}
