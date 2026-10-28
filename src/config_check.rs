//! `yanuget config check`: everything the server would refuse to start on, or
//! start with and then fail at, found before it starts — so a rollout (Puppet,
//! a CI job, a unit's `ExecStartPre=`) can stop on a bad configuration instead
//! of discovering it in a restart loop.
//!
//! It loads the configuration exactly as the server does (TOML, then the
//! environment, key files included), resolves the feeds, and then opens the
//! files the server would only open later: the TLS certificate and key, and
//! each mirror's CA bundle.

use crate::config::{Config, ResolvedFeed};

/// What the check found, most serious first when printed.
#[derive(Debug, Default)]
pub struct Report {
    /// The server would not start, or would start and fail.
    pub errors: Vec<String>,
    /// The server starts, but probably not as intended.
    pub warnings: Vec<String>,
    /// Worth knowing; nothing to fix.
    pub notes: Vec<String>,
}

impl Report {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Keys shorter than this are guessable online at the rate limiter's pace;
/// the server warns about them at startup, and the check reports them.
pub const MIN_KEY_CHARS: usize = 32;

/// Check the configuration at `path` (or the defaults), with the process
/// environment overlaid, as the server would load it.
pub fn check(path: Option<&str>) -> Report {
    let mut report = Report::default();
    let config = match Config::load(path) {
        Ok(c) => c,
        Err(e) => {
            report.errors.push(e.to_string());
            return report;
        }
    };
    check_loaded(&config, &mut report);
    for name in config.unknown_env() {
        report.warnings.push(format!(
            "environment variable {name} is not a setting and is ignored"
        ));
    }
    report
}

/// The checks after loading, on an already loaded configuration.
pub fn check_loaded(config: &Config, report: &mut Report) {
    let feeds = match config.resolved_feeds() {
        Ok(f) => f,
        Err(e) => {
            report.errors.push(e.to_string());
            return;
        }
    };
    for feed in &feeds {
        check_feed(feed, report);
        if config.files.enabled {
            check_file_types(feed, report);
        }
    }
    check_tls(config, report);
    if config.monitoring.enabled && config.monitoring.token.is_none() {
        report.warnings.push(
            "monitoring is enabled without a token: anyone who can reach the server \
             reads /metrics and /health/status (feed names, mirror upstreams)"
                .into(),
        );
    }

    if config.data_dir.exists() && !config.data_dir.is_dir() {
        report.errors.push(format!(
            "data_dir {} exists but is not a directory",
            config.data_dir.display()
        ));
    } else if !config.data_dir.exists() {
        report.notes.push(format!(
            "data_dir {} does not exist yet; the server creates it",
            config.data_dir.display()
        ));
    }
}

/// A file type no file name can have is a typo, and an empty list refuses
/// every file: both are legal, and both are worth a warning.
fn check_file_types(feed: &ResolvedFeed, report: &mut Report) {
    let name = &feed.name;
    if feed.allowed_extensions.is_empty() {
        report.warnings.push(format!(
            "feed {name}: no file types are allowed, so it refuses every attached file"
        ));
    }
    for ext in &feed.allowed_extensions {
        let bare = ext.trim_start_matches('.');
        if bare.is_empty()
            || !bare
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            report.warnings.push(format!(
                "feed {name}: file type {ext:?} can never match a file name \
                 (letters, digits, '_' and '-' only, without the dot)"
            ));
        }
    }
}

