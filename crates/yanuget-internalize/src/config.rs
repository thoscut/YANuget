//! Configuration: defaults, overlaid by an optional TOML file, overlaid by
//! `YANUGET_INTERNALIZE_*` environment variables, the way the server loads
//! its own.
//!
//! The TOML file refuses unknown keys. Every environment variable that is
//! set has to parse, and a secret can come from the file named by the same
//! variable with `_FILE` appended (a systemd credential, a container secret),
//! so the push key never has to sit in the TOML or in a unit's environment.
//!
//! A variable's name is its TOML path in capitals, joined by `_`:
//! `[sandbox.kvm] capture_image` is `YANUGET_INTERNALIZE_SANDBOX_KVM_CAPTURE_IMAGE`.

use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;

/// What every environment variable of the tool starts with.
pub const ENV_PREFIX: &str = "YANUGET_INTERNALIZE_";

/// The variable `--config` falls back to. Not a setting itself.
pub const CONFIG_ENV: &str = "YANUGET_INTERNALIZE_CONFIG";

/// The Chocolatey Community Repository's v2 feed.
pub const COMMUNITY_REPOSITORY: &str = "https://community.chocolatey.org/api/v2/";

/// A configuration that cannot be used, with the reason.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

type Result<T, E = ConfigError> = std::result::Result<T, E>;

/// The whole configuration.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Upstream packages and downloaded files, kept between runs and keyed by
    /// hash.
    pub cache_dir: PathBuf,
    /// Scratch space of a run: closures handed to the sandbox, rewritten
    /// packages before they are pushed.
    pub work_dir: PathBuf,
    pub target: TargetConfig,
    pub upstream: UpstreamConfig,
    pub sandbox: SandboxConfig,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            cache_dir: PathBuf::from("./cache"),
            work_dir: PathBuf::from("./work"),
            target: TargetConfig::default(),
            upstream: UpstreamConfig::default(),
            sandbox: SandboxConfig::default(),
        }
    }
}

/// The YANuget feed internalized packages are pushed to: an intake feed with
/// `requires_approval = true`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct TargetConfig {
    /// The feed's URL, where its `v3/index.json` is. `plan` works without
    /// one; `run` does not.
    pub url: Option<String>,
    /// A push key of that feed, and nothing more.
    pub api_key: Option<Secret>,
    /// How long one request to the feed may take, a push included.
    pub timeout_secs: u64,
}

impl Default for TargetConfig {
    fn default() -> Self {
        TargetConfig {
            url: None,
            api_key: None,
            timeout_secs: 300,
        }
    }
}

/// Where packages come from.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UpstreamConfig {
    /// A v2 feed (the community repository by default) or a v3 service index.
    pub url: String,
    /// Requests to the upstream at a time. Low by default: the community
    /// repository rate-limits heavy users.
    pub concurrency: usize,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        UpstreamConfig {
            url: COMMUNITY_REPOSITORY.to_string(),
            concurrency: 2,
        }
    }
}

/// Where package scripts run.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SandboxConfig {
    pub backend: Backend,
    /// The wall-clock limit of one capture, from restore to power-off.
    pub timeout_secs: u64,
    /// Sandboxes running at once.
    pub max_parallel: usize,
    /// The most a capture may write. More is an error, not a truncation: the
    /// capture comes from untrusted code.
    pub max_capture_bytes: u64,
    pub kvm: KvmConfig,
    pub windows_container: WindowsContainerConfig,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            backend: Backend::Kvm,
            timeout_secs: 1800,
            max_parallel: 1,
            max_capture_bytes: 4 * 1024 * 1024,
            kvm: KvmConfig::default(),
            windows_container: WindowsContainerConfig::default(),
        }
    }
}

/// The sandbox backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    /// A Windows Server Core guest under KVM/QEMU on a Linux host.
    Kvm,
    /// A Windows container under Hyper-V isolation on a Windows host.
    WindowsContainer,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Kvm => "kvm",
            Backend::WindowsContainer => "windows-container",
        }
    }

    fn parse(name: &str, v: &str) -> Result<Self> {
        match v.trim() {
            "kvm" => Ok(Backend::Kvm),
            "windows-container" => Ok(Backend::WindowsContainer),
            _ => Err(env_error(
                name,
                &format!("must be \"kvm\" or \"windows-container\", not {v:?}"),
            )),
        }
    }
}

