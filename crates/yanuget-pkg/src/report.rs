//! The internalizer report: what was done to a package, carried inside it as
//! [`REPORT_PATH`] so promotion, copies, migrations and backups keep it along
//! with the package hash that covers it.
//!
//! The tool that writes it and the server that shows it share this one
//! definition. The schema is versioned by [`Report::schema`]: a reader takes
//! only the versions it knows ([`SCHEMA_VERSION`]) and ignores fields it does
//! not know, so a version can grow without a new number. Enumerations read
//! values they do not know as `Unknown` rather than failing, for the same
//! reason.

use serde::{Deserialize, Serialize};

/// Where the report sits inside the package.
pub const REPORT_PATH: &str = "internalizer/report.json";

/// The schema version this crate writes and reads.
pub const SCHEMA_VERSION: u32 = 1;

/// The most a reader should take of a report. A report lists a package's
/// downloads and warnings; a megabyte is far more than a real one needs.
pub const MAX_REPORT_BYTES: u64 = 1024 * 1024;

/// One package's report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    /// The schema version; [`SCHEMA_VERSION`] for what this crate writes.
    pub schema: u32,
    pub package: PackageRef,
    pub status: Status,
    /// Why the status is what it is, for anything but `internalized`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    pub upstream: Upstream,
    /// What the scripts ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Environment>,
    /// Every captured download, in the order the scripts asked for them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub downloads: Vec<Download>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// The overrides a person set for the package that were used, one line
    /// each. Added to schema version 1 after YANuget 0.8.0, which ignores it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<String>,
    /// The package entry holding the prelude that points the install scripts
    /// at the internal URLs, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prelude: Option<String>,
    /// When the report was written (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
}

/// The package a report is about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageRef {
    pub id: String,
    pub version: String,
}

/// What became of the package.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Status {
    /// Every download was captured and is attached.
    Internalized,
    /// Nothing to download: an embedded or a meta package.
    Unchanged,
    /// The scripts did something the capture cannot follow; a person has to
    /// provide the package.
    NeedsOverride,
    Failed,
    /// A status a later schema added.
    #[serde(other)]
    Unknown,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Internalized => "internalized",
            Status::Unchanged => "unchanged",
            Status::NeedsOverride => "needs-override",
            Status::Failed => "failed",
            Status::Unknown => "unknown",
        }
    }
}

/// Where the package came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Upstream {
    /// The source's service index or feed URL.
    pub source: String,
    /// The upstream package's SHA-512, base64 as NuGet publishes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha512: Option<String>,
}

/// What the install scripts ran in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Environment {
    /// The golden image's SHA-256, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chocolatey_version: Option<String>,
    /// The internalizer's own version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_version: Option<String>,
}

/// One captured download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Download {
    /// The URL the script asked for, as it asked for it.
    pub url: String,
    /// The helper that asked, e.g. `Install-ChocolateyPackage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub helper: Option<String>,
    /// The attached file's name; absent when the file was not attached (the
    /// feed refuses its type, say), which the warnings then explain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// The file's SHA-256, lower-case hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub checksum: Checksum,
}

/// Whether the script's own checksum vouched for the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checksum {
    pub status: ChecksumStatus,
    /// The algorithm the script declared, e.g. `sha256`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub algorithm: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ChecksumStatus {
    /// The script declared a SHA-256 or SHA-512 checksum, and it matched.
    Verified,
    /// No checksum, or only MD5 or SHA-1: whatever the URL served on the day
    /// of the capture is what was pinned.
    TrustOnFirstUse,
    /// A status a later schema added.
    #[serde(other)]
    Unknown,
}

impl ChecksumStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ChecksumStatus::Verified => "verified",
            ChecksumStatus::TrustOnFirstUse => "trust-on-first-use",
            ChecksumStatus::Unknown => "unknown",
        }
    }
}

/// Why a report could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error("the report is larger than {} KiB", MAX_REPORT_BYTES / 1024)]
    TooLarge,
    #[error("the report is not valid JSON for its schema: {0}")]
    Invalid(String),
    #[error("the report has schema version {0}, and this server reads version {SCHEMA_VERSION}")]
    UnsupportedSchema(u32),
}

