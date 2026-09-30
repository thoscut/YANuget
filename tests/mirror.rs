//! Read-through mirroring and migration against a fake upstream V3 feed on
//! loopback: what the mirror fetches, what it sends where, and what it refuses.

use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use yanuget::config::{Config, FeedConfig, MirrorAuthConfig, MirrorConfig};
use yanuget::database::SqliteDatabase;
use yanuget::storage::FilesystemStorage;
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
    let behaviour = state.behaviour.lock().unwrap();
    let base = &state.base;

    if path == "/v3/index.json" {
        let package_base = behaviour
            .package_base
            .clone()
            .unwrap_or_else(|| format!("{base}/flat/"));
        return axum::Json(serde_json::json!({
            "version": "3.0.0",
            "resources": [
                {"@id": package_base, "@type": "PackageBaseAddress/3.0.0"},
                {"@id": format!("{base}/query"), "@type": "SearchQueryService"},
            ]
        }))
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
        let data: Vec<serde_json::Value> = behaviour
            .packages
            .keys()
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
