//! Read-through mirroring and migration against a fake upstream V3 feed on
//! loopback: what the mirror fetches, what it sends where, and what it refuses.

use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use yanuget::config::{
    Config, FeedConfig, LicensePolicyConfig, MirrorAuthConfig, MirrorConfig, PolicyAction,
};
use yanuget::database::{Membership, PackageDatabase, SqliteDatabase};
use yanuget::mirror::MirrorOptions;
use yanuget::storage::FilesystemStorage;
use yanuget::version::NuGetVersion;
use yanuget::web::{self, AppState, FeedMeta};
use zip::write::SimpleFileOptions;

/// Build a minimal but valid `.nupkg` in memory.
fn build_nupkg(id: &str, version: &str) -> Vec<u8> {
    build_nupkg_with(id, version, "")
}

/// A `.nupkg` whose manifest carries `extra` inside `<metadata>`.
fn build_nupkg_with(id: &str, version: &str, extra: &str) -> Vec<u8> {
    let nuspec = format!(
        r#"<?xml version="1.0"?>
<package xmlns="http://schemas.microsoft.com/packaging/2013/05/nuspec.xsd">
  <metadata>
    <id>{id}</id>
    <version>{version}</version>
    <authors>Upstream</authors>
    <description>A package served by the fake upstream.</description>
    {extra}
  </metadata>
</package>"#
    );
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut cursor);
        let opts =
            SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file(format!("{id}.nuspec"), opts).unwrap();
        zip.write_all(nuspec.as_bytes()).unwrap();
        zip.start_file("lib/net8.0/Lib.dll", opts).unwrap();
        zip.write_all(b"not really a dll").unwrap();
        zip.finish().unwrap();
    }
    cursor.into_inner()
}

/// One request the fake upstream received.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    authorization: Option<String>,
    feed_key: Option<String>,
}

impl Seen {
    fn carried_credentials(&self) -> bool {
        self.authorization.is_some() || self.feed_key.is_some()
    }
}

/// What the fake upstream serves and how it misbehaves.
#[derive(Default)]
struct Behaviour {
    /// Lower-cased id → (version as listed, `.nupkg` bytes served for it).
    packages: BTreeMap<String, Vec<(String, Vec<u8>)>>,
    /// Advertise this `PackageBaseAddress` instead of the upstream's own.
    package_base: Option<String>,
    /// Answer every `.nupkg` request with a 302 to this base plus the path.
    redirect_downloads: Option<String>,
    /// Answer version lists with a 500, as an upstream in an outage does.
    fail_listing: bool,
    /// Hold every `.nupkg` response this long before answering.
    download_delay: Option<std::time::Duration>,
    /// Send every `.nupkg` body in four pieces with this pause between them:
    /// steady, but slow.
    trickle: Option<std::time::Duration>,
    /// Search returns at most this many ids in all, as capped servers do.
    search_cap: Option<usize>,
    /// Search answers with a 500.
    search_fails: bool,
    /// Advertise a `Catalog/3.0.0` with one page per package.
    catalog: bool,
    /// The catalog page for the package at this index answers with a 500.
    broken_catalog_page: Option<usize>,
    /// Advertise a registration hive, served gzipped as nuget.org does, where
    /// these (lower id, version) pairs are unlisted.
    registration: bool,
    unlisted: std::collections::BTreeSet<(String, String)>,
}

struct UpstreamState {
    base: String,
    behaviour: Mutex<Behaviour>,
    seen: Mutex<Vec<Seen>>,
}

/// A fake upstream V3 feed on loopback.
#[derive(Clone)]
struct Upstream {
    state: Arc<UpstreamState>,
}

impl Upstream {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(UpstreamState {
            base,
            behaviour: Mutex::default(),
            seen: Mutex::default(),
        });
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state }
    }

    /// The same upstream over HTTPS as `https://localhost:{port}`, with a
    /// certificate from a private CA. Returns the CA's PEM, which is what an
    /// operator would put in `ca_cert_path`.
    async fn start_tls() -> (Self, String) {
        // Whichever test gets here first installs the provider.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["localhost".to_string()])
            .unwrap()
            .self_signed(&key)
            .unwrap();
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
            cert.pem().into_bytes(),
            key.serialize_pem().into_bytes(),
        )
        .await
        .unwrap();
        let listener =
            std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!(
            "https://localhost:{}",
            listener.local_addr().unwrap().port()
        );
        let state = Arc::new(UpstreamState {
            base,
            behaviour: Mutex::default(),
            seen: Mutex::default(),
        });
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(state.clone());
        tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)
                .serve(app.into_make_service())
                .await
                .unwrap();
        });
        (Self { state }, cert.pem())
    }

    fn base(&self) -> &str {
        &self.state.base
    }

    fn index_url(&self) -> String {
        format!("{}/v3/index.json", self.base())
    }

    fn flat_url(&self) -> String {
        format!("{}/flat/", self.base())
    }

    /// Serve `bytes` as `id` `version`.
    fn serve(&self, id: &str, version: &str, bytes: Vec<u8>) {
        self.state
            .behaviour
            .lock()
            .unwrap()
            .packages
            .entry(id.to_lowercase())
            .or_default()
            .push((version.to_string(), bytes));
    }

    fn publish(&self, id: &str, version: &str) {
        self.serve(id, version, build_nupkg(id, version));
    }

    fn behave(&self, f: impl FnOnce(&mut Behaviour)) {
        f(&mut self.state.behaviour.lock().unwrap());
    }

    fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().unwrap().clone()
    }

    /// How many requests went to a path ending in `suffix`.
    fn hits(&self, suffix: &str) -> usize {
        self.seen()
            .iter()
            .filter(|s| s.path.ends_with(suffix))
            .count()
    }
}