fn check_feed(feed: &ResolvedFeed, report: &mut Report) {
    let name = &feed.name;
    if let Some(preview) = &feed.preview_api_key {
        let reused = feed.read_api_key.as_deref() == Some(preview.as_str())
            || feed.api_keys.contains(preview)
            || feed.admin_api_key.as_deref() == Some(preview.as_str());
        if reused {
            report.warnings.push(format!(
                "feed {name}: the preview key is also another key of the feed; give it \
                 its own, or everyone holding that one can download pending versions"
            ));
        }
        if !feed.requires_approval {
            report.notes.push(format!(
                "feed {name}: a preview key without requires_approval only matters for \
                 versions promoted in while pending"
            ));
        }
    }
    if feed.allow_overwrite == crate::config::OverwriteMode::PendingOnly && !feed.requires_approval
    {
        report.warnings.push(format!(
            "feed {name}: allow_overwrite = \"pending-only\" without requires_approval; \
             only versions promoted in while pending elsewhere can be replaced"
        ));
    }
    if feed.api_keys.is_empty() {
        report.warnings.push(format!(
            "feed {name}: no push key, so push and delete are open to anyone who can reach it"
        ));
    }
    if feed.admin_api_key.is_none() {
        report.notes.push(format!(
            "feed {name}: no admin key, so it has no /admin area"
        ));
    }
    let short = |k: &str| k.chars().count() < MIN_KEY_CHARS;
    for (role, is_short) in [
        ("push", feed.api_keys.iter().any(|k| short(k))),
        ("read", feed.read_api_key.as_deref().is_some_and(short)),
        (
            "preview",
            feed.preview_api_key.as_deref().is_some_and(short),
        ),
        ("admin", feed.admin_api_key.as_deref().is_some_and(short)),
    ] {
        if is_short {
            report.warnings.push(format!(
                "feed {name}: a {role} key is shorter than {MIN_KEY_CHARS} characters"
            ));
        }
    }
    if feed.mirror.enabled {
        if let Some(ca) = &feed.mirror.ca_cert_path {
            match std::fs::read(ca) {
                Err(e) => report.errors.push(format!(
                    "feed {name}: mirror ca_cert_path {}: {e}",
                    ca.display()
                )),
                Ok(pem) => match reqwest::Certificate::from_pem_bundle(&pem) {
                    Ok(certs) if !certs.is_empty() => {}
                    Ok(_) => report.errors.push(format!(
                        "feed {name}: mirror ca_cert_path {} holds no certificate",
                        ca.display()
                    )),
                    Err(e) => report.errors.push(format!(
                        "feed {name}: mirror ca_cert_path {}: {e}",
                        ca.display()
                    )),
                },
            }
        }
    }
}

fn check_tls(config: &Config, report: &mut Report) {
    if !config.tls_enabled {
        report
            .notes
            .push("TLS is off: the server speaks plain HTTP".to_string());
        return;
    }
    let Some((cert, key)) = config.tls_pair() else {
        report.notes.push(format!(
            "no tls_cert_path/tls_key_path: the server uses a self-signed certificate under {}",
            config.data_dir.join("tls").display()
        ));
        return;
    };
    match crate::tls::check_pair(&cert, &key, &certificate_hosts(config)) {
        Err(e) => report.errors.push(e),
        Ok(pair) => {
            for host in pair.uncovered {
                report.warnings.push(format!(
                    "the TLS certificate does not name {host}; clients that reach the server \
                     by that name will refuse it (add it as a subject alternative name)"
                ));
            }
        }
    }
}

