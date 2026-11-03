//! The `YANUGET_*` environment overlay, and the names nothing reads.

use std::net::IpAddr;
use std::path::PathBuf;

use crate::error::{Error, Result};

use super::sections::OverwriteMode;
use super::Config;

impl Config {
    pub(super) fn apply_env(&mut self) -> Result<()> {
        self.apply_env_from(|name| std::env::var(name))
    }

    /// The `YANUGET_*` names among `names` that nothing reads (`DEP-09`).
    ///
    /// The TOML file rejects a misspelled key; a misspelled variable is
    /// simply never asked for, so `YANUGET_ADMIN_APIKEY` leaves `/admin` off
    /// without a word. What `apply_env_from` asks for is the list of
    /// names it knows, feed variables included (for the feeds this
    /// configuration declares), plus the ones the command line reads.
    pub fn unknown_env_names<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let asked = std::cell::RefCell::new(std::collections::HashSet::new());
        // Every lookup misses, so no value can fail to parse and every name
        // the overlay knows is asked for.
        let _ = self.clone().apply_env_from(|name| {
            asked.borrow_mut().insert(name.to_string());
            Err(std::env::VarError::NotPresent)
        });
        let asked = asked.into_inner();
        let mut unknown: Vec<String> = names
            .into_iter()
            .filter(|n| n.starts_with("YANUGET_") && !asked.contains(*n) && !CLI_ENV.contains(n))
            .map(str::to_string)
            .collect();
        unknown.sort_unstable();
        unknown
    }

    /// [`Self::unknown_env_names`] of the process environment.
    pub fn unknown_env(&self) -> Vec<String> {
        let names: Vec<String> = std::env::vars_os()
            .filter_map(|(k, _)| k.into_string().ok())
            .collect();
        self.unknown_env_names(names.iter().map(String::as_str))
    }

    /// Overlay `YANUGET_*` values, read through `var` (the process environment
    /// in production, a map in tests).
    ///
    /// Every value that is set has to parse. Skipping one that does not, as
    /// this used to, failed *open*: `YANUGET_TLS_ENABLED=enabled` served plain
    /// HTTP, `YANUGET_MAX_PACKAGE_SIZE_BYTES=10G` meant unlimited, and a typo
    /// in `YANUGET_RATELIMIT_ENABLED` turned the limiter off — all without a
    /// word, while the TOML file rejects a misspelled key outright. A setting
    /// believed on and actually off is worse than a server that refuses to
    /// start and says why.
    pub(super) fn apply_env_from(
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
        let bool_var = |name: &str| -> Result<Option<bool>> {
            get(name)?.map(|v| parse_env_bool(name, &v)).transpose()
        };

        if let Some(v) = get("YANUGET_HOST")? {
            self.host = parse_env_host("YANUGET_HOST", &v)?;
        }
        if let Some(v) = get("YANUGET_PORT")? {
            self.port = parse_env_int("YANUGET_PORT", &v)?;
        }
        if let Some(v) = get("YANUGET_BASE_URL")? {
            self.base_url = (!v.trim().is_empty()).then(|| v.trim().to_string());
        }
        if let Some(v) = get("YANUGET_DATA_DIR")? {
            self.data_dir = PathBuf::from(v);
        }
        if let Some(v) = get("YANUGET_STORAGE_PATH")? {
            self.storage_path = Some(PathBuf::from(v));
        }
        if let Some(v) = get("YANUGET_DATABASE_PATH")? {
            self.database_path = Some(v);
        }
        // Keys also come from a file named by `{NAME}_FILE` (a systemd
        // credential, a Docker or Kubernetes secret), so none has to sit in
        // the TOML or in the unit's environment.
        let secret = |name: &str| secret_var(name, &get);
        if let Some(v) = secret("YANUGET_API_KEY")? {
            self.api_key = Some(env_key("YANUGET_API_KEY", &v)?);
        }
        if let Some(v) = secret("YANUGET_API_KEYS")? {
            self.api_keys = env_keys("YANUGET_API_KEYS", &v)?;
        }
        if let Some(v) = secret("YANUGET_ADMIN_API_KEY")? {
            self.admin_api_key = Some(env_key("YANUGET_ADMIN_API_KEY", &v)?);
        }
        self.apply_feed_env(&secret)?;
        if let Some(v) = get("YANUGET_GALLERY_PAGE_SIZE")? {
            let n: i64 = parse_env_int("YANUGET_GALLERY_PAGE_SIZE", &v)?;
            if n < 1 {
                return Err(env_error("YANUGET_GALLERY_PAGE_SIZE", "must be at least 1"));
            }
            self.gallery_page_size = n;
        }
        if let Some(v) = get("YANUGET_MAX_PACKAGE_SIZE_BYTES")? {
            self.max_package_size_bytes = parse_env_opt_int("YANUGET_MAX_PACKAGE_SIZE_BYTES", &v)?;
        }
        if let Some(v) = get("YANUGET_UPLOAD_IDLE_TIMEOUT_SECS")? {
            self.upload_idle_timeout_secs = parse_env_int("YANUGET_UPLOAD_IDLE_TIMEOUT_SECS", &v)?;
        }
        if let Some(v) = get("YANUGET_MIN_FREE_DISK_BYTES")? {
            self.min_free_disk_bytes = parse_env_int("YANUGET_MIN_FREE_DISK_BYTES", &v)?;
        }
        if let Some(v) = get("YANUGET_MAX_CONNECTIONS")? {
            self.max_connections = parse_env_int("YANUGET_MAX_CONNECTIONS", &v)?;
        }
        if let Some(v) = get("YANUGET_ALLOW_OVERWRITE")? {
            self.allow_overwrite = OverwriteMode::parse_env("YANUGET_ALLOW_OVERWRITE", &v)?;
        }
        if let Some(b) = bool_var("YANUGET_HARD_DELETE_ENABLED")? {
            self.hard_delete_enabled = b;
        }
        if let Some(b) = bool_var("YANUGET_TLS_ENABLED")? {
            self.tls_enabled = b;
        }
        if let Some(v) = get("YANUGET_TLS_CERT_PATH")? {
            self.tls_cert_path = (!v.is_empty()).then(|| PathBuf::from(v));
        }
        if let Some(v) = get("YANUGET_TLS_KEY_PATH")? {
            self.tls_key_path = (!v.is_empty()).then(|| PathBuf::from(v));
        }
        if let Some(b) = bool_var("YANUGET_ENABLE_SYMBOL_SERVER")? {
            self.enable_symbol_server = b;
        }
        if let Some(b) = bool_var("YANUGET_ENABLE_WEB_UI")? {
            self.enable_web_ui = b;
        }
        if let Some(v) = get("YANUGET_PRIMARY_CLIENT")? {
            if !v.trim().is_empty() {
                self.primary_client = v.trim().to_ascii_lowercase();
            }
        }
        if let Some(b) = bool_var("YANUGET_RETENTION_ENABLED")? {
            self.retention.enabled = b;
        }
        if let Some(b) = bool_var("YANUGET_RETENTION_PRUNE_ON_PUSH")? {
            self.retention.prune_on_push = b;
        }
        if let Some(v) = get("YANUGET_RETENTION_INTERVAL_HOURS")? {
            self.retention.interval_hours = parse_env_int("YANUGET_RETENTION_INTERVAL_HOURS", &v)?;
        }
        if let Some(v) = get("YANUGET_RETENTION_KEEP_LATEST_STABLE")? {
            self.retention.keep_latest_stable =
                parse_env_opt_int("YANUGET_RETENTION_KEEP_LATEST_STABLE", &v)?;
        }
        if let Some(v) = get("YANUGET_RETENTION_KEEP_LATEST_PRERELEASE")? {
            self.retention.keep_latest_prerelease =
                parse_env_opt_int("YANUGET_RETENTION_KEEP_LATEST_PRERELEASE", &v)?;
        }
        if let Some(v) = get("YANUGET_RETENTION_MAX_AGE_DAYS")? {
            self.retention.max_age_days = parse_env_opt_int("YANUGET_RETENTION_MAX_AGE_DAYS", &v)?;
        }
        if let Some(b) = bool_var("YANUGET_MONITORING_ENABLED")? {
            self.monitoring.enabled = b;
        }
        // A key like the others: a blank value would quietly open /metrics.
        if let Some(v) = secret("YANUGET_MONITORING_TOKEN")? {
            self.monitoring.token = Some(env_key("YANUGET_MONITORING_TOKEN", &v)?);
        }
        if let Some(b) = bool_var("YANUGET_RATELIMIT_ENABLED")? {
            self.rate_limit.enabled = b;
        }
        if let Some(v) = get("YANUGET_RATELIMIT_MAX_REQUESTS")? {
            self.rate_limit.max_requests = parse_env_int("YANUGET_RATELIMIT_MAX_REQUESTS", &v)?;
        }
        if let Some(v) = get("YANUGET_RATELIMIT_WINDOW_SECS")? {
            self.rate_limit.window_secs = parse_env_int("YANUGET_RATELIMIT_WINDOW_SECS", &v)?;
        }
        if let Some(v) = get("YANUGET_RATELIMIT_MAX_FAILED_AUTH")? {
            self.rate_limit.max_failed_auth =
                parse_env_int("YANUGET_RATELIMIT_MAX_FAILED_AUTH", &v)?;
        }
        if let Some(b) = bool_var("YANUGET_FILES_ENABLED")? {
            self.files.enabled = b;
        }
        if let Some(v) = get("YANUGET_FILES_MAX_FILE_SIZE_BYTES")? {
            self.files.max_file_size_bytes =
                parse_env_opt_int("YANUGET_FILES_MAX_FILE_SIZE_BYTES", &v)?;
        }
        if let Some(v) = get("YANUGET_FILES_ALLOWED_EXTENSIONS")? {
            self.files.allowed_extensions = v
                .split(',')
                .map(|s| s.trim().trim_start_matches('.').to_string())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(v) = get("YANUGET_FILES_UPLOAD_EXPIRY_HOURS")? {
            self.files.upload_expiry_hours =
                parse_env_int("YANUGET_FILES_UPLOAD_EXPIRY_HOURS", &v)?;
        }
        if let Some(v) = get("YANUGET_FILES_MAX_OPEN_UPLOADS")? {
            self.files.max_open_uploads = parse_env_int("YANUGET_FILES_MAX_OPEN_UPLOADS", &v)?;
        }
        if let Some(v) = get("YANUGET_FILES_INBOX_DIR")? {
            self.files.inbox_dir = (!v.trim().is_empty()).then(|| PathBuf::from(v.trim()));
        }
        if let Some(v) = get("YANUGET_FILES_INBOX_SCAN_SECS")? {
            self.files.inbox_scan_secs = parse_env_int("YANUGET_FILES_INBOX_SCAN_SECS", &v)?;
        }
        // Set (even to the empty string) this replaces the list wholesale, so an
        // operator can pin trust to their proxy — or revoke it entirely.
        if let Some(v) = get("YANUGET_TRUSTED_PROXIES")? {
            self.trusted_proxies = split_list(&v);
        }
        if let Some(v) = get("YANUGET_ALLOWED_HOSTS")? {
            self.allowed_hosts = split_list(&v);
        }
        Ok(())
    }

    /// Overlay each configured feed's keys from `YANUGET_FEED_{NAME}_*` (see
    /// [`feed_env_name`]), each also readable from a `..._FILE`.
    ///
    /// Only feeds the TOML declares are looked up: the environment overrides
    /// a feed's secrets, it does not invent feeds.
    fn apply_feed_env(&mut self, secret: &impl Fn(&str) -> Result<Option<String>>) -> Result<()> {
        // `a-b` and `a.b` share a name in the environment; a value set there
        // could be meant for either, so it is refused rather than guessed.
        let mut seen: std::collections::HashMap<String, &str> = Default::default();
        let mut ambiguous = Vec::new();
        for f in &self.feeds {
            let env = feed_env_name(&f.name);
            if let Some(other) = seen.insert(env.clone(), &f.name) {
                ambiguous.push((env, other.to_string(), f.name.clone()));
            }
        }
        for (env, a, b) in ambiguous {
            for suffix in FEED_ENV_SUFFIXES {
                let name = format!("YANUGET_FEED_{env}_{suffix}");
                if secret(&name)?.is_some() {
                    return Err(env_error(
                        &name,
                        &format!("could be meant for feed {a:?} or {b:?}; rename one of them"),
                    ));
                }
            }
        }

        for f in &mut self.feeds {
            let prefix = format!("YANUGET_FEED_{}_", feed_env_name(&f.name));
            // The value of `{prefix}{suffix}`, checked to be a key.
            let key = |suffix: &str| -> Result<Option<String>> {
                let name = format!("{prefix}{suffix}");
                secret(&name)?.map(|v| env_key(&name, &v)).transpose()
            };
            if let Some(v) = key("API_KEY")? {
                f.api_key = Some(v);
            }
            let list = format!("{prefix}API_KEYS");
            if let Some(v) = secret(&list)? {
                f.api_keys = env_keys(&list, &v)?;
            }
            if let Some(v) = key("READ_API_KEY")? {
                f.read_api_key = Some(v);
            }
            if let Some(v) = key("PREVIEW_API_KEY")? {
                f.preview_api_key = Some(v);
            }
            if let Some(v) = key("ADMIN_API_KEY")? {
                f.admin_api_key = Some(v);
            }
            if let Some(v) = key("MIRROR_PASSWORD")? {
                f.mirror.auth.password = Some(v);
            }
            if let Some(v) = key("MIRROR_TOKEN")? {
                f.mirror.auth.token = Some(v);
            }
        }
        Ok(())
    }
}

