//! The binary's exit codes and output, and the online check of the target
//! feed against a local HTTP server.

use std::path::Path;
use std::process::{Command, Output};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use yanuget_internalize::check::{check_target_online, Report};
use yanuget_internalize::config::Config;

/// Run the binary in `dir` with a clean environment (only `PATH`), so the
/// developer's own `YANUGET_INTERNALIZE_*` variables do not leak in.
fn internalize(dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_yanuget-internalize"));
    cmd.args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("RUST_LOG", "off");
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn config_check_lists_the_errors_and_exits_1() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("internalize.toml"),
        "[target]\nurl = \"https://nuget.example.com/intake\"\n",
    )
    .unwrap();
    let o = internalize(
        dir.path(),
        &[
            "--config",
            "internalize.toml",
            "config",
            "check",
            "--offline",
        ],
        &[],
    );
    assert_eq!(o.status.code(), Some(1), "{}", stdout(&o));
    let out = stdout(&o);
    assert!(
        out.contains("error: sandbox.kvm.capture_image is not set"),
        "{out}"
    );
    assert!(
        out.contains("warning: target.url without target.api_key"),
        "{out}"
    );
    assert!(
        stderr(&o).contains("the configuration has"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn config_check_reports_a_bad_file_or_variable() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("bad.toml"),
        "[sandbox]\nbackend = \"qemu\"\n",
    )
    .unwrap();
    let o = internalize(dir.path(), &["-c", "bad.toml", "config", "check"], &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stdout(&o).contains("qemu"), "{}", stdout(&o));

    // The configuration file can come from the environment too.
    let o = internalize(
        dir.path(),
        &["config", "check", "--offline"],
        &[
            ("YANUGET_INTERNALIZE_CONFIG", "bad.toml"),
            ("YANUGET_INTERNALIZE_SANDBOX_BAKCEND", "kvm"),
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    let out = stdout(&o);
    assert!(out.contains("qemu"), "{out}");
    assert!(out.contains("SANDBOX_BAKCEND is not a setting"), "{out}");
}

#[test]
fn usage_errors_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        &["plan"][..],
        &["plan", "not..an.id"],
        &["plan", "git@notaversion"],
        &["frobnicate"],
    ] {
        let o = internalize(dir.path(), args, &[]);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", stderr(&o));
    }
    let o = internalize(dir.path(), &["--version"], &[]);
    assert_eq!(o.status.code(), Some(0));
}

#[test]
fn run_needs_a_target_and_a_push_key() {
    let dir = tempfile::tempdir().unwrap();
    let o = internalize(dir.path(), &["run", "git"], &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("target.url"), "{}", stderr(&o));

    let o = internalize(
        dir.path(),
        &["run", "git"],
        &[(
            "YANUGET_INTERNALIZE_TARGET_URL",
            "https://nuget.example.com/intake/",
        )],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("target.api_key"), "{}", stderr(&o));
}

#[test]
fn plan_stops_on_a_sandbox_that_cannot_run_here() {
    let dir = tempfile::tempdir().unwrap();
    // The container backend never runs on the hosts CI tests on.
    if cfg!(windows) {
        return;
    }
    let o = internalize(
        dir.path(),
        &["plan", "git"],
        &[("YANUGET_INTERNALIZE_SANDBOX_BACKEND", "windows-container")],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("needs a Windows host"),
        "{}",
        stderr(&o)
    );
}

#[test]
fn sync_needs_a_watch_list_a_target_and_a_sandbox() {
    let dir = tempfile::tempdir().unwrap();
    let o = internalize(dir.path(), &["sync"], &[]);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("sync.watch_list"), "{}", stderr(&o));

    std::fs::write(
        dir.path().join("watch.toml"),
        "[[package]]\nid = \"a..b\"\n",
    )
    .unwrap();
    let watch = [("YANUGET_INTERNALIZE_SYNC_WATCH_LIST", "watch.toml")];
    let o = internalize(dir.path(), &["sync", "--dry-run"], &watch);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("not a package id"), "{}", stderr(&o));

    std::fs::write(dir.path().join("watch.toml"), "[[package]]\nid = \"git\"\n").unwrap();
    let o = internalize(dir.path(), &["sync"], &watch);
    assert_eq!(o.status.code(), Some(1));
    assert!(stderr(&o).contains("target.url"), "{}", stderr(&o));

    // The container backend never runs on the hosts CI tests on.
    if cfg!(windows) {
        return;
    }
    let o = internalize(
        dir.path(),
        &["sync"],
        &[
            watch[0],
            (
                "YANUGET_INTERNALIZE_TARGET_URL",
                "https://nuget.example.com/intake/",
            ),
            (
                "YANUGET_INTERNALIZE_TARGET_API_KEY",
                "a-push-key-of-the-intake-feed",
            ),
            ("YANUGET_INTERNALIZE_SANDBOX_BACKEND", "windows-container"),
        ],
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(
        stderr(&o).contains("needs a Windows host"),
        "{}",
        stderr(&o)
    );
    // It stopped before it took the lock or wrote any state.
    assert!(!dir.path().join("state").exists());
}

/// Serve `responses` in order, one per connection, on a local port; return
/// the feed URL.
async fn serve(responses: Vec<&'static str>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.ok();
        }
    });
    format!("http://{addr}/intake/")
}

fn http(status: &str, body: &str) -> &'static str {
    Box::leak(
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_boxed_str(),
    )
}

async fn online(url: Option<String>) -> Report {
    let mut config = Config::default();
    config.target.url = url;
    config.target.timeout_secs = 5;
    let mut report = Report::default();
    check_target_online(&config, &mut report).await;
    report
}

#[tokio::test]
async fn the_target_has_to_answer_and_take_pushes() {
    let publish = r#"{"version":"3.0.0","resources":[{"@id":"x","@type":"PackagePublish/2.0.0"}]}"#;
    let read_only = r#"{"version":"3.0.0","resources":[{"@id":"x","@type":"SearchQueryService"}]}"#;
    let url = serve(vec![
        http("200 OK", publish),
        http("200 OK", read_only),
        http("401 Unauthorized", ""),
        http("500 Internal Server Error", ""),
        http("200 OK", "<html>"),
    ])
    .await;

    let r = online(Some(url.clone())).await;
    assert!(r.errors.is_empty() && r.warnings.is_empty(), "{r:?}");
    let r = online(Some(url.clone())).await;
    assert!(
        r.errors.iter().any(|e| e.contains("no PackagePublish")),
        "{r:?}"
    );
    let r = online(Some(url.clone())).await;
    assert!(r.warnings.iter().any(|w| w.contains("read-gated")), "{r:?}");
    let r = online(Some(url.clone())).await;
    assert!(r.errors.iter().any(|e| e.contains("500")), "{r:?}");
    let r = online(Some(url.clone())).await;
    assert!(
        r.errors.iter().any(|e| e.contains("service index")),
        "{r:?}"
    );
    // Nobody listens any more.
    let r = online(Some(url)).await;
    assert!(
        r.errors.iter().any(|e| e.contains("does not answer")),
        "{r:?}"
    );

    let r = online(None).await;
    assert!(r.errors.is_empty() && r.warnings.is_empty());
}
