//! The closure against a local v3 feed with a flat container only, whose
//! dependencies come from the manifests, and the target feed's listing.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use yanuget_internalize::closure;
use yanuget_internalize::config::{TargetConfig, UpstreamConfig};
use yanuget_internalize::target::Target;
use yanuget_internalize::upstream::Upstream;
use yanuget_pkg::NuGetVersion;

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn nuspec(id: &str, version: &str, deps: &str) -> String {
    format!(
        r#"<?xml version="1.0"?><package xmlns="http://schemas.microsoft.com/packaging/2015/06/nuspec.xsd"><metadata><id>{id}</id><version>{version}</version><authors>a</authors><description>d</description><dependencies>{deps}</dependencies></metadata></package>"#
    )
}

/// Files by path: the flat container's version lists and manifests.
type Files = Arc<HashMap<String, String>>;

async fn flat_feed() -> String {
    let mut files = HashMap::new();
    files.insert(
        "index.json".to_string(),
        json!({"version": "3.0.0", "resources": [
            {"@id": "flat/", "@type": "PackageBaseAddress/3.0.0"}
        ]})
        .to_string(),
    );
    let lists = [
        ("app", &["1.0.0"][..]),
        ("lib.extension", &["1.0.0", "1.2.0", "2.0.0"]),
    ];
    for (id, versions) in lists {
        files.insert(
            format!("flat/{id}/index.json"),
            json!({ "versions": versions }).to_string(),
        );
    }
    let deps = [
        (
            "app",
            "1.0.0",
            r#"<dependency id="Lib.Extension" version="[1.0,2.0)"/><dependency id="chocolatey" version="1.0"/>"#,
        ),
        ("lib.extension", "1.0.0", ""),
        ("lib.extension", "1.2.0", ""),
        ("lib.extension", "2.0.0", ""),
    ];
    for (id, version, d) in deps {
        files.insert(
            format!("flat/{id}/{version}/{id}.nuspec"),
            nuspec(id, version, d),
        );
    }
    let files: Files = Arc::new(files);
    let app = Router::new()
        .route(
            "/{*path}",
            get(
                |State(files): State<Files>, Path(path): Path<String>| async move {
                    match files.get(&path) {
                        Some(body) => body.clone().into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                },
            ),
        )
        .with_state(files);
    serve(app).await
}

#[tokio::test]
async fn a_flat_container_feed_gives_its_dependencies_through_the_manifests() {
    let base = flat_feed().await;
    let cache = tempfile::tempdir().unwrap();
    let config = UpstreamConfig {
        url: format!("{base}/index.json"),
        allow_private: true,
        timeout_secs: 5,
        ..UpstreamConfig::default()
    };
    let upstream = Upstream::new(&config, cache.path()).unwrap();
    let root = upstream.resolve("app", None).await.unwrap();
    let closure = closure::resolve(&upstream, &root).await.unwrap();
    let names: Vec<String> = closure
        .packages
        .iter()
        .map(|p| format!("{} {}", p.info.id, p.info.version.normalized()))
        .collect();
    // The highest version under 2.0; a flat container has no spelling of the
    // id, so the dependent's is kept.
    assert_eq!(names, ["app 1.0.0", "Lib.Extension 1.2.0"]);
    assert_eq!(closure.provided.len(), 1);
}

#[tokio::test]
async fn the_target_says_what_it_serves_unless_it_is_read_gated() {
    let app = Router::new()
        .route(
            "/open/v3/index.json",
            get(|| async {
                Json(json!({"version": "3.0.0", "resources": [
                    {"@id": "../v3/flat", "@type": "PackageBaseAddress/3.0.0"},
                    {"@id": "../api/v2/package", "@type": "PackagePublish/2.0.0"}
                ]}))
            }),
        )
        .route(
            "/open/v3/flat/git/index.json",
            get(|| async { Json(json!({"versions": ["2.50.0", "2.51.0"]})) }),
        )
        .route(
            "/gated/v3/index.json",
            get(|| async { StatusCode::UNAUTHORIZED }),
        );
    let base = serve(app).await;
    let target = |feed: &str| {
        Target::new(&TargetConfig {
            url: Some(format!("{base}/{feed}/")),
            ..TargetConfig::default()
        })
        .unwrap()
        .unwrap()
    };

    let open = target("open");
    let versions = open.versions("Git").await.unwrap().unwrap();
    assert!(versions.contains(&NuGetVersion::parse("2.51.0").unwrap()));
    assert_eq!(open.versions("unknown").await.unwrap(), Some(Vec::new()));

    assert_eq!(target("gated").versions("git").await.unwrap(), None);
    assert!(target("missing").versions("git").await.is_err());
    assert!(Target::new(&TargetConfig::default()).unwrap().is_none());
}
