//! The report on each package of a run (`INT-21`): what became of it, why,
//! and what was downloaded for it.
//!
//! A [`Draft`] gathers a package's report while the run goes; the schema it
//! fills is `yanuget_pkg::report`, the one the server reads from inside a
//! package. A run prints its reports as text for people ([`text`]) or as one
//! JSON document for pipelines ([`json`]).

use std::fmt::Write;

use serde::Serialize;
use yanuget_pkg::report::{
    Download, Environment, PackageRef, Report, Status, Upstream, SCHEMA_VERSION,
};

use crate::outcome::Outcome;

/// One package's report while the run fills it in.
#[derive(Debug, Clone)]
pub struct Draft {
    report: Report,
    /// Why the package failed: a download, the sandbox, the feed.
    failures: Vec<String>,
    /// Why a person has to provide the package.
    overrides: Vec<String>,
}

impl Draft {
    pub fn new(package: PackageRef, upstream: Upstream) -> Self {
        Draft {
            report: Report {
                schema: SCHEMA_VERSION,
                package,
                status: Status::Unchanged,
                reasons: Vec::new(),
                upstream,
                environment: None,
                downloads: Vec::new(),
                warnings: Vec::new(),
                prelude: None,
                generated_at: None,
            },
            failures: Vec::new(),
            overrides: Vec::new(),
        }
    }

    /// The package this draft is about.
    pub fn package(&self) -> &PackageRef {
        &self.report.package
    }

    /// Whether this draft is about `id` `version` (the id case-insensitively,
    /// the version normalized).
    pub fn is(&self, id: &str, version: &str) -> bool {
        self.report.package.id.eq_ignore_ascii_case(id) && self.report.package.version == version
    }

    /// What the scripts ran in. The tool's own version is added here.
    pub fn environment(&mut self, environment: Environment) {
        self.report.environment = Some(Environment {
            tool_version: Some(env!("CARGO_PKG_VERSION").to_string()),
            ..environment
        });
    }

    /// The package failed, for `why`.
    pub fn fail(&mut self, why: impl Into<String>) {
        push_once(&mut self.failures, why.into());
    }

    /// The package needs an override, for `why`.
    pub fn needs_override(&mut self, why: impl Into<String>) {
        push_once(&mut self.overrides, why.into());
    }

    pub fn warn(&mut self, warning: impl Into<String>) {
        push_once(&mut self.report.warnings, warning.into());
    }

    /// A captured download, in the order the scripts asked for it.
    pub fn download(&mut self, download: Download) {
        self.report.downloads.push(download);
    }

    /// The package entry holding the prelude.
    pub fn prelude(&mut self, entry: impl Into<String>) {
        self.report.prelude = Some(entry.into());
    }

    /// The status as things stand: a failure over an override, an override
    /// over the rest, and no downloads is `unchanged`.
    pub fn status(&self) -> Status {
        if !self.failures.is_empty() {
            Status::Failed
        } else if !self.overrides.is_empty() {
            Status::NeedsOverride
        } else if self.report.downloads.is_empty() {
            Status::Unchanged
        } else {
            Status::Internalized
        }
    }

    /// The finished report, stamped with `generated_at` (RFC 3339). The
    /// reasons list the failures first, then what needs an override.
    pub fn finish(&self, generated_at: impl Into<String>) -> Report {
        Report {
            status: self.status(),
            reasons: self
                .failures
                .iter()
                .chain(&self.overrides)
                .cloned()
                .collect(),
            generated_at: Some(generated_at.into()),
            ..self.report.clone()
        }
    }
}

fn push_once(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

/// The current time as RFC 3339, for [`Draft::finish`].
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// The reports of a run as one JSON document: the run's outcome, as its exit
/// code says it, and each package's report as the package carries it.
pub fn json(reports: &[Report]) -> Vec<u8> {
    #[derive(Serialize)]
    struct Run<'a> {
        outcome: &'static str,
        packages: &'a [Report],
    }
    let outcome = Outcome::of_all(reports.iter().map(|r| r.status));
    // Serializing these types cannot fail: every map key is a string.
    serde_json::to_vec_pretty(&Run {
        outcome: outcome.as_str(),
        packages: reports,
    })
    .expect("a run serializes")
}

