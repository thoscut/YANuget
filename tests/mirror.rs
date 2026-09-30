//! Read-through mirroring and migration against a fake upstream V3 feed on
//! loopback: what the mirror fetches, what it sends where, and what it refuses.

use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use yanuget::config::{MirrorAuthConfig, MirrorConfig};
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
