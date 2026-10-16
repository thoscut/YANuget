//! `yanuget-internalize config check`: everything a run would stop on, found
//! before it runs, the way `yanuget config check` does it for the server.
//!
//! It loads the configuration as a run does (TOML, environment, key files),
//! then looks at what it points at: the sandbox backend's host (`/dev/kvm`,
//! QEMU, the golden images), the directories, and, unless `--offline`, the
//! target feed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::config::{Backend, Config, KvmConfig, KNOWN_ENV};
use crate::sandbox::{kvm, windows_container};

/// What the check found.
#[derive(Debug, Default)]
pub struct Report {
    /// A run would stop on these.
    pub errors: Vec<String>,
    /// A run goes ahead, but probably not as intended.
    pub warnings: Vec<String>,
    /// Worth knowing; nothing to fix.
    pub notes: Vec<String>,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// The host the check looks at. [`Host::current`] outside tests.
#[derive(Debug, Clone)]
pub struct Host {
    pub linux: bool,
    pub windows: bool,
    pub kvm_device: PathBuf,
    /// The names of the process's environment variables.
    pub env_names: Vec<String>,
}

impl Host {
    pub fn current() -> Self {
        Host {
            linux: cfg!(target_os = "linux"),
            windows: cfg!(windows),
            kvm_device: PathBuf::from(kvm::KVM_DEVICE),
            env_names: std::env::vars_os()
                .filter_map(|(k, _)| k.into_string().ok())
                .collect(),
        }
    }
}

/// Fewer characters than this is guessable online; the server warns about
/// the same keys.
pub const MIN_KEY_CHARS: usize = 32;

/// Load the configuration at `path` as a run would, and check it against this
/// host; with `online`, also ask the target feed.
pub async fn check(path: Option<&str>, online: bool) -> Report {
    let mut report = Report::default();
    let host = Host::current();
    check_env_names(&host, &mut report);
    let config = match Config::load(path) {
        Ok(c) => c,
        Err(e) => {
            report.errors.push(e.to_string());
            return report;
        }
    };
    check_loaded(&config, &host, &mut report);
    if online {
        check_target_online(&config, &mut report).await;
    } else if config.target.url.is_some() {
        report
            .notes
            .push("--offline: the target feed was not asked".into());
    }
    report
}

/// The checks that need no network, on a loaded configuration.
pub fn check_loaded(config: &Config, host: &Host, report: &mut Report) {
    check_target(config, report);
    check_upstream(config, report);
    match config.sandbox.backend {
        Backend::Kvm => check_kvm(&config.sandbox.kvm, host, report),
        Backend::WindowsContainer => check_container(config, host, report),
    }
    for (name, dir) in [
        ("cache_dir", &config.cache_dir),
        ("work_dir", &config.work_dir),
    ] {
        if dir.exists() && !dir.is_dir() {
            report.errors.push(format!(
                "{name} {} exists but is not a directory",
                dir.display()
            ));
        } else if !dir.exists() {
            report.notes.push(format!(
                "{name} {} does not exist yet; a run creates it",
                dir.display()
            ));
        }
    }
    if config.cache_dir == config.work_dir {
        report.warnings.push(
            "cache_dir and work_dir are the same directory; a run clears its work \
             files, so keep the cache apart"
                .into(),
        );
    }
}

/// A `YANUGET_INTERNALIZE_*` variable nothing reads is a typo: the TOML file
/// refuses an unknown key, and the environment should not be quieter.
pub fn check_env_names(host: &Host, report: &mut Report) {
    let mut unknown: Vec<&str> = host
        .env_names
        .iter()
        .map(String::as_str)
        .filter(|n| n.starts_with(crate::config::ENV_PREFIX) && !KNOWN_ENV.contains(n))
        .collect();
    unknown.sort_unstable();
    for name in unknown {
        report.warnings.push(format!(
            "environment variable {name} is not a setting and is ignored"
        ));
    }
}

fn check_target(config: &Config, report: &mut Report) {
    let target = &config.target;
    let Some(url) = &target.url else {
        report
            .notes
            .push("no target.url: `plan` works, `run` refuses to start".into());
        return;
    };
    match &target.api_key {
        None => report
            .warnings
            .push("target.url without target.api_key: `run` refuses to start".into()),
        Some(key) if key.expose().chars().count() < MIN_KEY_CHARS => {
            report.warnings.push(format!(
                "target.api_key is shorter than {MIN_KEY_CHARS} characters"
            ));
        }
        Some(_) => {}
    }
    if url.starts_with("http://") && !is_loopback_url(url) {
        report.warnings.push(format!(
            "target.url {url} is plain HTTP: the push key and the packages cross \
             the network unencrypted"
        ));
    }
}

fn check_upstream(config: &Config, report: &mut Report) {
    let upstream = &config.upstream;
    if upstream.url.starts_with("http://") {
        report
            .warnings
            .push(format!("upstream.url {} is plain HTTP", upstream.url));
    }
    if upstream.url == crate::config::COMMUNITY_REPOSITORY && upstream.concurrency > 4 {
        report.warnings.push(format!(
            "upstream.concurrency = {} against the community repository, which \
             rate-limits heavy users; 2 to 4 is safer",
            upstream.concurrency
        ));
    }
}

fn check_kvm(kvm: &KvmConfig, host: &Host, report: &mut Report) {
    if !host.linux {
        report
            .errors
            .push("the kvm backend needs a Linux host".into());
        return;
    }
    check_kvm_device(&host.kvm_device, report);
    if kvm::find_program(&kvm.qemu).is_none() {
        report.errors.push(format!(
            "sandbox.kvm.qemu: {} not found",
            kvm.qemu.display()
        ));
    }
    match &kvm.capture_image {
        None => report
            .errors
            .push("sandbox.kvm.capture_image is not set".into()),
        Some(p) => check_image("sandbox.kvm.capture_image", p, report),
    }
    match &kvm.stock_image {
        None => report
            .notes
            .push("no sandbox.kvm.stock_image: validation installs are not possible".into()),
        Some(p) => check_image("sandbox.kvm.stock_image", p, report),
    }
    if kvm.capture_image.is_some() && kvm.capture_image == kvm.stock_image {
        report.errors.push(
            "sandbox.kvm.capture_image and stock_image are the same file; the \
             stock image must not carry the capture helpers"
                .into(),
        );
    }
    if kvm.memory_mib < 2048 {
        report.warnings.push(format!(
            "sandbox.kvm.memory_mib = {}: Windows Server Core needs at least 2048",
            kvm.memory_mib
        ));
    }
    report
        .notes
        .push("the KVM runner is not built yet: `plan` and `run` stop before capture".into());
}

fn check_kvm_device(device: &Path, report: &mut Report) {
    if !device.exists() {
        report.errors.push(format!(
            "{} does not exist: this host offers no hardware virtualization \
             (software emulation is too slow to be useful)",
            device.display()
        ));
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if !std::fs::metadata(device).is_ok_and(|m| m.file_type().is_char_device()) {
            report
                .errors
                .push(format!("{} is not a character device", device.display()));
            return;
        }
    }
    if let Err(e) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
    {
        report.errors.push(format!(
            "{} cannot be opened for reading and writing: {e} (is this user in \
             the kvm group?)",
            device.display()
        ));
    }
}

fn check_image(name: &str, path: &Path, report: &mut Report) {
    if !path.is_file() {
        report
            .errors
            .push(format!("{name} {} is not a file", path.display()));
        return;
    }
    match kvm::is_qcow2(path) {
        Ok(true) => {}
        Ok(false) => report
            .errors
            .push(format!("{name} {} is not a qcow2 image", path.display())),
        Err(e) => report
            .errors
            .push(format!("{name} {} cannot be read: {e}", path.display())),
    }
}

fn check_container(config: &Config, host: &Host, report: &mut Report) {
    if !host.windows {
        report
            .errors
            .push("the windows-container backend needs a Windows host".into());
    }
    match &config.sandbox.windows_container.image {
        None => report
            .errors
            .push("sandbox.windows_container.image is not set".into()),
        Some(image) if !windows_container::is_pinned(image) => report.warnings.push(format!(
            "sandbox.windows_container.image {image:?} is not pinned by digest \
                 (name@sha256:…), so a captured package depends on whatever the tag \
                 points at"
        )),
        Some(_) => {}
    }
    report.notes.push(
        "the windows-container runner is not built yet: `plan` and `run` stop before capture"
            .into(),
    );
}

fn is_loopback_url(url: &str) -> bool {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|h| {
            let h = h.trim_start_matches('[').trim_end_matches(']');
            h.eq_ignore_ascii_case("localhost")
                || h.parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

/// Ask the target feed for its service index: it has to answer, and it has
/// to take pushes.
pub async fn check_target_online(config: &Config, report: &mut Report) {
    let Some(url) = &config.target.url else {
        return;
    };
    let index = format!("{url}v3/index.json");
    let client = match reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        .timeout(Duration::from_secs(config.target.timeout_secs.min(30)))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            report
                .errors
                .push(format!("cannot build an HTTP client: {e}"));
            return;
        }
    };
    let response = match client.get(&index).send().await {
        Ok(r) => r,
        Err(e) => {
            report
                .errors
                .push(format!("target feed {index} does not answer: {e}"));
            return;
        }
    };
    let status = response.status();
    if status == reqwest::StatusCode::UNAUTHORIZED {
        report.warnings.push(format!(
            "target feed {index} answers 401: it is read-gated, and the tool \
             holds a push key only, so it cannot see what the feed already holds"
        ));
        return;
    }
    if !status.is_success() {
        report
            .errors
            .push(format!("target feed {index} answers {status}"));
        return;
    }
    let body: serde_json::Value = match response.json().await {
        Ok(b) => b,
        Err(e) => {
            report.errors.push(format!(
                "target feed {index} does not answer with a service index: {e}"
            ));
            return;
        }
    };
    let takes_pushes = body["resources"].as_array().is_some_and(|resources| {
        resources.iter().any(|r| {
            r["@type"]
                .as_str()
                .is_some_and(|t| t.starts_with("PackagePublish/"))
        })
    });
    if !takes_pushes {
        report.errors.push(format!(
            "target feed {index} lists no PackagePublish resource, so it takes no pushes"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Secret;

    fn host(kvm_device: PathBuf) -> Host {
        Host {
            linux: true,
            windows: false,
            kvm_device,
            env_names: vec![],
        }
    }

    fn checked(config: &Config, host: &Host) -> Report {
        let mut report = Report::default();
        check_loaded(config, host, &mut report);
        report
    }

    fn has(list: &[String], needle: &str) -> bool {
        list.iter().any(|m| m.contains(needle))
    }

    /// A configuration whose KVM host checks pass in `dir`: `/dev/null` stands
    /// in for `/dev/kvm` (a character device anyone can open), `sh` for QEMU.
    fn good(dir: &Path) -> (Config, Host) {
        let image = dir.join("capture.qcow2");
        std::fs::write(&image, b"QFI\xfb\0\0\0\x03rest").unwrap();
        let mut config = Config {
            cache_dir: dir.join("cache"),
            work_dir: dir.join("work"),
            ..Config::default()
        };
        config.sandbox.kvm.qemu = PathBuf::from("sh");
        config.sandbox.kvm.capture_image = Some(image);
        config.target.url = Some("https://nuget.example.com/intake/".into());
        config.target.api_key = Some(Secret::new("k".repeat(MIN_KEY_CHARS)));
        (config, host(PathBuf::from("/dev/null")))
    }

    #[test]
    fn a_good_kvm_configuration_has_no_errors_or_warnings() {
        let dir = tempfile::tempdir().unwrap();
        let (config, host) = good(dir.path());
        let r = checked(&config, &host);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert!(has(&r.notes, "stock_image"));
    }

    #[test]
    fn the_kvm_host_and_images_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, _) = good(dir.path());
        let not_qcow2 = dir.path().join("stock.qcow2");
        std::fs::write(&not_qcow2, b"not an image").unwrap();
        config.sandbox.kvm.stock_image = Some(not_qcow2);
        config.sandbox.kvm.qemu = PathBuf::from("/nonexistent/qemu-system-x86_64");
        config.sandbox.kvm.memory_mib = 1024;

        let r = checked(&config, &host(dir.path().join("kvm")));
        assert!(
            has(&r.errors, "does not exist: this host offers no hardware"),
            "{:?}",
            r.errors
        );
        assert!(
            has(&r.errors, "qemu-system-x86_64 not found"),
            "{:?}",
            r.errors
        );
        assert!(has(&r.errors, "stock_image"), "{:?}", r.errors);
        assert!(has(&r.warnings, "memory_mib"), "{:?}", r.warnings);

        // A regular file is no KVM device.
        let r = checked(
            &config,
            &host(config.sandbox.kvm.capture_image.clone().unwrap()),
        );
        assert!(has(&r.errors, "not a character device"), "{:?}", r.errors);

        config.sandbox.kvm.capture_image = None;
        let r = checked(&config, &host(PathBuf::from("/dev/null")));
        assert!(has(&r.errors, "capture_image is not set"), "{:?}", r.errors);

        let r = checked(
            &config,
            &Host {
                linux: false,
                ..host(PathBuf::from("/dev/null"))
            },
        );
        assert_eq!(r.errors, ["the kvm backend needs a Linux host"]);
    }

    #[test]
    fn the_target_and_upstream_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, host) = good(dir.path());
        config.target.api_key = None;
        config.target.url = Some("http://nuget.example.com/intake/".into());
        config.upstream.concurrency = 16;
        let r = checked(&config, &host);
        assert!(
            has(&r.warnings, "without target.api_key"),
            "{:?}",
            r.warnings
        );
        assert!(has(&r.warnings, "plain HTTP"), "{:?}", r.warnings);
        assert!(has(&r.warnings, "rate-limits"), "{:?}", r.warnings);

        config.target.url = Some("http://127.0.0.1:5000/intake/".into());
        config.target.api_key = Some(Secret::new("short"));
        let r = checked(&config, &host);
        assert!(!has(&r.warnings, "plain HTTP"), "{:?}", r.warnings);
        assert!(has(&r.warnings, "shorter than"), "{:?}", r.warnings);

        config.target.url = None;
        let r = checked(&config, &host);
        assert!(has(&r.notes, "`plan` works"), "{:?}", r.notes);
    }

