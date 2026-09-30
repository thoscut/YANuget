//! End-to-end tests of the security claims in the README and SECURITY.md:
//! per-feed key isolation, read-gated feeds, forwarding chains, TLS, caching of
//! gated content, host validation and strict configuration.
//!
//! The helpers are a minimal copy of those in `integration.rs`, so each test
//! binary stays self-contained.

use std::time::Duration;

/// Run the real binary with `env` and return its exit status and stderr,
/// killing it if it is still running after `timeout` (it then started, which
/// is what these tests assert does not happen).
fn run_binary(
    env: &[(&str, &str)],
    timeout: Duration,
) -> (Option<std::process::ExitStatus>, String) {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_yanuget"));
    for (k, _) in std::env::vars() {
        if k.starts_with("YANUGET_") {
            cmd.env_remove(k);
        }
    }
    cmd.env("YANUGET_DATA_DIR", dir.path())
        .env("YANUGET_PORT", "0")
        .env("YANUGET_HOST", "127.0.0.1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    (status, stderr)
}

#[test]
fn an_unparseable_environment_value_stops_the_server() {
    // `enabled` is not a boolean spelling; it used to be read as "off" and
    // serve plain HTTP.
    for (name, value) in [
        ("YANUGET_TLS_ENABLED", "enabled"),
        ("YANUGET_MAX_PACKAGE_SIZE_BYTES", "10G"),
        ("YANUGET_RATELIMIT_WINDOW_SECS", "0"),
    ] {
        let (status, stderr) = run_binary(&[(name, value)], Duration::from_secs(30));
        let status = status.unwrap_or_else(|| panic!("{name}={value}: the server started"));
        assert!(!status.success(), "{name}={value}: exited successfully");
        let expect = if name.ends_with("WINDOW_SECS") {
            "window_secs"
        } else {
            name
        };
        assert!(
            stderr.contains(expect),
            "{name}={value}: error does not name the setting: {stderr}"
        );
    }
}