/// The names clients are configured to use: the base URL's host and every
/// allowed host. A wildcard entry names nothing to check.
fn certificate_hosts(config: &Config) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    if let Some(host) = config.base_url.as_deref().and_then(crate::tls::host_of) {
        hosts.push(host);
    }
    for h in &config.allowed_hosts {
        let h = h.trim();
        // `host:port` → `host`; a bracketed IPv6 literal keeps its colons.
        let host = match h.strip_prefix('[') {
            Some(rest) => rest.split(']').next().unwrap_or(""),
            None if h.matches(':').count() == 1 => h.split(':').next().unwrap_or(""),
            None => h,
        };
        if !host.is_empty() && host != "*" && !host.starts_with("*.") {
            hosts.push(host.to_ascii_lowercase());
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::FeedConfig;

    fn checked(config: Config) -> Report {
        let mut report = Report::default();
        check_loaded(&config, &mut report);
        report
    }

    #[test]
    fn pending_only_without_a_gate_is_warned_about() {
        use crate::config::OverwriteMode;
        let feed = |name: &str, gated: bool| FeedConfig {
            name: name.into(),
            allow_overwrite: Some(OverwriteMode::PendingOnly),
            requires_approval: gated,
            ..Default::default()
        };
        let config = Config {
            tls_enabled: false,
            feeds: vec![feed("intake", true), feed("open", false)],
            ..Config::default()
        };
        let report = checked(config);
        assert!(report.is_ok(), "{report:?}");
        let pending = |f: &str| {
            report
                .warnings
                .iter()
                .any(|w| w.starts_with(&format!("feed {f}: ")) && w.contains("pending-only"))
        };
        assert!(!pending("intake"), "{report:?}");
        assert!(pending("open"), "{report:?}");
    }

    #[test]
    fn a_feeds_own_file_types_are_checked() {
        let feed = |name: &str, exts: &[&str]| FeedConfig {
            name: name.into(),
            files: crate::config::FeedFilesConfig {
                allowed_extensions: Some(exts.iter().map(|e| e.to_string()).collect()),
            },
            ..Default::default()
        };
        let config = Config {
            tls_enabled: false,
            feeds: vec![
                feed("choco", &["exe", ".msi"]),
                feed("typo", &["exe msi", "."]),
                feed("none", &[]),
            ],
            ..Config::default()
        };
        let report = checked(config);
        assert!(report.is_ok(), "{report:?}");
        let about = |feed: &str| {
            report
                .warnings
                .iter()
                .filter(|w| w.starts_with(&format!("feed {feed}: ")) && w.contains("file"))
                .count()
        };
        assert_eq!(about("choco"), 0, "{report:?}");
        assert_eq!(about("typo"), 2, "{report:?}");
        assert_eq!(about("none"), 1, "{report:?}");
    }

    #[test]
    fn open_feeds_and_short_keys_are_warned_about() {
        let config = Config {
            tls_enabled: false,
            api_key: Some("short".into()),
            feeds: vec![
                FeedConfig {
                    name: "stage".into(),
                    ..Default::default()
                },
                FeedConfig {
                    name: "production".into(),
                    api_key: Some("p".repeat(32)),
                    ..Default::default()
                },
            ],
            ..Config::default()
        };
        let report = checked(config);
        assert!(report.is_ok(), "{report:?}");
        assert!(report
            .warnings
            .iter()
            .any(|w| w.contains("feed stage: a push key is shorter")));
        assert!(!report.warnings.iter().any(|w| w.contains("production")));
    }

    #[test]
    fn monitoring_without_a_token_is_warned_about() {
        let mut config = Config {
            tls_enabled: false,
            ..Config::default()
        };
        config.monitoring.enabled = true;
        let open = |r: &Report| r.warnings.iter().any(|w| w.contains("monitoring"));
        assert!(open(&checked(config.clone())));
        config.monitoring.token = Some("t".repeat(32));
        assert!(!open(&checked(config)));
    }

    #[test]
    fn an_unknown_promotion_target_is_an_error() {
        let config = Config {
            feeds: vec![FeedConfig {
                name: "stage".into(),
                promotes_to: Some("prod".into()),
                ..Default::default()
            }],
            ..Config::default()
        };
        let report = checked(config);
        assert!(!report.is_ok());
        assert!(report.errors[0].contains("unknown feed"), "{report:?}");
    }

    #[test]
    fn a_missing_certificate_and_a_missing_mirror_ca_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: dir.path().to_path_buf(),
            tls_cert_path: Some(dir.path().join("cert.pem")),
            tls_key_path: Some(dir.path().join("key.pem")),
            feeds: vec![FeedConfig {
                name: "production".into(),
                mirror: crate::config::MirrorConfig {
                    enabled: true,
                    upstream: "https://upstream.example.com/v3/index.json".into(),
                    ca_cert_path: Some(dir.path().join("ca.pem")),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Config::default()
        };
        let report = checked(config);
        assert_eq!(report.errors.len(), 2, "{report:?}");
        assert!(report
            .errors
            .iter()
            .any(|e| e.contains("mirror ca_cert_path")));
        assert!(report.errors.iter().any(|e| e.contains("TLS certificate")));
    }

    #[test]
    fn the_hosts_clients_use_are_the_ones_checked() {
        let config = Config {
            base_url: Some("https://nuget.example.com/".into()),
            allowed_hosts: vec![
                "NUGET".into(),
                "nuget.example.com:443".into(),
                "10.0.0.5".into(),
                "[::1]:5000".into(),
                "*".into(),
            ],
            ..Config::default()
        };
        assert_eq!(
            certificate_hosts(&config),
            vec!["10.0.0.5", "::1", "nuget", "nuget.example.com"]
        );
    }
}
