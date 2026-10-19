//! The static cross-check (`INT-23`): every URL a package's scripts spell
//! out, set against what the capture saw.
//!
//! Capture runs `chocolateyInstall.ps1` only, and only down the branches the
//! sandbox takes. A URL in `chocolateyUninstall.ps1`, in
//! `chocolateyBeforeModify.ps1`, or behind a branch the install did not take
//! is not internalized, and an internalized package that reaches for it
//! fails at that point. The scan cannot tell a download from a link to a
//! project page, so all it ever raises is a warning.

use std::io::Read;
use std::path::{Path, PathBuf};

use reqwest::Url;
use yanuget_pkg::nupkg::{entry_name, open_archive};

use crate::assess::Wanted;
use crate::report::Draft;

/// The largest script read; a larger one is skipped with a warning.
pub const MAX_SCRIPT_BYTES: u64 = 4 * 1024 * 1024;

/// The most mentions one package warns about; the rest are counted.
const MAX_WARNINGS: usize = 20;

/// Where the tool's own entries go (`internalizer/report.json`).
const OWN_DIR: &str = "internalizer/";

/// The longest URL taken from a script, as the capture takes it.
const MAX_URL_CHARS: usize = crate::capture::MAX_URL_CHARS;

/// A URL a script spells out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// The package entry, as the package names it.
    pub entry: String,
    /// 1-based.
    pub line: usize,
    pub url: String,
}

/// What the scan of one package found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    pub mentions: Vec<Mention>,
    /// Scripts too large to read.
    pub skipped: Vec<String>,
}

/// Every URL the PowerShell files of the package at `nupkg` spell out, in
/// entry order, outside comments.
pub async fn scan(nupkg: &Path) -> Result<Scan, yanuget_pkg::Error> {
    let nupkg: PathBuf = nupkg.to_path_buf();
    tokio::task::spawn_blocking(move || scan_blocking(&nupkg))
        .await
        .map_err(|e| yanuget_pkg::Error::Task(format!("script scan panicked: {e}")))?
}

fn scan_blocking(nupkg: &Path) -> Result<Scan, yanuget_pkg::Error> {
    let mut archive = open_archive(nupkg)?;
    let mut scan = Scan::default();
    for i in 0..archive.len() {
        let Some(name) = entry_name(&archive, i)? else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        let is_script = [".ps1", ".psm1", ".psd1"]
            .iter()
            .any(|ext| lower.ends_with(ext));
        // The tool's own entries say what it did, not what the package does.
        let own = lower.replace('\\', "/").starts_with(OWN_DIR);
        if !is_script || own {
            continue;
        }
        let file = archive
            .by_index(i)
            .map_err(|e| yanuget_pkg::Error::InvalidPackage(format!("{name}: {e}")))?;
        if file.size() > MAX_SCRIPT_BYTES {
            scan.skipped.push(name);
            continue;
        }
        let mut bytes = Vec::new();
        file.take(MAX_SCRIPT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| yanuget_pkg::Error::InvalidPackage(format!("{name}: {e}")))?;
        if bytes.len() as u64 > MAX_SCRIPT_BYTES {
            scan.skipped.push(name);
            continue;
        }
        for (line, url) in urls_in(&decode(&bytes)) {
            scan.mentions.push(Mention {
                entry: name.clone(),
                line,
                url,
            });
        }
    }
    Ok(scan)
}

/// A script's text: UTF-16 where a byte-order mark says so, as Windows
/// PowerShell writes it, UTF-8 otherwise.
fn decode(bytes: &[u8]) -> String {
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|p| {
                if le {
                    u16::from_le_bytes([p[0], p[1]])
                } else {
                    u16::from_be_bytes([p[0], p[1]])
                }
            })
            .collect();
        String::from_utf16_lossy(&units)
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        _ => String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes))
            .into_owned(),
    }
}