pub(super) fn env_error(name: &str, why: &str) -> Error {
    Error::BadRequest(format!("environment variable {name} {why}"))
}

/// The spellings a boolean environment variable accepts, case-insensitively.
/// Anything else is refused rather than read as `false`.
fn parse_env_bool(name: &str, v: &str) -> Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(env_error(
            name,
            &format!("must be one of 1/true/yes/on or 0/false/no/off, not {v:?}"),
        )),
    }
}

/// A plain decimal integer: digits only, so `10G`, `1e9`, `+5` and `-1` are
/// errors instead of whatever a lenient parse would make of them.
fn parse_env_int<T: std::str::FromStr>(name: &str, v: &str) -> Result<T> {
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

/// Like [`parse_env_int`], with the empty string meaning "unset".
fn parse_env_opt_int<T: std::str::FromStr>(name: &str, v: &str) -> Result<Option<T>> {
    if v.trim().is_empty() {
        Ok(None)
    } else {
        parse_env_int(name, v).map(Some)
    }
}

/// An IP address, or `localhost` for the IPv4 loopback — the one name people
/// reach for, and the one that used to be dropped for `0.0.0.0`, exposing a
/// server meant to be local to the whole network.
fn parse_env_host(name: &str, v: &str) -> Result<IpAddr> {
    let t = v.trim();
    if t.eq_ignore_ascii_case("localhost") {
        return Ok(IpAddr::from([127, 0, 0, 1]));
    }
    t.parse().map_err(|_| {
        env_error(
            name,
            &format!("must be an IP address or \"localhost\", not {v:?}"),
        )
    })
}

/// A comma-separated environment list, trimmed, empties dropped.
/// The per-feed secrets the environment can set, as `YANUGET_FEED_{NAME}_{SUFFIX}`.
const FEED_ENV_SUFFIXES: [&str; 7] = [
    "API_KEY",
    "API_KEYS",
    "READ_API_KEY",
    "PREVIEW_API_KEY",
    "ADMIN_API_KEY",
    "MIRROR_PASSWORD",
    "MIRROR_TOKEN",
];

/// A feed's name as it appears in its environment variables: upper-cased,
/// with `-` and `.` (which a variable name cannot hold) as `_`. Feed `stage`
/// is `YANUGET_FEED_STAGE_*`, feed `pre-prod` is `YANUGET_FEED_PRE_PROD_*`.
pub fn feed_env_name(feed: &str) -> String {
    feed.chars()
        .map(|c| match c {
            '-' | '.' => '_',
            c => c.to_ascii_uppercase(),
        })
        .collect()
}

/// `YANUGET_*` variables the command line reads rather than the configuration:
/// the config path, `migrate`'s source and the admin commands' server and key.
const CLI_ENV: &[&str] = &[
    "YANUGET_CONFIG",
    "YANUGET_SOURCE_USERNAME",
    "YANUGET_SOURCE_PASSWORD",
    "YANUGET_SOURCE_TOKEN",
    "YANUGET_SOURCE_HEADERS",
    "YANUGET_ADMIN_URL",
    "YANUGET_ADMIN_KEY",
];

/// A secret from the environment variable `name`, or from the file named by
/// `{name}_FILE`; setting both is refused, since only one can win and the
/// operator meant one of them.
///
/// A file's trailing newline is not part of the secret. An empty or
/// unreadable file is an error rather than "no key": for a push key that
/// would quietly open the feed to anyone.
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

/// A single key from the variable `name` (or its key file), trimmed.
///
/// A blank value is an error, not "no key": Compose writes `${UNSET}` as an
/// empty string, and reading that as a removal would quietly drop a feed's
/// read key — making it public — or its push key. A key comes out of the
/// configuration by unsetting the variable and removing it from the TOML.
fn env_key(name: &str, v: &str) -> Result<String> {
    let v = v.trim();
    if v.is_empty() {
        return Err(env_error(
            name,
            "is set but blank; give it a key, or unset it to keep the configuration file's",
        ));
    }
    Ok(v.to_string())
}

/// A key list from the variable `name` (or its key file): comma- or
/// line-separated, so a file can hold one key per line. A list with no key in
/// it is refused, as for [`env_key`].
fn env_keys(name: &str, v: &str) -> Result<Vec<String>> {
    let keys: Vec<String> = v
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if keys.is_empty() {
        return Err(env_error(
            name,
            "is set but holds no key; give it one, or unset it to keep the configuration file's",
        ));
    }
    Ok(keys)
}

fn split_list(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Combine a single optional key with a list of keys into a deduplicated,
/// non-empty list (order preserved, empties dropped).
pub(super) fn combine_keys(single: &Option<String>, list: &[String]) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for k in single
        .iter()
        .map(String::as_str)
        .chain(list.iter().map(String::as_str))
    {
        let k = k.trim();
        if !k.is_empty() && !keys.iter().any(|e| e == k) {
            keys.push(k.to_string());
        }
    }
    keys
}