impl Report {
    /// Parse a report, refusing one over [`MAX_REPORT_BYTES`] or of a schema
    /// version this crate does not read.
    pub fn from_slice(bytes: &[u8]) -> Result<Report, ReportError> {
        if bytes.len() as u64 > MAX_REPORT_BYTES {
            return Err(ReportError::TooLarge);
        }
        // The version first, so a later schema is named as such rather than
        // reported as whatever field it changed.
        #[derive(Deserialize)]
        struct Versioned {
            schema: u32,
        }
        let Versioned { schema } =
            serde_json::from_slice(bytes).map_err(|e| ReportError::Invalid(e.to_string()))?;
        if schema != SCHEMA_VERSION {
            return Err(ReportError::UnsupportedSchema(schema));
        }
        serde_json::from_slice(bytes).map_err(|e| ReportError::Invalid(e.to_string()))
    }

    /// The report as written into a package.
    pub fn to_vec(&self) -> Vec<u8> {
        // Serializing these types cannot fail: every map key is a string.
        serde_json::to_vec_pretty(self).expect("a report serializes")
    }

    /// The files the report says are attached, in report order.
    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.downloads.iter().filter_map(|d| d.file.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
        "schema": 1,
        "package": {"id": "7zip.install", "version": "24.8.0"},
        "status": "internalized",
        "upstream": {"source": "https://community.chocolatey.org/api/v2/", "sha512": "abc=="},
        "environment": {"image_sha256": "00ff", "chocolatey_version": "2.4.3", "tool_version": "0.1.0"},
        "downloads": [
            {"url": "https://www.7-zip.org/a/7z2408.exe", "helper": "Install-ChocolateyPackage",
             "file": "7z2408.exe", "size": 1592000, "sha256": "aa11",
             "checksum": {"status": "verified", "algorithm": "sha256"}},
            {"url": "https://www.7-zip.org/a/7z2408-x64.exe",
             "file": "7z2408-x64.exe", "checksum": {"status": "trust-on-first-use"}},
            {"url": "https://example.com/tool.zip", "checksum": {"status": "trust-on-first-use"}}
        ],
        "warnings": ["tool.zip: the feed does not allow .zip files"],
        "overrides": ["download https://example.com/tool.zip added"],
        "prelude": "tools/internalizer-prelude.ps1",
        "generated_at": "2026-10-01T12:00:00Z"
    }"#;

    #[test]
    fn a_full_report_round_trips() {
        let r = Report::from_slice(FULL.as_bytes()).unwrap();
        assert_eq!(r.status, Status::Internalized);
        assert_eq!(r.downloads[0].checksum.status, ChecksumStatus::Verified);
        assert_eq!(
            r.files().collect::<Vec<_>>(),
            ["7z2408.exe", "7z2408-x64.exe"]
        );
        assert_eq!(Report::from_slice(&r.to_vec()).unwrap(), r);
    }

    #[test]
    fn unknown_fields_and_values_are_tolerated() {
        let r = Report::from_slice(
            br#"{"schema": 1, "package": {"id": "a", "version": "1.0.0", "extra": 1},
                 "status": "quarantined", "upstream": {"source": "s"}, "new_field": [1, 2],
                 "downloads": [{"url": "u", "checksum": {"status": "signed", "by": "x"}}]}"#,
        )
        .unwrap();
        assert_eq!(r.status, Status::Unknown);
        assert_eq!(r.downloads[0].checksum.status, ChecksumStatus::Unknown);
        assert!(r.warnings.is_empty() && r.prelude.is_none());
    }

    #[test]
    fn another_schema_version_is_refused_by_name() {
        let err = Report::from_slice(br#"{"schema": 2, "status": {"changed": true}}"#).unwrap_err();
        assert!(matches!(err, ReportError::UnsupportedSchema(2)), "{err}");
        assert!(matches!(
            Report::from_slice(b"{\"schema\": 1}"),
            Err(ReportError::Invalid(_))
        ));
        assert!(matches!(
            Report::from_slice(b"not json"),
            Err(ReportError::Invalid(_))
        ));
        let big = vec![b' '; MAX_REPORT_BYTES as usize + 1];
        assert!(matches!(
            Report::from_slice(&big),
            Err(ReportError::TooLarge)
        ));
    }
}
