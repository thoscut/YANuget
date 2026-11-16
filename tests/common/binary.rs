//! The real `yanuget` binary, for what only it does: the data lock, the
//! signal handling, the wiring of the command line.

use std::path::Path;

/// A command for the binary that does not see the caller's `YANUGET_*`
/// variables or its `RUST_LOG`.
///
/// The variables override the configuration file, so an exported
/// `YANUGET_DATA_DIR` would send a test's `restore --force` or `migrate`
/// into a data directory that is not the test's. `RUST_LOG` decides what
/// the binary prints, which the tests read. A test that wants either sets
/// it on the command itself.
pub fn yanuget_command() -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_yanuget"));
    for (name, _) in std::env::vars_os() {
        if name.to_str().is_some_and(|n| n.starts_with("YANUGET_")) {
            command.env_remove(&name);
        }
    }
    command
        .env_remove("RUST_LOG")
        .env("NO_COLOR", "1")
        .kill_on_drop(true);
    command
}

/// `[[feeds]]` tables for feeds of these names, with nothing else set.
pub fn feed_tables(feeds: &[&str]) -> String {
    feeds
        .iter()
        .map(|name| format!("[[feeds]]\nname = '{name}'\n"))
        .collect()
}

/// The configuration of a plain-HTTP server on loopback that keeps its data
/// in `data`, followed by `rest`.
pub fn config_toml(data: &Path, port: u16, rest: &str) -> String {
    // A TOML literal string takes a Windows path's backslashes as they are.
    format!(
        "data_dir = '{}'\ntls_enabled = false\nhost = '127.0.0.1'\nport = {port}\n{rest}",
        data.display()
    )
}

/// A running server process, and the port it listens on.
pub struct ServerProcess {
    pub child: tokio::process::Child,
    pub port: u16,
}

impl ServerProcess {
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Stop the server the way a service manager or a container runtime
    /// does, with SIGTERM, and wait for it to exit: its status, or a panic
    /// when it is still running after `within`.
    #[cfg(unix)]
    pub async fn terminate(mut self, within: std::time::Duration) -> std::process::ExitStatus {
        let pid = self.child.id().expect("the server is still running");
        let sent = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .unwrap();
        assert!(sent.success(), "kill -TERM {pid} failed");
        tokio::time::timeout(within, self.child.wait())
            .await
            .expect("the server kept running after SIGTERM")
            .unwrap()
    }
}

/// The command that serves `data` with the feeds `feeds`, on port 0.
pub fn server_command(data: &Path, feeds: &[&str]) -> tokio::process::Command {
    server_command_with(data, &feed_tables(feeds))
}

/// [`server_command`] with `rest` appended to the configuration. Its file is
/// one of its own, so a test can write the configuration a command-line
/// client reads beside it.
pub fn server_command_with(data: &Path, rest: &str) -> tokio::process::Command {
    let config = data.join("server.toml");
    std::fs::write(&config, config_toml(data, 0, rest)).unwrap();
    let mut command = yanuget_command();
    // `start_server` learns the port from the "listening on" line, an INFO
    // event: under a stricter filter it would never come.
    command.arg("--config").arg(&config).env("RUST_LOG", "info");
    command
}

/// Start the real server on `data` with the feeds `feeds`, and wait until it
/// listens.
pub async fn start_server(data: &Path, feeds: &[&str]) -> ServerProcess {
    start(server_command(data, feeds)).await
}

/// Start the server `command` describes, and wait until it listens.
pub async fn start(mut command: tokio::process::Command) -> ServerProcess {
    use tokio::io::AsyncBufReadExt;

    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let listening = async {
        while let Some(line) = lines.next_line().await.unwrap() {
            // "YANuget listening on http://127.0.0.1:{port}"
            if let Some((_, addr)) = line.split_once("listening on http://127.0.0.1:") {
                let digits: String = addr.chars().take_while(char::is_ascii_digit).collect();
                return digits.parse::<u16>().unwrap();
            }
        }
        panic!("the server exited before it listened");
    };
    let port = tokio::time::timeout(std::time::Duration::from_secs(60), listening)
        .await
        .expect("the server never listened");
    // Keep reading, so the server never blocks on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    ServerProcess { child, port }
}

/// Run the server command to its end: its output, or a panic when it is
/// still running after a minute (it then started, which the callers assert
/// it does not).
pub async fn server_exits(mut command: tokio::process::Command) -> std::process::Output {
    let running = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
        .wait_with_output();
    tokio::time::timeout(std::time::Duration::from_secs(60), running)
        .await
        .expect("the server started and kept running")
        .unwrap()
}

/// Stdout and stderr of a finished command, as one text.
pub fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}
