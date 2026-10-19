//! The capture: what the recording helpers inside the sandbox wrote, one
//! JSON object per line, for each download or install call a package script
//! made.
//!
//! Code from the package wrote it (it runs in the same PowerShell session as
//! the helpers), so it is parsed as hostile input: a versioned schema with
//! strict types and no unknown fields, caps on line length, entry count and
//! total size, no control characters in any string, `http` and `https` URLs
//! only, and every call attributed to a package of the closure. A capture
//! that breaks any rule is rejected as a whole, with every reason found, and
//! the package needs an override; nothing is repaired.
//!
//! One line, as the helpers write it:
//!
//! ```json
//! {"schema":1,"package":"git.install","version":"2.51.0",
//!  "helper":"Install-ChocolateyPackage",
//!  "url":"https://github.com/git-for-windows/…/Git-2.51.0-32-bit.exe",
//!  "url64":"https://github.com/git-for-windows/…/Git-2.51.0-64-bit.exe",
//!  "checksum":"…","checksumType":"sha256",
//!  "checksum64":"…","checksumType64":"sha256",
//!  "fileType":"exe","fileFullPath":null,
//!  "caller":"C:\\ProgramData\\chocolatey\\lib\\git.install\\tools\\chocolateyInstall.ps1"}
//! ```

use reqwest::Url;
use serde::Deserialize;
use yanuget_pkg::NuGetVersion;

/// The capture schema this tool reads, and its helpers write.
pub const SCHEMA_VERSION: u32 = 1;

/// The longest line taken.
pub const MAX_LINE_BYTES: usize = 16 * 1024;

/// The most calls one capture may hold.
pub const MAX_CALLS: usize = 1_000;

/// The most bytes one capture may hold. The sandbox enforces its own cap
/// (`sandbox.max_capture_bytes`) before this; this one holds whatever it is.
pub const MAX_CAPTURE_BYTES: usize = 16 * 1024 * 1024;

/// The longest URL taken.
pub const MAX_URL_CHARS: usize = 4096;

/// The longest path (`fileFullPath`, `caller`) taken.
pub const MAX_PATH_CHARS: usize = 1024;

/// The most reasons a rejection lists; one hostile capture should not flood
/// the report.
const MAX_REASONS: usize = 20;

/// The helpers that record, as Chocolatey names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Helper {
    #[serde(rename = "Get-ChocolateyWebFile")]
    GetChocolateyWebFile,
    #[serde(rename = "Get-WebFile")]
    GetWebFile,
    #[serde(rename = "Get-FtpFile")]
    GetFtpFile,
    #[serde(rename = "Install-ChocolateyPackage")]
    InstallChocolateyPackage,
    #[serde(rename = "Install-ChocolateyZipPackage")]
    InstallChocolateyZipPackage,
    #[serde(rename = "Install-ChocolateyVsixPackage")]
    InstallChocolateyVsixPackage,
    #[serde(rename = "Install-ChocolateyPowershellCommand")]
    InstallChocolateyPowershellCommand,
    #[serde(rename = "Install-ChocolateyInstallPackage")]
    InstallChocolateyInstallPackage,
    #[serde(rename = "Get-ChocolateyUnzip")]
    GetChocolateyUnzip,
}

impl Helper {
    pub fn as_str(self) -> &'static str {
        match self {
            Helper::GetChocolateyWebFile => "Get-ChocolateyWebFile",
            Helper::GetWebFile => "Get-WebFile",
            Helper::GetFtpFile => "Get-FtpFile",
            Helper::InstallChocolateyPackage => "Install-ChocolateyPackage",
            Helper::InstallChocolateyZipPackage => "Install-ChocolateyZipPackage",
            Helper::InstallChocolateyVsixPackage => "Install-ChocolateyVsixPackage",
            Helper::InstallChocolateyPowershellCommand => "Install-ChocolateyPowershellCommand",
            Helper::InstallChocolateyInstallPackage => "Install-ChocolateyInstallPackage",
            Helper::GetChocolateyUnzip => "Get-ChocolateyUnzip",
        }
    }
}