/// The KVM backend.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct KvmConfig {
    /// The QEMU binary: a path, or a name looked up in `PATH`.
    pub qemu: PathBuf,
    /// The capture image: the stock image plus the capture helpers.
    pub capture_image: Option<PathBuf>,
    /// The stock image, for validation installs. Optional until those exist.
    pub stock_image: Option<PathBuf>,
    pub memory_mib: u32,
    pub cpus: u32,
}

impl Default for KvmConfig {
    fn default() -> Self {
        KvmConfig {
            qemu: PathBuf::from("qemu-system-x86_64"),
            capture_image: None,
            stock_image: None,
            memory_mib: 4096,
            cpus: 2,
        }
    }
}

/// The Windows container backend.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct WindowsContainerConfig {
    /// The image, pinned by digest (`name@sha256:…`).
    pub image: Option<String>,
}

/// A value that must not show up in logs or `Debug` output.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(\"…\")")
    }
}

/// Every variable the tool reads, for spotting a misspelled one.
pub const KNOWN_ENV: &[&str] = &[
    CONFIG_ENV,
    "YANUGET_INTERNALIZE_CACHE_DIR",
    "YANUGET_INTERNALIZE_WORK_DIR",
    "YANUGET_INTERNALIZE_TARGET_URL",
    "YANUGET_INTERNALIZE_TARGET_API_KEY",
    "YANUGET_INTERNALIZE_TARGET_API_KEY_FILE",
    "YANUGET_INTERNALIZE_TARGET_TIMEOUT_SECS",
    "YANUGET_INTERNALIZE_UPSTREAM_URL",
    "YANUGET_INTERNALIZE_UPSTREAM_CONCURRENCY",
    "YANUGET_INTERNALIZE_SANDBOX_BACKEND",
    "YANUGET_INTERNALIZE_SANDBOX_TIMEOUT_SECS",
    "YANUGET_INTERNALIZE_SANDBOX_MAX_PARALLEL",
    "YANUGET_INTERNALIZE_SANDBOX_MAX_CAPTURE_BYTES",
    "YANUGET_INTERNALIZE_SANDBOX_KVM_QEMU",
    "YANUGET_INTERNALIZE_SANDBOX_KVM_CAPTURE_IMAGE",
    "YANUGET_INTERNALIZE_SANDBOX_KVM_STOCK_IMAGE",
    "YANUGET_INTERNALIZE_SANDBOX_KVM_MEMORY_MIB",
    "YANUGET_INTERNALIZE_SANDBOX_KVM_CPUS",
    "YANUGET_INTERNALIZE_SANDBOX_WINDOWS_CONTAINER_IMAGE",
];

impl Config {
    /// Load the configuration: defaults, the TOML file at `path` if there is
    /// one, then the process environment.
    pub fn load(path: Option<&str>) -> Result<Self> {
        Self::load_with(path, |name| std::env::var(name))
    }