/// The `http` and `https` URLs in a script, with their lines, outside line
/// comments (`# …`) and block comments (`<# … #>`). A URL ends at
/// whitespace, a quote, a backtick or one of `<>|;()`, so one built from
/// variables keeps its `$name` parts.
pub fn urls_in(text: &str) -> Vec<(usize, String)> {
    let mut found = Vec::new();
    let mut in_block = false;
    for (n, line) in text.lines().enumerate() {
        let mut code = String::new();
        let mut rest = line;
        loop {
            if in_block {
                match rest.find("#>") {
                    Some(end) => {
                        rest = &rest[end + 2..];
                        in_block = false;
                    }
                    None => break,
                }
            } else {
                match rest.find("<#") {
                    Some(start) => {
                        code.push_str(&rest[..start]);
                        code.push(' ');
                        rest = &rest[start + 2..];
                        in_block = true;
                    }
                    None => {
                        code.push_str(rest);
                        break;
                    }
                }
            }
        }
        if code.trim_start().starts_with('#') {
            continue;
        }
        let lower = code.to_ascii_lowercase();
        let mut from = 0;
        while let Some(at) = ["https://", "http://"]
            .iter()
            .filter_map(|s| lower[from..].find(s))
            .min()
        {
            let start = from + at;
            let len = code[start..]
                .find(|c: char| {
                    c.is_whitespace()
                        || matches!(c, '\'' | '"' | '`' | '<' | '>' | '|' | ';' | '(' | ')')
                        || c.is_control()
                })
                .unwrap_or(code.len() - start);
            let url = code[start..start + len].trim_end_matches(['.', ',', ':']);
            if url.chars().count() <= MAX_URL_CHARS && url.len() > "https://".len() {
                found.push((n + 1, url.to_string()));
            }
            from = start + len.max(1);
        }
    }
    found
}

/// Whether the capture saw `url`: the same URL, or, for one built from
/// variables, a captured URL that starts with its literal part. One whose
/// host is a variable cannot be compared and counts as seen.
fn seen(url: &str, captured: &[Url]) -> bool {
    match url.find(['$', '{']) {
        Some(cut) => {
            let prefix = url[..cut].to_ascii_lowercase();
            let host_known = prefix
                .split_once("://")
                .is_some_and(|(_, after)| after.contains('/'));
            !host_known
                || captured
                    .iter()
                    .any(|c| c.as_str().to_ascii_lowercase().starts_with(&prefix))
        }
        None => match Url::parse(url) {
            Ok(parsed) => captured.contains(&parsed),
            Err(_) => captured.iter().any(|c| c.as_str() == url),
        },
    }
}