async fn handle(State(state): State<Arc<UpstreamState>>, uri: Uri, headers: HeaderMap) -> Response {
    let path = uri.path().to_string();
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    state.seen.lock().unwrap().push(Seen {
        path: path.clone(),
        authorization: header("authorization"),
        feed_key: header("x-feed-key"),
    });
    let (delay, trickle) = if path.ends_with(".nupkg") {
        let b = state.behaviour.lock().unwrap();
        (b.download_delay, b.trickle)
    } else {
        (None, None)
    };
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    let response = respond(&state, &uri);
    match trickle {
        Some(pause) if response.status().is_success() => {
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let len = body.len();
            let pieces: Vec<axum::body::Bytes> = body
                .chunks(len.div_ceil(4))
                .map(axum::body::Bytes::copy_from_slice)
                .collect();
            let stream = futures::stream::iter(pieces).then(move |piece| async move {
                tokio::time::sleep(pause).await;
                Ok::<_, std::io::Error>(piece)
            });
            Response::builder()
                .header("content-length", len)
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }
        _ => response,
    }
}

fn respond(state: &UpstreamState, uri: &Uri) -> Response {
    let path = uri.path();
    let behaviour = state.behaviour.lock().unwrap();
    let base = &state.base;

    if path == "/v3/index.json" {
        let package_base = behaviour
            .package_base
            .clone()
            .unwrap_or_else(|| format!("{base}/flat/"));
        let mut resources = vec![
            serde_json::json!({"@id": package_base, "@type": "PackageBaseAddress/3.0.0"}),
            serde_json::json!({"@id": format!("{base}/query"), "@type": "SearchQueryService"}),
        ];
        if behaviour.catalog {
            resources.push(serde_json::json!({
                "@id": format!("{base}/catalog/index.json"),
                "@type": "Catalog/3.0.0"
            }));
        }
        if behaviour.registration {
            resources.push(serde_json::json!({
                "@id": format!("{base}/registration/"),
                "@type": "RegistrationsBaseUrl/3.6.0"
            }));
        }
        return axum::Json(serde_json::json!({"version": "3.0.0", "resources": resources}))
            .into_response();
    }
    if path == "/query" && behaviour.search_fails {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    if path == "/catalog/index.json" {
        let pages: Vec<serde_json::Value> = (0..behaviour.packages.len())
            .map(|i| serde_json::json!({"@id": format!("{base}/catalog/page{i}.json")}))
            .collect();
        return axum::Json(serde_json::json!({ "items": pages })).into_response();
    }
    if let Some(page) = path
        .strip_prefix("/catalog/page")
        .and_then(|p| p.strip_suffix(".json"))
        .and_then(|p| p.parse::<usize>().ok())
    {
        if behaviour.broken_catalog_page == Some(page) {
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        let Some(id) = behaviour.packages.keys().nth(page) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        return axum::Json(serde_json::json!({
            "items": [{"nuget:id": id, "@type": "nuget:PackageDetails"}]
        }))
        .into_response();
    }
    if let Some(id) = path
        .strip_prefix("/registration/")
        .and_then(|p| p.strip_suffix("/index.json"))
    {
        let Some(versions) = behaviour.packages.get(id) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        let leaves: Vec<serde_json::Value> = versions
            .iter()
            .map(|(v, _)| {
                let listed = !behaviour
                    .unlisted
                    .contains(&(id.to_string(), v.to_lowercase()));
                serde_json::json!({"catalogEntry": {"id": id, "version": v, "listed": listed}})
            })
            .collect();
        let json = serde_json::to_vec(&serde_json::json!({
            "count": 1,
            "items": [{"@id": format!("{base}/registration/{id}/index.json#page"), "items": leaves}]
        }))
        .unwrap();
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&json).unwrap();
        return (
            [
                ("content-type", "application/json"),
                ("content-encoding", "gzip"),
            ],
            gz.finish().unwrap(),
        )
            .into_response();
    }
    if path == "/query" {
        let skip = uri
            .query()
            .unwrap_or("")
            .split('&')
            .find_map(|kv| kv.strip_prefix("skip="))
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        let cap = behaviour.search_cap.unwrap_or(usize::MAX);
        let data: Vec<serde_json::Value> = behaviour
            .packages
            .keys()
            .take(cap)
            .skip(skip)
            .map(|id| serde_json::json!({"id": id, "version": "1.0.0"}))
            .collect();
        return axum::Json(serde_json::json!({"totalHits": data.len(), "data": data}))
            .into_response();
    }
    let Some(rest) = path
        .strip_prefix("/flat/")
        .or_else(|| path.strip_prefix("/blobs/"))
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [_, "index.json"] if behaviour.fail_listing => {
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
        [id, "index.json"] => match behaviour.packages.get(*id) {
            Some(versions) => {
                let listed: Vec<String> = versions.iter().map(|(v, _)| v.to_lowercase()).collect();
                axum::Json(serde_json::json!({ "versions": listed })).into_response()
            }
            None => StatusCode::NOT_FOUND.into_response(),
        },
        [id, version, _file] => {
            if path.starts_with("/flat/") {
                if let Some(target) = &behaviour.redirect_downloads {
                    return (
                        StatusCode::FOUND,
                        [(
                            "location",
                            format!("{target}{id}/{version}/{id}.{version}.nupkg"),
                        )],
                    )
                        .into_response();
                }
            }
            let bytes = behaviour.packages.get(*id).and_then(|versions| {
                versions
                    .iter()
                    .find(|(v, _)| v.eq_ignore_ascii_case(version))
                    .map(|(_, b)| b.clone())
            });
            match bytes {
                Some(bytes) => bytes.into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A mirror config for `upstream`, which is on loopback.
fn mirror_config(upstream: &Upstream) -> MirrorConfig {
    MirrorConfig {
        enabled: true,
        upstream: upstream.index_url(),
        timeout_secs: 5,
        allow_private_upstream: true,
        ..Default::default()
    }
}

/// Credentials of every kind the mirror can send.
fn credentials() -> MirrorAuthConfig {
    let mut headers = BTreeMap::new();
    headers.insert("X-Feed-Key".to_string(), "feed-secret".to_string());
    MirrorAuthConfig {
        token: Some("bearer-secret".into()),
        headers,
        ..Default::default()
    }
}

const API_KEY: &str = "push-key";

/// A YANuget server with one feed, `mirror`, reading through to an upstream.
struct Server {
    base: String,
    client: reqwest::Client,
    _dir: tempfile::TempDir,
}

impl Server {
    /// `path` under the mirror feed.
    fn url(&self, path: &str) -> String {
        format!("{}/mirror{path}", self.base)
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client.get(self.url(path)).send().await.unwrap()
    }

    /// The versions the flat container lists, or `None` on a 404.
    async fn versions(&self, id: &str) -> Option<Vec<String>> {
        let resp = self.get(&format!("/v3/package/{id}/index.json")).await;
        if resp.status() == StatusCode::NOT_FOUND {
            return None;
        }
        assert!(resp.status().is_success(), "{}", resp.status());
        let doc: serde_json::Value = resp.json().await.unwrap();
        Some(
            doc["versions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect(),
        )
    }

    async fn download(&self, id: &str, version: &str) -> reqwest::Response {
        self.get(&format!("/v3/package/{id}/{version}/{id}.{version}.nupkg"))
            .await
    }
}

async fn spawn_mirror(upstream: &Upstream, customize: impl FnOnce(&mut FeedConfig)) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let mut feed = FeedConfig {
        name: "mirror".into(),
        mirror: mirror_config(upstream),
        ..Default::default()
    };
    customize(&mut feed);
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        api_key: Some(API_KEY.into()),
        tls_enabled: false,
        feeds: vec![feed],
        ..Config::default()
    };
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
    Server {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
        _dir: dir,
    }
}

// ---------------------------------------------------------------------------
// Deletes stick
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_version_deleted_from_a_mirror_feed_is_not_fetched_back() {
    // Deleting drops the membership, which is all the mirror used to check:
    // a version removed as malicious came straight back on the next read.
    let upstream = Upstream::start().await;
    upstream.publish("Gone.Pkg", "1.0.0");
    upstream.publish("Gone.Pkg", "2.0.0");
    let server = spawn_mirror(&upstream, |f| f.hard_delete_enabled = Some(true)).await;
    assert_eq!(
        server.versions("gone.pkg").await.unwrap(),
        ["1.0.0", "2.0.0"]
    );

    let deleted = server
        .client
        .delete(server.url("/api/v2/package/gone.pkg/1.0.0"))
        .header("X-NuGet-ApiKey", API_KEY)
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    // A client asking for it gets a 404, not a fresh copy from upstream.
    let fetched = upstream.seen().len();
    assert_eq!(
        server.download("gone.pkg", "1.0.0").await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(server.versions("gone.pkg").await.unwrap(), ["2.0.0"]);
    let downloads_of_deleted = upstream.seen()[fetched..]
        .iter()
        .filter(|s| s.path.ends_with("gone.pkg.1.0.0.nupkg"))
        .count();
    assert_eq!(downloads_of_deleted, 0);

    // Pushing it back is how an operator undoes the delete.
    let part = reqwest::multipart::Part::bytes(build_nupkg("Gone.Pkg", "1.0.0"))
        .file_name("package.nupkg");
    let pushed = server
        .client
        .put(server.url("/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .multipart(reqwest::multipart::Form::new().part("package", part))
        .send()
        .await
        .unwrap();
    assert_eq!(pushed.status(), StatusCode::CREATED);
    assert!(server
        .download("gone.pkg", "1.0.0")
        .await
        .status()
        .is_success());
}

// ---------------------------------------------------------------------------
// Credentials and redirects
// ---------------------------------------------------------------------------

#[tokio::test]
async fn credentials_are_not_sent_to_a_host_the_upstream_names() {
    // The service index is the operator's; the PackageBaseAddress inside it
    // is the upstream's choice. Pointing it at another origin must not hand
    // that origin the feed's token or its custom key header.
    let upstream = Upstream::start().await;
    let elsewhere = Upstream::start().await;
    elsewhere.publish("Foreign.Pkg", "1.0.0");
    upstream.behave(|b| b.package_base = Some(elsewhere.flat_url()));

    let mut config = mirror_config(&upstream);
    config.auth = credentials();
    let client = yanuget::mirror::MirrorClient::from_config(&config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    client
        .download_nupkg("foreign.pkg", "1.0.0", &dir.path().join("a.nupkg"))
        .await
        .unwrap();

    let index = upstream.seen();
    assert_eq!(index.len(), 1, "{index:?}");
    assert_eq!(
        index[0].authorization.as_deref(),
        Some("Bearer bearer-secret")
    );
    assert_eq!(index[0].feed_key.as_deref(), Some("feed-secret"));
    let foreign = elsewhere.seen();
    assert!(!foreign.is_empty());
    assert!(
        foreign.iter().all(|s| !s.carried_credentials()),
        "credentials leaked to another origin: {foreign:?}"
    );
}

#[tokio::test]
async fn a_download_redirected_to_another_host_is_followed_without_credentials() {
    // Feeds hand package downloads off to a blob store or CDN. The redirect is
    // followed — the old policy stopped and stored the 302's body as the
    // package — but the credentials stay behind.
    let upstream = Upstream::start().await;
    let cdn = Upstream::start().await;
    upstream.publish("Cdn.Pkg", "1.0.0");
    cdn.publish("Cdn.Pkg", "1.0.0");
    upstream.behave(|b| b.redirect_downloads = Some(format!("{}/blobs/", cdn.base())));

    let mut config = mirror_config(&upstream);
    config.auth = credentials();
    let client = yanuget::mirror::MirrorClient::from_config(&config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let summary = client
        .download_nupkg("cdn.pkg", "1.0.0", &dir.path().join("a.nupkg"))
        .await
        .unwrap();
    assert_eq!(summary.size, build_nupkg("Cdn.Pkg", "1.0.0").len() as u64);

    let origin: Vec<Seen> = upstream.seen();
    assert!(origin.iter().all(Seen::carried_credentials), "{origin:?}");
    let blobs = cdn.seen();
    assert_eq!(blobs.len(), 1, "{blobs:?}");
    assert!(!blobs[0].carried_credentials(), "{blobs:?}");
}

#[tokio::test]
async fn a_same_origin_redirect_keeps_the_credentials() {
    let upstream = Upstream::start().await;
    upstream.publish("Same.Pkg", "1.0.0");
    let blobs = format!("{}/blobs/", upstream.base());
    upstream.behave(|b| b.redirect_downloads = Some(blobs));

    let mut config = mirror_config(&upstream);
    config.auth = credentials();
    let client = yanuget::mirror::MirrorClient::from_config(&config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    client
        .download_nupkg("same.pkg", "1.0.0", &dir.path().join("a.nupkg"))
        .await
        .unwrap();
    let seen = upstream.seen();
    assert!(seen.iter().any(|s| s.path.starts_with("/blobs/")));
    assert!(seen.iter().all(Seen::carried_credentials), "{seen:?}");
}

#[tokio::test]
async fn upstream_errors_do_not_echo_credentials_in_the_url() {
    // reqwest puts the request URL in its errors, userinfo and query
    // included, and the error ends up in the log.
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // nothing listens there now
    let config = MirrorConfig {
        enabled: true,
        upstream: format!("http://user:hunter2@{addr}/v3/index.json?token=hunter3"),
        timeout_secs: 2,
        allow_private_upstream: true,
        ..Default::default()
    };
    let client = yanuget::mirror::MirrorClient::from_config(&config).unwrap();
    let err = client.upstream_versions("x").await.unwrap_err().to_string();
    assert!(!err.contains("hunter2"), "{err}");
    assert!(!err.contains("hunter3"), "{err}");
    assert!(err.contains(&addr.to_string()), "{err}");
}

// ---------------------------------------------------------------------------
// TLS trust
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_upstream_behind_a_private_ca_is_trusted_through_ca_cert_path() {
    let (upstream, ca_pem) = Upstream::start_tls().await;
    upstream.publish("Tls.Pkg", "1.0.0");

    // Neither the bundled roots nor the system store know this CA.
    let untrusted = yanuget::mirror::MirrorClient::from_config(&mirror_config(&upstream)).unwrap();
    let err = untrusted.upstream_versions("tls.pkg").await.unwrap_err();
    assert!(err.to_string().contains("certificate"), "{err}");

    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("internal-ca.pem");
    std::fs::write(&ca, ca_pem).unwrap();
    let mut config = mirror_config(&upstream);
    config.ca_cert_path = Some(ca);
    let trusted = yanuget::mirror::MirrorClient::from_config(&config).unwrap();
    assert_eq!(
        trusted.upstream_versions("tls.pkg").await.unwrap(),
        vec!["1.0.0".to_string()]
    );
}

#[tokio::test]
async fn an_unreadable_ca_file_is_a_configuration_error() {
    let upstream = Upstream::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut config = mirror_config(&upstream);
    config.ca_cert_path = Some(dir.path().join("missing.pem"));
    assert!(yanuget::mirror::MirrorClient::try_from_config(&config).is_err());

    let empty = dir.path().join("empty.pem");
    std::fs::write(&empty, "not a certificate").unwrap();
    config.ca_cert_path = Some(empty);
    assert!(yanuget::mirror::MirrorClient::try_from_config(&config).is_err());
}

// ---------------------------------------------------------------------------
// Disk reserve
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_download_that_would_eat_the_disk_reserve_is_refused() {
    // Pushes keep `min_free_disk_bytes` free; an anonymous read that starts a
    // mirror fetch has to as well.
    let upstream = Upstream::start().await;
    upstream.publish("Big.Pkg", "1.0.0");
    let mut client = yanuget::mirror::MirrorClient::from_config(&mirror_config(&upstream)).unwrap();
    client.set_min_free_disk_bytes(u64::MAX / 2);
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("big.nupkg");
    let err = client
        .download_nupkg("big.pkg", "1.0.0", &dest)
        .await
        .unwrap_err();
    assert!(
        matches!(err, yanuget::Error::InsufficientStorage(_)),
        "{err}"
    );
    assert!(!dest.exists(), "nothing may be written");

    // A reserve the volume can meet lets it through.
    client.set_min_free_disk_bytes(1);
    client
        .download_nupkg("big.pkg", "1.0.0", &dest)
        .await
        .unwrap();
}

#[tokio::test]
async fn migrate_holds_downloads_to_the_disk_reserve() {
    let upstream = Upstream::start().await;
    upstream.publish("Big.Pkg", "1.0.0");
    let dir = tempfile::tempdir().unwrap();
    let storage = FilesystemStorage::new(dir.path().join("packages"))
        .await
        .unwrap();
    let db = SqliteDatabase::in_memory().await.unwrap();
    let temp = dir.path().join("packages").join(".migrate");
    std::fs::create_dir_all(&temp).unwrap();
    let feeds = Config::default().resolved_feeds().unwrap();
    let summary = yanuget::migrate::run(
        &storage,
        &db,
        &feeds[0],
        &temp,
        mirror_config(&upstream),
        yanuget::migrate::MigrateOptions {
            quiet: true,
            min_free_disk_bytes: u64::MAX / 2,
            ..Default::default()
        },
        indicatif::ProgressDrawTarget::hidden(),
    )
    .await
    .unwrap();
    assert_eq!((summary.imported, summary.failed), (0, 1));
    assert!(
        summary.failures[0].error.contains("free"),
        "{:?}",
        summary.failures
    );
}

// ---------------------------------------------------------------------------
// Read-through
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_package_is_read_through_once_and_then_served_locally() {
    let upstream = Upstream::start().await;
    upstream.publish("Happy.Pkg", "1.0.0");
    upstream.publish("Happy.Pkg", "2.0.0-beta.1");
    let server = spawn_mirror(&upstream, |_| {}).await;

    assert_eq!(
        server.versions("happy.pkg").await.unwrap(),
        ["1.0.0", "2.0.0-beta.1"]
    );
    let registration = server.get("/v3/registration/happy.pkg/index.json").await;
    assert!(registration.status().is_success());

    let body = server.download("happy.pkg", "1.0.0").await;
    assert!(body.status().is_success());
    assert_eq!(
        body.bytes().await.unwrap().as_ref(),
        build_nupkg("Happy.Pkg", "1.0.0").as_slice()
    );
    // Served from the local copy: each version was downloaded once.
    server.download("happy.pkg", "1.0.0").await;
    assert_eq!(upstream.hits("happy.pkg.1.0.0.nupkg"), 1);
    assert_eq!(upstream.hits("/flat/happy.pkg/index.json"), 1);
}

#[tokio::test]
async fn a_pinned_older_version_is_fetched_outside_the_newest_n() {
    // Only the newest versions are fetched on a listing; a client restoring
    // an older one used to get a 404 forever.
    let upstream = Upstream::start().await;
    for v in ["1.0.0", "2.0.0", "3.0.0", "4.0.0"] {
        upstream.publish("Pinned.Pkg", v);
    }
    let server = spawn_mirror(&upstream, |f| {
        f.mirror.max_versions_per_package = Some(2);
    })
    .await;
    assert_eq!(
        server.versions("pinned.pkg").await.unwrap(),
        ["3.0.0", "4.0.0"]
    );
    let old = server.download("pinned.pkg", "1.0.0").await;
    assert!(old.status().is_success(), "{}", old.status());
    assert_eq!(upstream.hits("pinned.pkg.2.0.0.nupkg"), 0);
}

#[tokio::test]
async fn new_upstream_releases_appear_once_the_list_is_stale() {
    let upstream = Upstream::start().await;
    upstream.publish("Fresh.Pkg", "1.0.0");

    // Within `refresh_secs` the local list stands: no upstream request.
    let cached = spawn_mirror(&upstream, |_| {}).await;
    assert_eq!(cached.versions("fresh.pkg").await.unwrap(), ["1.0.0"]);
    upstream.publish("Fresh.Pkg", "2.0.0");
    assert_eq!(cached.versions("fresh.pkg").await.unwrap(), ["1.0.0"]);
    assert_eq!(upstream.hits("/flat/fresh.pkg/index.json"), 1);

    // Once it is stale, a read re-lists in the background, and the new
    // release shows up without anyone deleting anything.
    let refreshing = spawn_mirror(&upstream, |f| f.mirror.refresh_secs = 0).await;
    assert_eq!(
        refreshing.versions("fresh.pkg").await.unwrap(),
        ["1.0.0", "2.0.0"]
    );
    upstream.publish("Fresh.Pkg", "3.0.0");
    let mut seen = Vec::new();
    for _ in 0..100 {
        seen = refreshing.versions("fresh.pkg").await.unwrap();
        if seen.len() == 3 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(seen, ["1.0.0", "2.0.0", "3.0.0"]);
}

#[tokio::test]
async fn an_id_the_upstream_lacks_is_not_asked_for_on_every_read() {
    let upstream = Upstream::start().await;
    let server = spawn_mirror(&upstream, |_| {}).await;
    for _ in 0..3 {
        assert!(server.versions("no.such.pkg").await.is_none());
    }
    assert_eq!(upstream.hits("/flat/no.such.pkg/index.json"), 1);
}

#[tokio::test]
async fn a_failing_upstream_is_backed_off() {
    let upstream = Upstream::start().await;
    upstream.publish("Down.Pkg", "1.0.0");
    upstream.behave(|b| b.fail_listing = true);
    let server = spawn_mirror(&upstream, |_| {}).await;
    for _ in 0..3 {
        assert!(server.versions("down.pkg").await.is_none());
    }
    assert_eq!(upstream.hits("/flat/down.pkg/index.json"), 1);
}

#[tokio::test]
async fn concurrent_misses_wait_for_one_fetch_instead_of_failing() {
    // Two restores of a package being fetched used to give the loser an
    // immediate 404.
    let upstream = Upstream::start().await;
    upstream.publish("Busy.Pkg", "1.0.0");
    upstream.behave(|b| b.download_delay = Some(std::time::Duration::from_millis(400)));
    let server = Arc::new(spawn_mirror(&upstream, |_| {}).await);
    let requests: Vec<_> = (0..4)
        .map(|_| {
            let server = server.clone();
            tokio::spawn(async move { server.versions("busy.pkg").await })
        })
        .collect();
    for request in requests {
        assert_eq!(request.await.unwrap().unwrap(), ["1.0.0"]);
    }
    assert_eq!(upstream.hits("busy.pkg.1.0.0.nupkg"), 1);
}

#[tokio::test]
async fn a_large_download_is_not_held_to_the_metadata_timeout() {
    // `timeout_secs` bounds metadata requests and silences; the whole
    // download has a deadline of its own, generous by default.
    let upstream = Upstream::start().await;
    upstream.publish("Slow.Pkg", "1.0.0");
    // Four pieces 600 ms apart: 2.4 s in all, no silence near the 1 s timeout.
    upstream.behave(|b| b.trickle = Some(std::time::Duration::from_millis(600)));
    let server = spawn_mirror(&upstream, |f| f.mirror.timeout_secs = 1).await;
    let resp = server.download("slow.pkg", "1.0.0").await;
    assert!(resp.status().is_success(), "{}", resp.status());
}

// ---------------------------------------------------------------------------
// What the mirror admits
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_package_that_is_not_what_was_asked_for_is_refused() {
    // A hostile upstream answering a request for one id with a manifest that
    // claims another would otherwise publish it under that trusted name.
    let upstream = Upstream::start().await;
    upstream.serve("Wanted.Pkg", "1.0.0", build_nupkg("Trusted.Pkg", "1.0.0"));
    upstream.serve("Wanted.Pkg", "2.0.0", build_nupkg("Wanted.Pkg", "9.9.9"));
    let server = spawn_mirror(&upstream, |_| {}).await;

    assert!(server.versions("wanted.pkg").await.is_none());
    assert_eq!(
        server.download("wanted.pkg", "1.0.0").await.status(),
        StatusCode::NOT_FOUND
    );
    // Nothing landed under the name the manifest claimed either.
    assert!(server.versions("trusted.pkg").await.is_none());
    // And a refused version is not downloaded again on the next read.
    let before = upstream.hits("wanted.pkg.1.0.0.nupkg");
    server.download("wanted.pkg", "1.0.0").await;
    assert_eq!(upstream.hits("wanted.pkg.1.0.0.nupkg"), before);
}

/// A local package store and database a mirror client fills directly.
struct Local {
    storage: FilesystemStorage,
    db: SqliteDatabase,
    temp: std::path::PathBuf,
    _dir: tempfile::TempDir,
}

async fn local_feed() -> Local {
    let dir = tempfile::tempdir().unwrap();
    let storage = FilesystemStorage::new(dir.path().join("packages"))
        .await
        .unwrap();
    let temp = dir.path().join("packages").join(".mirror");
    std::fs::create_dir_all(&temp).unwrap();
    Local {
        storage,
        db: SqliteDatabase::in_memory().await.unwrap(),
        temp,
        _dir: dir,
    }
}

impl Local {
    async fn mirror(&self, upstream: &Upstream, id: &str, options: MirrorOptions) -> usize {
        let client = yanuget::mirror::MirrorClient::from_config(&mirror_config(upstream)).unwrap();
        yanuget::mirror::ensure_package(
            &client,
            &self.storage,
            &self.db,
            "mirror",
            &self.temp,
            id,
            &options,
        )
        .await
        .unwrap()
    }

    async fn membership(&self, id: &str, version: &str) -> Option<Membership> {
        let v = NuGetVersion::parse(version).unwrap();
        self.db.get_membership("mirror", id, &v).await.unwrap()
    }
}

#[tokio::test]
async fn mirrored_versions_wait_for_approval_on_a_gated_feed() {
    let upstream = Upstream::start().await;
    upstream.publish("Gated.Pkg", "1.0.0");
    let local = local_feed().await;
    let options = MirrorOptions {
        requires_approval: true,
        ..Default::default()
    };
    assert_eq!(local.mirror(&upstream, "Gated.Pkg", options).await, 1);
    let membership = local.membership("gated.pkg", "1.0.0").await.unwrap();
    assert!(membership.pending);
    let v = NuGetVersion::parse("1.0.0").unwrap();
    assert!(!local
        .db
        .is_servable("mirror", "gated.pkg", &v)
        .await
        .unwrap());
}

#[tokio::test]
async fn the_feeds_license_policy_applies_to_mirrored_versions() {
    let upstream = Upstream::start().await;
    let gpl = r#"<license type="expression">GPL-3.0-only</license>"#;
    upstream.serve(
        "Copyleft.Pkg",
        "1.0.0",
        build_nupkg_with("Copyleft.Pkg", "1.0.0", gpl),
    );
    let local = local_feed().await;
    let policy = |action| LicensePolicyConfig {
        enabled: true,
        blocked: vec!["GPL-3.0-only".into()],
        action,
        ..Default::default()
    };

    // Blocked: never admitted.
    let blocking = MirrorOptions {
        license_policy: policy(PolicyAction::Block),
        ..Default::default()
    };
    assert_eq!(local.mirror(&upstream, "Copyleft.Pkg", blocking).await, 0);
    assert!(local.membership("copyleft.pkg", "1.0.0").await.is_none());

    // Warn: admitted, and flagged for the admin.
    let other = local_feed().await;
    let warning = MirrorOptions {
        license_policy: policy(PolicyAction::Warn),
        ..Default::default()
    };
    assert_eq!(other.mirror(&upstream, "Copyleft.Pkg", warning).await, 1);
    let membership = other.membership("copyleft.pkg", "1.0.0").await.unwrap();
    assert!(membership.flagged, "{membership:?}");
}

// ---------------------------------------------------------------------------
// Migration
// ---------------------------------------------------------------------------

impl Local {
    /// Migrate everything from `upstream` into `feed`.
    async fn migrate_into(
        &self,
        feed: &str,
        upstream: &Upstream,
    ) -> yanuget::migrate::MigrateSummary {
        let mut resolved = Config::default().resolved_feeds().unwrap().remove(0);
        resolved.name = feed.to_string();
        yanuget::migrate::run(
            &self.storage,
            &self.db,
            &resolved,
            &self.temp,
            mirror_config(upstream),
            yanuget::migrate::MigrateOptions {
                quiet: true,
                ..Default::default()
            },
            indicatif::ProgressDrawTarget::hidden(),
        )
        .await
        .unwrap()
    }

    async fn migrate(&self, upstream: &Upstream) -> yanuget::migrate::MigrateSummary {
        self.migrate_into("mirror", upstream).await
    }
}

/// An upstream with three packages, one version each.
async fn three_packages() -> Upstream {
    let upstream = Upstream::start().await;
    for id in ["Alpha.Pkg", "Beta.Pkg", "Gamma.Pkg"] {
        upstream.publish(id, "1.0.0");
    }
    upstream
}

#[tokio::test]
async fn migrate_takes_the_union_of_a_capped_search_and_the_catalog() {
    // A search that stops at a cap used to be the whole list whenever it
    // returned anything; the catalog was only a fallback for an empty one.
    let upstream = three_packages().await;
    upstream.behave(|b| {
        b.search_cap = Some(1);
        b.catalog = true;
    });
    let local = local_feed().await;
    let summary = local.migrate(&upstream).await;
    assert_eq!(summary.discovered_ids, 3, "{summary:?}");
    assert_eq!(summary.imported, 3);
    assert!(summary.is_complete(), "{:?}", summary.failures);
}

#[tokio::test]
async fn migrate_falls_back_to_the_catalog_when_search_fails() {
    let upstream = three_packages().await;
    upstream.behave(|b| {
        b.search_fails = true;
        b.catalog = true;
    });
    let summary = local_feed().await.migrate(&upstream).await;
    assert_eq!(summary.imported, 3, "{:?}", summary.failures);
}

#[tokio::test]
async fn a_catalog_page_that_fails_makes_the_migration_incomplete() {
    // Its ids are missing from the copy; the run used to exit 0 regardless.
    let upstream = three_packages().await;
    upstream.behave(|b| {
        b.search_fails = true;
        b.catalog = true;
        b.broken_catalog_page = Some(1);
    });
    let summary = local_feed().await.migrate(&upstream).await;
    assert_eq!(summary.imported, 2);
    assert_eq!(summary.failed_discovery, 1);
    assert_eq!(summary.failed, 0, "no version failed");
    assert!(!summary.is_complete());
    assert!(
        summary.failures[0].id.starts_with("catalog page"),
        "{:?}",
        summary.failures
    );
}

#[tokio::test]
async fn package_and_version_failures_are_counted_apart() {
    let upstream = three_packages().await;
    upstream.behave(|b| b.fail_listing = true);
    let summary = local_feed().await.migrate(&upstream).await;
    assert_eq!((summary.failed_ids, summary.failed), (3, 0));
    assert!(summary.failures.iter().all(|f| f.version.is_none()));
}

#[tokio::test]
async fn versions_unlisted_on_the_source_stay_unlisted() {
    // The registration is gzipped, as nuget.org serves it.
    let upstream = Upstream::start().await;
    upstream.publish("Hidden.Pkg", "1.0.0");
    upstream.publish("Hidden.Pkg", "2.0.0");
    upstream.behave(|b| {
        b.registration = true;
        b.unlisted.insert(("hidden.pkg".into(), "1.0.0".into()));
    });

    let migrated = local_feed().await;
    let summary = migrated.migrate(&upstream).await;
    assert_eq!((summary.imported, summary.unlisted), (2, 1));
    assert!(
        !migrated
            .membership("hidden.pkg", "1.0.0")
            .await
            .unwrap()
            .listed
    );
    assert!(
        migrated
            .membership("hidden.pkg", "2.0.0")
            .await
            .unwrap()
            .listed
    );

    // The read-through mirror follows the source the same way.
    let mirrored = local_feed().await;
    assert_eq!(
        mirrored
            .mirror(&upstream, "Hidden.Pkg", MirrorOptions::default())
            .await,
        2
    );
    assert!(
        !mirrored
            .membership("hidden.pkg", "1.0.0")
            .await
            .unwrap()
            .listed
    );
    assert!(
        mirrored
            .membership("hidden.pkg", "2.0.0")
            .await
            .unwrap()
            .listed
    );
}

#[tokio::test]
async fn a_version_held_elsewhere_with_other_content_is_a_failure_not_a_skip() {
    // Another feed on the target holds Clash.Pkg 1.0.0 with different bytes.
    // The target will never serve the source's copy; counting it as "already
    // there" hid that.
    let theirs = Upstream::start().await;
    theirs.serve(
        "Clash.Pkg",
        "1.0.0",
        build_nupkg_with("Clash.Pkg", "1.0.0", "<tags>theirs</tags>"),
    );
    let ours = Upstream::start().await;
    ours.publish("Clash.Pkg", "1.0.0");

    let local = local_feed().await;
    assert_eq!(local.migrate_into("other", &theirs).await.imported, 1);
    let summary = local.migrate(&ours).await;
    assert_eq!((summary.skipped, summary.failed), (0, 1), "{summary:?}");
    assert!(summary.failures[0].error.contains("different content"));
}

#[tokio::test]
async fn migrate_does_not_bring_back_a_deleted_version() {
    let upstream = three_packages().await;
    let local = local_feed().await;
    let v = NuGetVersion::parse("1.0.0").unwrap();
    local
        .db
        .add_tombstone("mirror", "beta.pkg", &v)
        .await
        .unwrap();
    let summary = local.migrate(&upstream).await;
    assert_eq!((summary.imported, summary.skipped), (2, 1));
    assert!(local.membership("beta.pkg", "1.0.0").await.is_none());
}

#[tokio::test]
async fn an_id_reserved_for_another_feed_is_not_downloaded() {
    let upstream = Upstream::start().await;
    upstream.publish("Contoso.Core", "1.0.0");
    let reserved = vec![yanuget::config::ReservedPrefix {
        prefix: "Contoso.".into(),
        feed: "internal".into(),
    }];

    // The mirror does not fetch what indexing would refuse.
    let local = local_feed().await;
    let options = MirrorOptions {
        reserved_elsewhere: reserved.clone(),
        ..Default::default()
    };
    assert_eq!(local.mirror(&upstream, "Contoso.Core", options).await, 0);
    assert_eq!(upstream.hits("/flat/contoso.core/index.json"), 0);

    // Migrate reports the package once instead of downloading each version.
    let mut feed = Config::default().resolved_feeds().unwrap().remove(0);
    feed.name = "mirror".into();
    feed.reserved_elsewhere = reserved;
    let summary = yanuget::migrate::run(
        &local.storage,
        &local.db,
        &feed,
        &local.temp,
        mirror_config(&upstream),
        yanuget::migrate::MigrateOptions {
            quiet: true,
            ..Default::default()
        },
        indicatif::ProgressDrawTarget::hidden(),
    )
    .await
    .unwrap();
    assert_eq!((summary.failed_ids, summary.imported), (1, 0));
    assert!(
        summary.failures[0].error.contains("reserved"),
        "{:?}",
        summary.failures
    );
    assert_eq!(upstream.hits(".nupkg"), 0);
}