    /// [`Config::load`] with the environment read through `var`.
    pub fn load_with(
        path: Option<&str>,
        var: impl Fn(&str) -> std::result::Result<String, std::env::VarError>,
    ) -> Result<Self> {
        let mut config = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .map_err(|e| ConfigError(format!("cannot read config {p}: {e}")))?;
                Self::from_toml(&text).map_err(|e| ConfigError(format!("config {p}: {e}")))?
            }
            None => Config::default(),
        };
        config.apply_env(var)?;
        config.validate()?;
        Ok(config)
    }

    /// Parse a TOML document over the defaults, without the environment or
    /// validation.
    pub fn from_toml(text: &str) -> Result<Self> {
        toml::from_str(text).map_err(|e| ConfigError(format!("invalid TOML: {e}")))
    }

    fn apply_env(
        &mut self,
        var: impl Fn(&str) -> std::result::Result<String, std::env::VarError>,
    ) -> Result<()> {
        let get = |name: &str| -> Result<Option<String>> {
            match var(name) {
                Ok(v) => Ok(Some(v)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => {
                    Err(env_error(name, "is not valid UTF-8"))
                }
            }
        };
        let path = |v: String| PathBuf::from(v.trim());
        let opt_path = |v: String| (!v.trim().is_empty()).then(|| PathBuf::from(v.trim()));
        let opt_str = |v: String| (!v.trim().is_empty()).then(|| v.trim().to_string());

        if let Some(v) = get("YANUGET_INTERNALIZE_CACHE_DIR")? {
            self.cache_dir = path(v);
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_WORK_DIR")? {
            self.work_dir = path(v);
        }

        if let Some(v) = get("YANUGET_INTERNALIZE_TARGET_URL")? {
            self.target.url = opt_str(v);
        }
        if let Some(v) = secret_var("YANUGET_INTERNALIZE_TARGET_API_KEY", &get)? {
            self.target.api_key = opt_str(v).map(Secret);
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_TARGET_TIMEOUT_SECS")? {
            self.target.timeout_secs = parse_int("YANUGET_INTERNALIZE_TARGET_TIMEOUT_SECS", &v)?;
        }

        if let Some(v) = get("YANUGET_INTERNALIZE_UPSTREAM_URL")? {
            self.upstream.url = v.trim().to_string();
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_UPSTREAM_CONCURRENCY")? {
            self.upstream.concurrency = parse_int("YANUGET_INTERNALIZE_UPSTREAM_CONCURRENCY", &v)?;
        }

        let s = &mut self.sandbox;
        if let Some(v) = get("YANUGET_INTERNALIZE_SANDBOX_BACKEND")? {
            s.backend = Backend::parse("YANUGET_INTERNALIZE_SANDBOX_BACKEND", &v)?;
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_SANDBOX_TIMEOUT_SECS")? {
            s.timeout_secs = parse_int("YANUGET_INTERNALIZE_SANDBOX_TIMEOUT_SECS", &v)?;
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_SANDBOX_MAX_PARALLEL")? {
            s.max_parallel = parse_int("YANUGET_INTERNALIZE_SANDBOX_MAX_PARALLEL", &v)?;
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_SANDBOX_MAX_CAPTURE_BYTES")? {
            s.max_capture_bytes = parse_int("YANUGET_INTERNALIZE_SANDBOX_MAX_CAPTURE_BYTES", &v)?;
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_SANDBOX_KVM_QEMU")? {
            s.kvm.qemu = path(v);
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_SANDBOX_KVM_CAPTURE_IMAGE")? {
            s.kvm.capture_image = opt_path(v);
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_SANDBOX_KVM_STOCK_IMAGE")? {
            s.kvm.stock_image = opt_path(v);
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_SANDBOX_KVM_MEMORY_MIB")? {
            s.kvm.memory_mib = parse_int("YANUGET_INTERNALIZE_SANDBOX_KVM_MEMORY_MIB", &v)?;
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_SANDBOX_KVM_CPUS")? {
            s.kvm.cpus = parse_int("YANUGET_INTERNALIZE_SANDBOX_KVM_CPUS", &v)?;
        }
        if let Some(v) = get("YANUGET_INTERNALIZE_SANDBOX_WINDOWS_CONTAINER_IMAGE")? {
            s.windows_container.image = opt_str(v);
        }
        Ok(())
    }

    /// Refuse what no run could use, and normalize the URLs to end in `/`.
    /// What depends on the host (the KVM device, the images, the target
    /// answering) is `config check`'s business.
    pub fn validate(&mut self) -> Result<()> {
        if let Some(url) = &self.target.url {
            self.target.url = Some(normalize_url("target.url", url)?);
        }
        self.upstream.url = normalize_url("upstream.url", &self.upstream.url)?;
        if self
            .target
            .api_key
            .as_ref()
            .is_some_and(|k| k.expose().trim().is_empty())
        {
            return Err(ConfigError(
                "target.api_key is empty; leave it out instead".into(),
            ));
        }
        for (name, value) in [
            ("target.timeout_secs", self.target.timeout_secs),
            ("upstream.concurrency", self.upstream.concurrency as u64),
            ("sandbox.timeout_secs", self.sandbox.timeout_secs),
            ("sandbox.max_parallel", self.sandbox.max_parallel as u64),
            ("sandbox.max_capture_bytes", self.sandbox.max_capture_bytes),
            ("sandbox.kvm.memory_mib", self.sandbox.kvm.memory_mib as u64),
            ("sandbox.kvm.cpus", self.sandbox.kvm.cpus as u64),
        ] {
            if value == 0 {
                return Err(ConfigError(format!("{name} must be at least 1")));
            }
        }
        if self.sandbox.kvm.qemu.as_os_str().is_empty() {
            return Err(ConfigError("sandbox.kvm.qemu is empty".into()));
        }
        Ok(())
    }
}