/// A checksum algorithm Chocolatey's helpers accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumType {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl ChecksumType {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "md5" => ChecksumType::Md5,
            "sha1" => ChecksumType::Sha1,
            "sha256" => ChecksumType::Sha256,
            "sha512" => ChecksumType::Sha512,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ChecksumType::Md5 => "md5",
            ChecksumType::Sha1 => "sha1",
            ChecksumType::Sha256 => "sha256",
            ChecksumType::Sha512 => "sha512",
        }
    }

    /// The digest's length in hex digits.
    fn hex_len(self) -> usize {
        match self {
            ChecksumType::Md5 => 32,
            ChecksumType::Sha1 => 40,
            ChecksumType::Sha256 => 64,
            ChecksumType::Sha512 => 128,
        }
    }

    /// Whether a match proves the file is the one the package author meant:
    /// MD5 and SHA-1 collide on demand.
    pub fn is_strong(self) -> bool {
        matches!(self, ChecksumType::Sha256 | ChecksumType::Sha512)
    }
}

/// A checksum a script declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub kind: ChecksumType,
    /// Lower-case hex.
    pub hex: String,
}

/// Where in the guest the call came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The package's own scripts, under `lib\{id}\`.
    Package,
    /// An extension's module, under `extensions\{name}\`; the prelude cannot
    /// redirect such a call.
    Extension(String),
    /// Anywhere else, or nowhere the helpers could tell.
    Other(Option<String>),
}

/// One recorded call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The package whose script made it, as the closure spells it.
    pub package: String,
    pub version: NuGetVersion,
    pub helper: Helper,
    pub url: Option<Url>,
    pub url64: Option<Url>,
    pub checksum: Option<Checksum>,
    pub checksum64: Option<Checksum>,
    /// What the script said the file is (`exe`, `msi`, `zip`, …), lower case.
    pub file_type: Option<String>,
    /// Where the script asked the file to go, as it asked.
    pub file_full_path: Option<String>,
    pub origin: Origin,
}

/// Why a capture was rejected: every reason found, up to a cap.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the capture is not usable: {}", reasons.join("; "))]
pub struct Rejected {
    pub reasons: Vec<String>,
}

/// One line as written. Every field is named, typed and required to be
/// known; `null` and a missing field mean the same.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct Line {
    schema: u32,
    package: String,
    version: String,
    helper: Helper,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    url64: Option<String>,
    #[serde(default)]
    checksum: Option<String>,
    #[serde(default)]
    checksum_type: Option<String>,
    #[serde(default)]
    checksum64: Option<String>,
    #[serde(default)]
    checksum_type64: Option<String>,
    #[serde(default)]
    file_type: Option<String>,
    #[serde(default)]
    file_full_path: Option<String>,
    #[serde(default)]
    caller: Option<String>,
}

/// Parse a capture against the closure it ran with, given as `(id, version)`
/// of each package in it.
pub fn parse(raw: &[u8], closure: &[(&str, &NuGetVersion)]) -> Result<Vec<Call>, Rejected> {
    let mut reasons = Vec::new();
    if raw.len() > MAX_CAPTURE_BYTES {
        return Err(Rejected {
            reasons: vec![format!("it is larger than {MAX_CAPTURE_BYTES} bytes")],
        });
    }
    // Windows PowerShell writes UTF-8 with a byte-order mark.
    let raw = raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(raw);
    let mut calls = Vec::new();
    for (n, line) in raw.split(|&b| b == b'\n').enumerate() {
        let n = n + 1;
        if reasons.len() >= MAX_REASONS {
            reasons.push("more reasons not listed".to_string());
            break;
        }
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_LINE_BYTES {
            reasons.push(format!("line {n} is longer than {MAX_LINE_BYTES} bytes"));
            continue;
        }
        if calls.len() >= MAX_CALLS {
            reasons.push(format!("it holds more than {MAX_CALLS} calls"));
            break;
        }
        match parse_line(line, closure) {
            Ok(call) => calls.push(call),
            Err(why) => reasons.push(format!("line {n}: {why}")),
        }
    }
    if reasons.is_empty() {
        Ok(calls)
    } else {
        Err(Rejected { reasons })
    }
}