    #[test]
    fn the_container_backend_wants_windows_and_a_pinned_image() {
        let mut config = Config::default();
        config.sandbox.backend = Backend::WindowsContainer;
        let linux = host(PathBuf::from("/dev/null"));
        let r = checked(&config, &linux);
        assert!(has(&r.errors, "needs a Windows host"), "{:?}", r.errors);
        assert!(has(&r.errors, "image is not set"), "{:?}", r.errors);

        config.sandbox.windows_container.image = Some("internalize:latest".into());
        let windows = Host {
            linux: false,
            windows: true,
            ..linux
        };
        let r = checked(&config, &windows);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(has(&r.warnings, "not pinned by digest"), "{:?}", r.warnings);
    }

    #[test]
    fn directories_and_unknown_variables_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, mut host) = good(dir.path());
        let file = dir.path().join("file");
        std::fs::write(&file, b"").unwrap();
        config.cache_dir = file.clone();
        config.work_dir = file;
        let r = checked(&config, &host);
        assert!(has(&r.errors, "cache_dir"), "{:?}", r.errors);
        assert!(has(&r.warnings, "same directory"), "{:?}", r.warnings);

        host.env_names = vec![
            "YANUGET_INTERNALIZE_TARGET_URL".into(),
            "YANUGET_INTERNALIZE_TARGET_APIKEY".into(),
            "YANUGET_API_KEY".into(),
        ];
        let mut r = Report::default();
        check_env_names(&host, &mut r);
        assert_eq!(r.warnings.len(), 1, "{:?}", r.warnings);
        assert!(has(&r.warnings, "TARGET_APIKEY"));
    }
}
