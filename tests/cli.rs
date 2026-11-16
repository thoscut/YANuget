//! `yanuget admin` and `yanuget backup` as an operator runs them: the real
//! binary, against a running server, configured the way
//! `docs/admin-cli.md` describes. The library parts are tested elsewhere;
//! this is the wiring only the binary has: where the admin key comes from,
//! which feed is meant, and by which name the server on this machine is
//! asked for.

mod common;

use std::net::{Ipv4Addr, SocketAddr};

use common::binary::{config_toml, server_command_with, start, text, yanuget_command};
use common::{build_nupkg, ADMIN_KEY, API_KEY};

/// A server that answers only for `nuget.test` (`allowed_hosts`), with a
/// gated feed and an open one. With `admin_key`, the gated feed's admin key
/// is in the configuration; without, it is left out, as on a machine where
/// the command takes it from the environment.
fn settings(admin_key: bool) -> String {
    let key = if admin_key {
        format!("admin_api_key = '{ADMIN_KEY}'\n")
    } else {
        String::new()
    };
    format!(
        "api_key = '{API_KEY}'\nallowed_hosts = ['nuget.test']\n\n\
         [[feeds]]\nname = 'gated'\nrequires_approval = true\n{key}\n\
         [[feeds]]\nname = 'open'\n"
    )
}

/// A client that reaches the server under the name it serves.
fn client(port: u16) -> reqwest::Client {
    common::client_builder()
        .resolve("nuget.test", SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
        .build()
        .unwrap()
}

async fn push(port: u16, feed: &str, nupkg: Vec<u8>) -> reqwest::StatusCode {
    let part = reqwest::multipart::Part::bytes(nupkg)
        .file_name("package.nupkg")
        .mime_str("application/octet-stream")
        .unwrap();
    client(port)
        .put(format!("http://nuget.test:{port}/{feed}/api/v2/package"))
        .header("X-NuGet-ApiKey", API_KEY)
        .multipart(reqwest::multipart::Form::new().part("package", part))
        .send()
        .await
        .unwrap()
        .status()
}

/// Whether `feed` serves Cli.Pkg 1.0.0 to clients.
async fn served(port: u16, feed: &str) -> bool {
    client(port)
        .get(format!(
            "http://nuget.test:{port}/{feed}/v3/package/cli.pkg/1.0.0/cli.pkg.1.0.0.nupkg"
        ))
        .send()
        .await
        .unwrap()
        .status()
        .is_success()
}

/// Write the configuration the command-line client reads, for the server
/// on `port`, and return its path.
fn cli_config(data: &std::path::Path, port: u16, settings: &str) -> std::path::PathBuf {
    let path = data.join("cli.toml");
    std::fs::write(&path, config_toml(data, port, settings)).unwrap();
    path
}

/// `TEST-29`: `yanuget admin` finds the server on this machine from the
/// configuration, by the name `allowed_hosts` lets through (a request for
/// `127.0.0.1` is refused there), with the feed's prefix and its admin key
/// from the configuration, or the key from `YANUGET_ADMIN_KEY`.
#[tokio::test]
async fn admin_acts_on_the_local_server_by_the_name_it_serves() {
    let data = tempfile::tempdir().unwrap();
    let server = start(server_command_with(data.path(), &settings(true))).await;
    let port = server.port;

    // The host check is on: by address, the server does not answer.
    let by_address = common::client()
        .get(server.url("/gated/v3/index.json"))
        .send()
        .await
        .unwrap();
    assert!(
        by_address.status().is_client_error(),
        "{}",
        by_address.status()
    );

    assert_eq!(
        push(port, "gated", build_nupkg("Cli.Pkg", "1.0.0", b"c")).await,
        reqwest::StatusCode::CREATED
    );
    assert!(!served(port, "gated").await, "pending, yet served");

    // The key from the configuration.
    let config = cli_config(data.path(), port, &settings(true));
    let approve = yanuget_command()
        .arg("--config")
        .arg(&config)
        .args(["admin", "--feed", "gated", "approve", "Cli.Pkg", "1.0.0"])
        .output()
        .await
        .unwrap();
    assert!(approve.status.success(), "{}", text(&approve));
    assert!(
        text(&approve).contains("approved Cli.Pkg 1.0.0"),
        "{}",
        text(&approve)
    );
    assert!(served(port, "gated").await, "approved, yet not served");

    // Two feeds, and none named: the command does not guess.
    let unnamed = yanuget_command()
        .arg("--config")
        .arg(&config)
        .args(["admin", "disable", "Cli.Pkg", "1.0.0"])
        .output()
        .await
        .unwrap();
    assert!(!unnamed.status.success());
    assert!(text(&unnamed).contains("--feed"), "{}", text(&unnamed));

    // The key from the environment, where the configuration has none; a
    // wrong one is refused by the server and changes nothing.
    let config = cli_config(data.path(), port, &settings(false));
    let disable = |key: &str| {
        let mut command = yanuget_command();
        command
            .arg("--config")
            .arg(&config)
            .args(["admin", "--feed", "gated", "disable", "Cli.Pkg", "1.0.0"])
            .env("YANUGET_ADMIN_KEY", key);
        command
    };
    let wrong = disable("not-the-key").output().await.unwrap();
    assert!(!wrong.status.success(), "{}", text(&wrong));
    assert!(served(port, "gated").await);
    let disabled = disable(ADMIN_KEY).output().await.unwrap();
    assert!(disabled.status.success(), "{}", text(&disabled));
    assert!(
        text(&disabled).contains("disabled Cli.Pkg 1.0.0"),
        "{}",
        text(&disabled)
    );
    assert!(!served(port, "gated").await, "disabled, yet served");
}

/// `TEST-29`: `yanuget backup` copies the store of a running server and
/// says what it copied, and a second run copies nothing new.
#[tokio::test]
async fn backup_copies_a_running_servers_store_and_says_what_it_did() {
    let data = tempfile::tempdir().unwrap();
    let server = start(server_command_with(data.path(), &settings(true))).await;
    let port = server.port;
    assert_eq!(
        push(port, "open", build_nupkg("Cli.Pkg", "1.0.0", b"c")).await,
        reqwest::StatusCode::CREATED
    );

    let config = cli_config(data.path(), port, &settings(true));
    let dest = tempfile::tempdir().unwrap();
    let backup = || {
        let mut command = yanuget_command();
        command
            .arg("--config")
            .arg(&config)
            .arg("backup")
            .arg(dest.path());
        command
    };
    let first = backup().output().await.unwrap();
    assert!(first.status.success(), "{}", text(&first));
    let said = String::from_utf8_lossy(&first.stdout).into_owned();
    assert!(said.starts_with("Backed up 1 package versions, "), "{said}");
    assert!(
        said.contains(&format!("to {}", dest.path().display())),
        "{said}"
    );
    assert!(dest.path().join(yanuget::backup::MANIFEST).is_file());
    assert!(dest.path().join("yanuget.db").is_file());

    // Nothing changed in between, so nothing is copied again.
    let second = backup().output().await.unwrap();
    assert!(second.status.success(), "{}", text(&second));
    let said = String::from_utf8_lossy(&second.stdout).into_owned();
    assert!(said.contains(": 0 copied ("), "{said}");
    assert!(said.trim_end().ends_with("0 removed."), "{said}");
}