fn parse_line(line: &[u8], closure: &[(&str, &NuGetVersion)]) -> Result<Call, String> {
    let text = std::str::from_utf8(line).map_err(|_| "not UTF-8".to_string())?;
    let l: Line = serde_json::from_str(text).map_err(|e| format!("not a capture record: {e}"))?;
    if l.schema != SCHEMA_VERSION {
        return Err(format!(
            "schema version {}, and this tool reads version {SCHEMA_VERSION}",
            l.schema
        ));
    }
    for (name, value) in [
        ("package", Some(&l.package)),
        ("version", Some(&l.version)),
        ("url", l.url.as_ref()),
        ("url64", l.url64.as_ref()),
        ("checksum", l.checksum.as_ref()),
        ("checksumType", l.checksum_type.as_ref()),
        ("checksum64", l.checksum64.as_ref()),
        ("checksumType64", l.checksum_type64.as_ref()),
        ("fileType", l.file_type.as_ref()),
        ("fileFullPath", l.file_full_path.as_ref()),
        ("caller", l.caller.as_ref()),
    ] {
        if let Some(c) = value.and_then(|v| v.chars().find(|c| is_forbidden(*c))) {
            return Err(format!(
                "{name} holds the control character U+{:04X}",
                c as u32
            ));
        }
    }

    // Attribution: a package of the closure, and only one.
    let version = NuGetVersion::parse(&l.version)
        .map_err(|_| format!("version {:?} is not a NuGet version", l.version))?;
    let Some((package, _)) = closure
        .iter()
        .find(|(id, v)| id.eq_ignore_ascii_case(&l.package) && **v == version)
    else {
        return Err(format!(
            "{} {} is not a package of the closure",
            short(&l.package),
            version.normalized()
        ));
    };

    let url = l.url.as_deref().map(|u| url_of("url", u)).transpose()?;
    let url64 = l.url64.as_deref().map(|u| url_of("url64", u)).transpose()?;
    let checksum = checksum_of("checksum", l.checksum, l.checksum_type)?;
    let checksum64 = checksum_of("checksum64", l.checksum64, l.checksum_type64)?;
    let file_type = l
        .file_type
        .filter(|t| !t.is_empty())
        .map(|t| {
            if t.len() <= 16 && t.bytes().all(|b| b.is_ascii_alphanumeric()) {
                Ok(t.to_ascii_lowercase())
            } else {
                Err(format!("fileType {:?} is not a file type", short(&t)))
            }
        })
        .transpose()?;
    for (name, path) in [("fileFullPath", &l.file_full_path), ("caller", &l.caller)] {
        if path
            .as_ref()
            .is_some_and(|p| p.chars().count() > MAX_PATH_CHARS)
        {
            return Err(format!("{name} is longer than {MAX_PATH_CHARS} characters"));
        }
    }
    Ok(Call {
        origin: origin_of(l.caller.as_deref(), package),
        package: package.to_string(),
        version,
        helper: l.helper,
        url,
        url64,
        checksum,
        checksum64,
        file_type,
        file_full_path: l.file_full_path.filter(|p| !p.is_empty()),
    })
}

/// Control characters (C0, DEL, C1) and the bidirectional controls that make
/// text read differently from what it is.
fn is_forbidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// At most 64 characters of untrusted text, for a message.
fn short(s: &str) -> String {
    let mut out: String = s.chars().take(64).collect();
    if out.len() < s.len() {
        out.push('…');
    }
    out
}

fn url_of(name: &str, s: &str) -> Result<Url, String> {
    if s.chars().count() > MAX_URL_CHARS {
        return Err(format!("{name} is longer than {MAX_URL_CHARS} characters"));
    }
    let url = Url::parse(s).map_err(|e| format!("{name} {:?} is not a URL: {e}", short(s)))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("{name} uses the scheme {:?}", short(url.scheme())));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(format!("{name} {:?} has no host", short(s)));
    }
    Ok(url)
}