/// One report as text for people: the status line, then where the package
/// came from and ran, the reasons, each download and the warnings.
pub fn text(report: &Report) -> String {
    let mut out = String::new();
    let r = report;
    let _ = writeln!(
        out,
        "{} {}: {}",
        r.package.id,
        r.package.version,
        r.status.as_str()
    );
    let _ = write!(out, "  from {}", r.upstream.source);
    match &r.upstream.sha512 {
        Some(sha) => {
            let _ = writeln!(out, " (sha512 {})", abbreviate(sha));
        }
        None => out.push('\n'),
    }
    if let Some(env) = &r.environment {
        let mut parts = Vec::new();
        if let Some(image) = &env.image_sha256 {
            parts.push(format!("image sha256 {}", abbreviate(image)));
        }
        if let Some(choco) = &env.chocolatey_version {
            parts.push(format!("Chocolatey {choco}"));
        }
        if !parts.is_empty() {
            let _ = writeln!(out, "  ran in {}", parts.join(", "));
        }
    }
    for reason in &r.reasons {
        let _ = writeln!(out, "  reason: {reason}");
    }
    for d in &r.downloads {
        let _ = writeln!(
            out,
            "  {}{}",
            d.helper
                .as_deref()
                .map_or(String::new(), |h| format!("{h} ")),
            d.url
        );
        let mut parts = vec![d.file.clone().unwrap_or_else(|| "not attached".into())];
        if let Some(size) = d.size {
            parts.push(bytes(size));
        }
        if let Some(sha) = &d.sha256 {
            parts.push(format!("sha256 {}", abbreviate(sha)));
        }
        parts.push(format!(
            "checksum {}{}",
            d.checksum.status.as_str(),
            d.checksum
                .algorithm
                .as_deref()
                .map_or(String::new(), |a| format!(" ({a})"))
        ));
        let _ = writeln!(out, "    {}", parts.join(", "));
    }
    if let Some(prelude) = &r.prelude {
        let _ = writeln!(out, "  prelude: {prelude}");
    }
    for w in &r.warnings {
        let _ = writeln!(out, "  warning: {w}");
    }
    out
}

/// The first 12 characters of a hash, which tell files apart on screen.
fn abbreviate(hash: &str) -> String {
    match hash.char_indices().nth(12) {
        Some((i, _)) => format!("{}…", &hash[..i]),
        None => hash.to_string(),
    }
}