/// Warn in `draft` about each mention the capture did not see, given what it
/// asked to download.
pub fn warn(draft: &mut Draft, scan: &Scan, wanted: &[Wanted]) {
    let captured: Vec<Url> = wanted.iter().map(|w| w.url.clone()).collect();
    let mut warned = 0;
    let mut more = 0;
    let mut done: Vec<&str> = Vec::new();
    for m in scan.mentions.iter().filter(|m| !seen(&m.url, &captured)) {
        if done.contains(&m.url.as_str()) {
            continue;
        }
        done.push(&m.url);
        if warned < MAX_WARNINGS {
            draft.warn(format!(
                "{} line {} mentions {}, which the capture did not see",
                m.entry, m.line, m.url
            ));
            warned += 1;
        } else {
            more += 1;
        }
    }
    if more > 0 {
        draft.warn(format!(
            "{more} more URLs in the scripts that the capture did not see"
        ));
    }
    for entry in &scan.skipped {
        draft.warn(format!(
            "{entry} is larger than {} MiB and was not scanned for URLs",
            MAX_SCRIPT_BYTES / (1024 * 1024)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Helper;
    use yanuget_pkg::report::{PackageRef, Upstream};

    #[test]
    fn urls_are_found_outside_comments() {
        let script = r#"
$url = 'https://example.com/a/setup.exe'
$url64 = "https://example.com/a/setup-x64.exe" # see http://docs.example.com/
# https://example.com/commented.exe
<# a block
   https://example.com/in-block.exe
#> Get-Thing -Url https://example.com/after-block.zip; $x = 1
$t = "https://example.com/$version/tool-${arch}.msi"
Start-Process (HTTPS://Example.com/upper.exe)
Write-Host "see https://example.com/page."
$bare = 'https://'
"#;
        let urls: Vec<(usize, String)> = urls_in(script);
        assert_eq!(
            urls,
            [
                (2, "https://example.com/a/setup.exe".into()),
                (3, "https://example.com/a/setup-x64.exe".into()),
                (3, "http://docs.example.com/".into()),
                (7, "https://example.com/after-block.zip".into()),
                (8, "https://example.com/$version/tool-${arch}.msi".into()),
                (9, "HTTPS://Example.com/upper.exe".into()),
                (10, "https://example.com/page".into()),
            ]
        );
    }

    #[test]
    fn utf16_scripts_are_read() {
        let text = "$u = 'https://example.com/x.exe'";
        let mut le = vec![0xFF, 0xFE];
        le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let mut be = vec![0xFE, 0xFF];
        be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        for bytes in [le, be, format!("\u{FEFF}{text}").into_bytes()] {
            assert_eq!(
                urls_in(&decode(&bytes)),
                [(1, "https://example.com/x.exe".to_string())]
            );
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn templates_match_by_their_literal_part() {
        let captured = [url("https://example.com/2.0/tool-x64.msi")];
        assert!(seen("https://example.com/2.0/tool-x64.msi", &captured));
        assert!(seen("HTTPS://EXAMPLE.com/2.0/tool-x64.msi", &captured));
        assert!(seen("https://example.com/$version/tool.msi", &captured));
        assert!(!seen(
            "https://other.example.com/$version/tool.msi",
            &captured
        ));
        // The host itself is a variable: nothing to compare.
        assert!(seen("https://$mirror/tool.msi", &captured));
        assert!(!seen("https://example.com/uninstall.exe", &captured));
    }

    #[test]
    fn unseen_mentions_become_warnings() {
        let mut draft = Draft::new(
            PackageRef {
                id: "tool".into(),
                version: "2.0.0".into(),
            },
            Upstream {
                source: "s".into(),
                sha512: None,
            },
        );
        let mention = |entry: &str, line, url: &str| Mention {
            entry: entry.into(),
            line,
            url: url.into(),
        };
        let mut scan = Scan {
            mentions: vec![
                mention(
                    "tools/chocolateyInstall.ps1",
                    3,
                    "https://example.com/2.0/tool.msi",
                ),
                mention(
                    "tools/chocolateyUninstall.ps1",
                    7,
                    "https://example.com/cleanup.exe",
                ),
                mention("tools/helpers.psm1", 1, "https://example.com/cleanup.exe"),
            ],
            skipped: vec!["tools/huge.ps1".into()],
        };
        for i in 0..MAX_WARNINGS + 2 {
            scan.mentions.push(mention(
                "tools/x.ps1",
                i,
                &format!("https://example.com/{i}"),
            ));
        }
        let wanted = [Wanted {
            url: url("https://example.com/2.0/tool.msi"),
            checksum: None,
            helper: Helper::InstallChocolateyPackage,
            file_type: None,
        }];
        warn(&mut draft, &scan, &wanted);
        let report = draft.finish("t");
        assert_eq!(report.warnings.len(), MAX_WARNINGS + 2);
        assert_eq!(
            report.warnings[0],
            "tools/chocolateyUninstall.ps1 line 7 mentions https://example.com/cleanup.exe, \
             which the capture did not see"
        );
        assert_eq!(
            report.warnings[MAX_WARNINGS],
            "3 more URLs in the scripts that the capture did not see"
        );
        assert_eq!(
            report.warnings[MAX_WARNINGS + 1],
            "tools/huge.ps1 is larger than 4 MiB and was not scanned for URLs"
        );
        // Warnings only: the status is untouched.
        assert_eq!(report.status, yanuget_pkg::report::Status::Unchanged);
    }
}