/// An `http` or `https` URL with a host and no credentials, query or
/// fragment, ending in `/` so relative paths join under it.
fn normalize_url(name: &str, raw: &str) -> Result<String> {
    let url = reqwest::Url::parse(raw.trim())
        .map_err(|e| ConfigError(format!("{name} {raw:?} is not a URL: {e}")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(ConfigError(format!(
            "{name} {raw:?} must be an http or https URL"
        )));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(ConfigError(format!("{name} {raw:?} has no host")));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(ConfigError(format!(
            "{name} must not carry credentials; use the key settings instead"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(ConfigError(format!(
            "{name} {raw:?} must not have a query or a fragment"
        )));
    }
    let mut s = url.to_string();
    if !s.ends_with('/') {
        s.push('/');
    }
    Ok(s)
}

fn env_error(name: &str, why: &str) -> ConfigError {
    ConfigError(format!("environment variable {name} {why}"))
}

/// A plain decimal integer: digits only, so `10G`, `1e9` and `-1` are errors
/// rather than whatever a lenient parse makes of them.
fn parse_int<T: std::str::FromStr>(name: &str, v: &str) -> Result<T> {
    let t = v.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(env_error(
            name,
            &format!("must be a plain whole number, not {v:?}"),
        ));
    }
    t.parse()
        .map_err(|_| env_error(name, &format!("is out of range: {v:?}")))
}

/// A secret from the variable `name`, or from the file named by
/// `{name}_FILE`. Both set is refused, since only one can win. A file's
/// trailing newline is not part of the secret; an empty or unreadable file
/// is an error rather than "no key".
fn secret_var(name: &str, get: &impl Fn(&str) -> Result<Option<String>>) -> Result<Option<String>> {
    let file_var = format!("{name}_FILE");
    match (get(name)?, get(&file_var)?) {
        (Some(_), Some(_)) => Err(env_error(
            name,
            &format!("and {file_var} are both set; set only one"),
        )),
        (Some(v), None) => Ok(Some(v)),
        (None, Some(path)) => {
            let raw = std::fs::read_to_string(&path).map_err(|e| {
                env_error(
                    &file_var,
                    &format!("names {path:?}, which cannot be read: {e}"),
                )
            })?;
            let value = raw.trim_end_matches(['\r', '\n']);
            if value.trim().is_empty() {
                return Err(env_error(
                    &file_var,
                    &format!("names {path:?}, which is empty"),
                ));
            }
            Ok(Some(value.to_string()))
        }
        (None, None) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(toml: Option<&str>, vars: &[(&str, &str)]) -> Result<Config> {
        let dir = tempfile::tempdir().unwrap();
        let path = toml.map(|t| {
            let p = dir.path().join("internalize.toml");
            std::fs::write(&p, t).unwrap();
            p.to_str().unwrap().to_string()
        });
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::load_with(path.as_deref(), |name| {
            vars.get(name)
                .cloned()
                .ok_or(std::env::VarError::NotPresent)
        })
    }

    #[test]
    fn defaults_load_and_point_at_the_community_repository() {
        let c = load(None, &[]).unwrap();
        assert_eq!(c.upstream.url, COMMUNITY_REPOSITORY);
        assert_eq!(c.upstream.concurrency, 2);
        assert_eq!(c.sandbox.backend, Backend::Kvm);
        assert_eq!(c.target.url, None);
    }

    #[test]
    fn toml_is_overlaid_by_the_environment() {
        let c = load(
            Some(
                r#"
                cache_dir = "/var/cache/i"
                [target]
                url = "https://nuget.example.com/intake"
                api_key = "from-toml"
                [sandbox]
                backend = "windows-container"
                [sandbox.kvm]
                capture_image = "/img/capture.qcow2"
                cpus = 4
                "#,
            ),
            &[
                ("YANUGET_INTERNALIZE_TARGET_API_KEY", "from-env"),
                ("YANUGET_INTERNALIZE_SANDBOX_BACKEND", "kvm"),
                ("YANUGET_INTERNALIZE_SANDBOX_KVM_CPUS", "8"),
            ],
        )
        .unwrap();
        assert_eq!(c.cache_dir, PathBuf::from("/var/cache/i"));
        assert_eq!(
            c.target.url.as_deref(),
            Some("https://nuget.example.com/intake/")
        );
        assert_eq!(c.target.api_key.as_ref().unwrap().expose(), "from-env");
        assert_eq!(c.sandbox.backend, Backend::Kvm);
        assert_eq!(c.sandbox.kvm.cpus, 8);
        assert_eq!(
            c.sandbox.kvm.capture_image,
            Some(PathBuf::from("/img/capture.qcow2"))
        );
    }

    #[test]
    fn unknown_keys_and_bad_values_are_refused() {
        let e = load(
            Some("[target]\nurl = \"https://a/\"\npush_key = \"x\"\n"),
            &[],
        )
        .unwrap_err();
        assert!(e.0.contains("push_key"), "{e}");
        let e = load(Some("[sandbox]\nbackend = \"docker\"\n"), &[]).unwrap_err();
        assert!(e.0.contains("docker"), "{e}");
        for (var, value) in [
            ("YANUGET_INTERNALIZE_UPSTREAM_CONCURRENCY", "2x"),
            ("YANUGET_INTERNALIZE_SANDBOX_TIMEOUT_SECS", "-1"),
            ("YANUGET_INTERNALIZE_SANDBOX_BACKEND", "hyperv"),
            ("YANUGET_INTERNALIZE_UPSTREAM_CONCURRENCY", "0"),
        ] {
            let e = load(None, &[(var, value)]).unwrap_err();
            assert!(
                e.0.contains(var) || e.0.contains("concurrency"),
                "{var}={value}: {e}"
            );
        }
    }

    #[test]
    fn urls_are_checked_and_end_in_a_slash() {
        for bad in [
            "ftp://example.com/",
            "not a url",
            "https://user:pw@example.com/",
            "https://example.com/feed?x=1",
        ] {
            assert!(
                load(None, &[("YANUGET_INTERNALIZE_TARGET_URL", bad)]).is_err(),
                "{bad}"
            );
        }
        let c = load(
            None,
            &[(
                "YANUGET_INTERNALIZE_UPSTREAM_URL",
                "https://feed.example/v3/index.json",
            )],
        )
        .unwrap();
        assert_eq!(c.upstream.url, "https://feed.example/v3/index.json/");
        // A blank variable clears an optional value from the TOML.
        let c = load(
            Some("[target]\nurl = \"https://a.example/\"\n"),
            &[("YANUGET_INTERNALIZE_TARGET_URL", " ")],
        )
        .unwrap();
        assert_eq!(c.target.url, None);
    }

    #[test]
    fn the_push_key_comes_from_a_file_but_not_from_both() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key");
        std::fs::write(&key, "s3cret\n").unwrap();
        let empty = dir.path().join("empty");
        std::fs::write(&empty, "\n").unwrap();
        let key = key.to_str().unwrap();

        let c = load(None, &[("YANUGET_INTERNALIZE_TARGET_API_KEY_FILE", key)]).unwrap();
        assert_eq!(c.target.api_key.unwrap().expose(), "s3cret");

        let e = load(
            None,
            &[
                ("YANUGET_INTERNALIZE_TARGET_API_KEY_FILE", key),
                ("YANUGET_INTERNALIZE_TARGET_API_KEY", "other"),
            ],
        )
        .unwrap_err();
        assert!(e.0.contains("both set"), "{e}");
        for path in [empty.to_str().unwrap(), "/nonexistent/key"] {
            assert!(load(None, &[("YANUGET_INTERNALIZE_TARGET_API_KEY_FILE", path)]).is_err());
        }
    }

    #[test]
    fn a_secret_never_shows_in_debug_output() {
        let c = load(
            None,
            &[("YANUGET_INTERNALIZE_TARGET_API_KEY", "s3cret-value")],
        )
        .unwrap();
        assert!(!format!("{c:?}").contains("s3cret-value"));
    }

    #[test]
    fn every_variable_read_is_known() {
        // A variable `apply_env` reads that KNOWN_ENV lacks would be reported
        // as a typo by the check; one KNOWN_ENV lists that nothing reads
        // would hide a real typo.
        let source = include_str!("config.rs");
        let body = source
            .split("fn apply_env(")
            .nth(1)
            .and_then(|s| s.split("pub fn validate").next())
            .unwrap();
        let mut read = std::collections::BTreeSet::new();
        for piece in body.split('"').skip(1).step_by(2) {
            if piece.starts_with(ENV_PREFIX) {
                assert!(KNOWN_ENV.contains(&piece), "{piece} is not in KNOWN_ENV");
                read.insert(piece.to_string());
                if piece.ends_with("_API_KEY") {
                    read.insert(format!("{piece}_FILE"));
                }
            }
        }
        read.insert(CONFIG_ENV.to_string());
        let known: std::collections::BTreeSet<String> =
            KNOWN_ENV.iter().map(|s| s.to_string()).collect();
        assert_eq!(read, known);
    }
}