fn checksum_of(
    name: &str,
    value: Option<String>,
    kind: Option<String>,
) -> Result<Option<Checksum>, String> {
    let value = value.filter(|v| !v.is_empty());
    let kind = kind.filter(|k| !k.is_empty());
    let Some(value) = value else {
        // A type without a checksum declares nothing.
        return Ok(None);
    };
    // Chocolatey takes a checksum without a type as MD5.
    let kind = match kind {
        None => ChecksumType::Md5,
        Some(k) => ChecksumType::parse(&k)
            .ok_or_else(|| format!("{name} has the unknown type {:?}", short(&k)))?,
    };
    let hex = value.to_ascii_lowercase();
    if hex.len() != kind.hex_len() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "{name} is not a {} digest in hex ({} digits)",
            kind.as_str(),
            kind.hex_len()
        ));
    }
    Ok(Some(Checksum { kind, hex }))
}

/// Tell the package's own scripts from an extension's module by the path of
/// the script that called the helper: Chocolatey runs a package from
/// `lib\{id}\` and loads extensions from `extensions\{name}\`.
fn origin_of(caller: Option<&str>, package: &str) -> Origin {
    let Some(caller) = caller.filter(|c| !c.is_empty()) else {
        return Origin::Other(None);
    };
    let parts: Vec<&str> = caller.split(['\\', '/']).collect();
    for pair in parts.windows(2) {
        if pair[0].eq_ignore_ascii_case("lib") && pair[1].eq_ignore_ascii_case(package) {
            return Origin::Package;
        }
    }
    for pair in parts.windows(2) {
        if pair[0].eq_ignore_ascii_case("extensions") && !pair[1].is_empty() {
            return Origin::Extension(pair[1].to_ascii_lowercase());
        }
    }
    Origin::Other(Some(caller.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v(s: &str) -> NuGetVersion {
        NuGetVersion::parse(s).unwrap()
    }

    const SHA256: &str = "AB0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcd";

    fn line(overrides: serde_json::Value) -> String {
        let mut base = json!({
            "schema": 1,
            "package": "Git.Install",
            "version": "2.51.0",
            "helper": "Install-ChocolateyPackage",
            "url": "https://github.com/x/Git-32-bit.exe",
            "url64": "https://github.com/x/Git-64-bit.exe",
            "checksum": SHA256,
            "checksumType": "SHA256",
            "checksum64": SHA256,
            "checksumType64": "sha256",
            "fileType": "EXE",
            "fileFullPath": null,
            "caller": "C:\\ProgramData\\chocolatey\\lib\\git.install\\tools\\chocolateyInstall.ps1"
        });
        for (k, val) in overrides.as_object().unwrap() {
            if val.is_null() {
                base.as_object_mut().unwrap().remove(k);
            } else {
                base[k] = val.clone();
            }
        }
        base.to_string()
    }

    fn parse_lines(lines: &[String]) -> Result<Vec<Call>, Rejected> {
        let (git, install) = (v("2.51.0"), v("2.51.0"));
        let closure = [("git", &git), ("git.install", &install)];
        parse(lines.join("\r\n").as_bytes(), &closure)
    }

    #[test]
    fn a_capture_parses_into_calls() {
        let raw = format!(
            "\u{FEFF}{}\r\n{}\r\n\r\n",
            line(json!({})),
            line(json!({
                "helper": "Get-ChocolateyWebFile",
                "url64": null,
                "checksum": "d41d8cd98f00b204e9800998ecf8427e",
                "checksumType": null,
                "caller": "C:\\ProgramData\\chocolatey\\extensions\\chocolatey-core\\Get-AppInstallLocation.psm1"
            }))
        );
        let (git, install) = (v("2.51.0"), v("2.51.0"));
        let calls = parse(raw.as_bytes(), &[("git", &git), ("git.install", &install)]).unwrap();
        assert_eq!(calls.len(), 2);
        let c = &calls[0];
        assert_eq!(c.package, "git.install", "the closure's spelling");
        assert_eq!(c.helper, Helper::InstallChocolateyPackage);
        assert_eq!(
            c.url.as_ref().unwrap().as_str(),
            "https://github.com/x/Git-32-bit.exe"
        );
        let sum = c.checksum.as_ref().unwrap();
        assert_eq!(
            (sum.kind, sum.hex.as_str()),
            (ChecksumType::Sha256, SHA256.to_ascii_lowercase().as_str())
        );
        assert_eq!(c.file_type.as_deref(), Some("exe"));
        assert_eq!(c.origin, Origin::Package);
        // A checksum without a type is MD5, as Chocolatey takes it.
        let c = &calls[1];
        assert_eq!(c.checksum.as_ref().unwrap().kind, ChecksumType::Md5);
        assert!(c.url64.is_none());
        assert_eq!(c.origin, Origin::Extension("chocolatey-core".into()));
    }

    #[test]
    fn every_broken_rule_rejects_the_capture() {
        let cases = [
            (json!({"schema": 2}), "schema version 2"),
            (json!({"extra": 1}), "unknown field"),
            (json!({"url": 5}), "not a capture record"),
            (
                json!({"helper": "Invoke-WebRequest"}),
                "not a capture record",
            ),
            (json!({"url": "ftp://example.com/x.exe"}), "scheme \"ftp\""),
            (json!({"url": "file:///C:/x.exe"}), "scheme \"file\""),
            (json!({"url": "not a url"}), "is not a URL"),
            (
                json!({"url": "https://example.com/a\u{1b}[2J"}),
                "control character U+001B",
            ),
            (
                json!({"fileFullPath": "C:\\a\u{202E}exe.txt"}),
                "control character U+202E",
            ),
            (
                json!({"package": "evil"}),
                "evil 2.51.0 is not a package of the closure",
            ),
            (
                json!({"version": "2.50.0"}),
                "is not a package of the closure",
            ),
            (json!({"version": "x"}), "is not a NuGet version"),
            (json!({"checksumType": "crc32"}), "unknown type \"crc32\""),
            (json!({"checksum": "abc"}), "not a sha256 digest"),
            (json!({"checksum": "z".repeat(64)}), "not a sha256 digest"),
            (json!({"fileType": "exe;rm"}), "not a file type"),
            (
                json!({"caller": "c".repeat(MAX_PATH_CHARS + 1)}),
                "caller is longer",
            ),
        ];
        for (overrides, want) in cases {
            let err = parse_lines(&[line(json!({})), line(overrides.clone())]).unwrap_err();
            assert_eq!(err.reasons.len(), 1, "{overrides}: {err}");
            assert!(err.reasons[0].starts_with("line 2: "), "{err}");
            assert!(err.to_string().contains(want), "{overrides}: {err}");
        }
    }

    #[test]
    fn size_and_count_are_capped() {
        let long = format!("{{\"x\":\"{}\"}}", "a".repeat(MAX_LINE_BYTES));
        let err = parse_lines(&[long]).unwrap_err();
        assert!(err.reasons[0].contains("longer than"), "{err}");

        let many = vec![line(json!({})); MAX_CALLS + 1];
        let err = parse_lines(&many).unwrap_err();
        assert_eq!(
            err.reasons,
            [format!("it holds more than {MAX_CALLS} calls")]
        );

        let bad = vec![line(json!({"schema": 9})); 50];
        let err = parse_lines(&bad).unwrap_err();
        assert_eq!(err.reasons.len(), MAX_REASONS + 1);

        let huge = vec![b'\n'; MAX_CAPTURE_BYTES + 1];
        assert!(parse(&huge, &[]).is_err());
        assert!(parse(b"\xff\n", &[]).unwrap_err().reasons[0].contains("not UTF-8"));
        assert_eq!(parse(b"", &[]).unwrap(), []);
    }

    #[test]
    fn origins_follow_the_callers_path() {
        assert_eq!(
            origin_of(
                Some("C:/ProgramData/chocolatey/lib/GIT.INSTALL/tools/x.ps1"),
                "git.install"
            ),
            Origin::Package
        );
        assert_eq!(
            origin_of(
                Some("C:\\ProgramData\\chocolatey\\extensions\\vs\\x.psm1"),
                "git"
            ),
            Origin::Extension("vs".into())
        );
        // Another package's folder is neither.
        assert!(matches!(
            origin_of(
                Some("C:\\ProgramData\\chocolatey\\lib\\other\\x.ps1"),
                "git"
            ),
            Origin::Other(Some(_))
        ));
        assert_eq!(origin_of(None, "git"), Origin::Other(None));
    }
}