/// A size as people read it.
fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if n < 1024 {
        return format!("{n} bytes");
    }
    let mut value = n as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use yanuget_pkg::report::{Checksum, ChecksumStatus};

    fn draft() -> Draft {
        Draft::new(
            PackageRef {
                id: "git.install".into(),
                version: "2.51.0".into(),
            },
            Upstream {
                source: "https://community.chocolatey.org/api/v2/".into(),
                sha512: Some("q1w2e3r4t5y6u7i8o9p0==".into()),
            },
        )
    }

    fn download(file: Option<&str>) -> Download {
        Download {
            url: "https://github.com/git-for-windows/git/releases/download/v2.51.0.windows.1/Git-2.51.0-64-bit.exe".into(),
            helper: Some("Install-ChocolateyPackage".into()),
            file: file.map(String::from),
            size: Some(63_123_456),
            sha256: Some("ab".repeat(32)),
            checksum: Checksum {
                status: ChecksumStatus::Verified,
                algorithm: Some("sha256".into()),
            },
        }
    }

    #[test]
    fn the_status_follows_what_happened() {
        let mut d = draft();
        assert_eq!(d.status(), Status::Unchanged);
        d.download(download(Some("Git-2.51.0-64-bit.exe")));
        assert_eq!(d.status(), Status::Internalized);
        d.needs_override("a download from inside the extension chocolatey-core");
        assert_eq!(d.status(), Status::NeedsOverride);
        d.fail("https://example.com/x.exe: answered 404 Not Found");
        d.fail("https://example.com/x.exe: answered 404 Not Found");
        let r = d.finish("2026-10-19T12:00:00Z");
        assert_eq!(r.status, Status::Failed);
        // Failures first, each once.
        assert_eq!(
            r.reasons,
            [
                "https://example.com/x.exe: answered 404 Not Found",
                "a download from inside the extension chocolatey-core"
            ]
        );
        assert_eq!(r.generated_at.as_deref(), Some("2026-10-19T12:00:00Z"));
    }

    #[test]
    fn a_report_reads_back_as_the_server_reads_it() {
        let mut d = draft();
        d.environment(Environment {
            image_sha256: Some("00ff".repeat(16)),
            chocolatey_version: Some("2.4.3".into()),
            tool_version: None,
        });
        d.download(download(Some("Git-2.51.0-64-bit.exe")));
        d.warn("the feed served the version at once");
        let r = d.finish(now());
        let back = Report::from_slice(&r.to_vec()).unwrap();
        assert_eq!(back, r);
        assert_eq!(
            back.environment.unwrap().tool_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        assert!(chrono::DateTime::parse_from_rfc3339(r.generated_at.as_deref().unwrap()).is_ok());
    }

    #[test]
    fn text_shows_everything_a_reviewer_needs() {
        let mut d = draft();
        d.environment(Environment {
            image_sha256: Some("00ff".repeat(16)),
            chocolatey_version: Some("2.4.3".into()),
            tool_version: None,
        });
        d.download(download(Some("Git-2.51.0-64-bit.exe")));
        d.download(Download {
            file: None,
            size: None,
            sha256: None,
            checksum: Checksum {
                status: ChecksumStatus::TrustOnFirstUse,
                algorithm: None,
            },
            ..download(None)
        });
        d.needs_override("setup.bin: the feed takes no .bin files");
        d.warn("something to know");
        let text = text(&d.finish("2026-10-19T12:00:00Z"));
        let expected = "\
git.install 2.51.0: needs-override
  from https://community.chocolatey.org/api/v2/ (sha512 q1w2e3r4t5y6…)
  ran in image sha256 00ff00ff00ff…, Chocolatey 2.4.3
  reason: setup.bin: the feed takes no .bin files
  Install-ChocolateyPackage https://github.com/git-for-windows/git/releases/download/v2.51.0.windows.1/Git-2.51.0-64-bit.exe
    Git-2.51.0-64-bit.exe, 60.2 MiB, sha256 abababababab…, checksum verified (sha256)
  Install-ChocolateyPackage https://github.com/git-for-windows/git/releases/download/v2.51.0.windows.1/Git-2.51.0-64-bit.exe
    not attached, checksum trust-on-first-use
  warning: something to know
";
        assert_eq!(text, expected);
    }

    #[test]
    fn json_carries_the_outcome_and_the_reports() {
        let mut ok = draft();
        ok.download(download(Some("Git-2.51.0-64-bit.exe")));
        let mut attention = draft();
        attention.needs_override("the install exited with code 1");
        let reports = [ok.finish(now()), attention.finish(now())];
        let doc: serde_json::Value = serde_json::from_slice(&json(&reports)).unwrap();
        assert_eq!(doc["outcome"], "needs-attention");
        assert_eq!(doc["packages"][0]["status"], "internalized");
        assert_eq!(doc["packages"][1]["status"], "needs-override");
        assert_eq!(doc["packages"][1]["schema"], 1);
        let doc: serde_json::Value = serde_json::from_slice(&json(&[])).unwrap();
        assert_eq!(doc["outcome"], "done");
    }

    #[test]
    fn sizes_and_hashes_are_short() {
        assert_eq!(bytes(512), "512 bytes");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(8 * 1024 * 1024 * 1024), "8.0 GiB");
        assert_eq!(abbreviate("abc"), "abc");
        assert_eq!(abbreviate(&"a".repeat(64)), format!("{}…", "a".repeat(12)));
    }
}
