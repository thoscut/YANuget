//! The prelude (`INT-19`): what makes an internalized package install from
//! the feed. Its `chocolateyInstall.ps1` becomes a loader that dot-sources
//! `internalizer/prelude.ps1` and then the package's own script, renamed to
//! `chocolateyInstall.upstream.ps1` beside it, so `$toolsDir` and
//! `$MyInvocation` lead to the same folder as before.
//!
//! The prelude wraps Chocolatey's download helpers in proxy commands, made on
//! the client from the helpers it has, so a call binds exactly as it does
//! against them. A wrapper replaces each URL with the attached file's and
//! the checksum with the file's SHA-256; a URL it does not know stops the
//! install. The map is keyed by the SHA-256 of the URL, so nothing a package
//! wrote is ever emitted as PowerShell; the report carries the originals.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};

use reqwest::Url;
use sha2::{Digest, Sha256};
use yanuget_pkg::nupkg::{entry_name, open_archive};
use yanuget_pkg::report::Download;
use yanuget_pkg::rewrite::Source;
use yanuget_pkg::NuGetVersion;

use crate::crosscheck::MAX_SCRIPT_BYTES;

/// The package entry holding the prelude, beside the report.
pub const PRELUDE_PATH: &str = "internalizer/prelude.ps1";

/// What the package's own `chocolateyInstall.ps1` is renamed to.
pub const UPSTREAM_SCRIPT: &str = "chocolateyInstall.upstream.ps1";

/// The script the placeholders are filled into.
const TEMPLATE: &str = include_str!("prelude.ps1");

#[derive(Debug, thiserror::Error)]
pub enum PreludeError {
    #[error("{0}")]
    Package(String),
    #[error(transparent)]
    Pkg(#[from] yanuget_pkg::Error),
}

/// The entries to add to an internalized package; the prelude itself is at
/// [`PRELUDE_PATH`], for the report.
#[derive(Debug)]
pub struct Prelude {
    pub entries: Vec<(String, Source)>,
}

/// The prelude for the package at `nupkg`, `id` `version`, whose downloads
/// were attached to it in the feed at `feed` (the feed's URL, as the tool
/// pushes to it). `None` when no download was attached: the package installs
/// as it is.
///
/// Downloads without a file are left out of the map, so the install stops
/// where the package asks for one.
pub async fn build(
    nupkg: &Path,
    id: &str,
    version: &str,
    downloads: &[Download],
    feed: &Url,
) -> Result<Option<Prelude>, PreludeError> {
    let prelude = render(id, version, downloads, feed)?;
    let Some(prelude) = prelude else {
        return Ok(None);
    };
    let nupkg: PathBuf = nupkg.to_path_buf();
    let (name, script) = tokio::task::spawn_blocking(move || install_script(&nupkg))
        .await
        .map_err(|e| {
            yanuget_pkg::Error::Task(format!("reading the install script panicked: {e}"))
        })??;
    let (dir, _) = split(&name);
    let depth = dir.split(['/', '\\']).filter(|c| !c.is_empty()).count();
    let upstream = if dir.is_empty() {
        UPSTREAM_SCRIPT.to_string()
    } else {
        format!("{dir}/{UPSTREAM_SCRIPT}")
    };
    Ok(Some(Prelude {
        entries: vec![
            (
                PRELUDE_PATH.to_string(),
                Source::Bytes(prelude.into_bytes()),
            ),
            (upstream, Source::Bytes(script)),
            (name, Source::Bytes(loader(depth).into_bytes())),
        ],
    }))
}

/// The prelude's text, or `None` when there is nothing to redirect.
pub fn render(
    id: &str,
    version: &str,
    downloads: &[Download],
    feed: &Url,
) -> Result<Option<String>, PreludeError> {
    let version = NuGetVersion::parse(version)
        .map_err(|e| PreludeError::Package(format!("version {version:?}: {e}")))?
        .normalized()
        .to_ascii_lowercase();
    let dir = format!("files/{}/{version}/", id.to_ascii_lowercase());
    if !dir.bytes().all(is_plain) {
        return Err(PreludeError::Package(format!(
            "{id} {version} cannot be put in a file URL as it is"
        )));
    }
    let mut map = std::collections::BTreeMap::new();
    for d in downloads {
        let (Some(file), Some(sha256)) = (&d.file, &d.sha256) else {
            continue;
        };
        if !file.bytes().all(is_plain) || file.contains('/') {
            return Err(PreludeError::Package(format!(
                "the attached file name {file:?} cannot be put in a URL as it is"
            )));
        }
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(PreludeError::Package(format!(
                "{file} has no SHA-256 to pin it by"
            )));
        }
        map.insert(
            hex::encode(Sha256::digest(d.url.as_bytes())),
            (format!("{dir}{file}"), sha256.to_ascii_lowercase()),
        );
    }
    if map.is_empty() {
        return Ok(None);
    }
    let mut files = String::new();
    for (key, (path, sha256)) in &map {
        let _ = writeln!(files, "        '{key}' = @('{path}', '{sha256}')");
    }
    let files = files.trim_end_matches('\n');
    let mut feed = feed.to_string();
    if !feed.ends_with('/') {
        feed.push('/');
    }
    let text = TEMPLATE
        .replace("@FEED_URL@", &single_quoted(&feed))
        .replace("@FILES@", files);
    debug_assert!(text.is_ascii());
    Ok(Some(crlf(&text)))
}

/// The `chocolateyInstall.ps1` of an internalized package, `depth` folders
/// below the package root.
pub fn loader(depth: usize) -> String {
    let prelude = format!(
        "{}{}",
        "..\\".repeat(depth),
        PRELUDE_PATH.replace('/', "\\")
    );
    crlf(&format!(
        "# Internalized by yanuget-internalize: this package's downloads come from\n\
         # the feed it was installed from (see {PRELUDE_PATH}). The package's own\n\
         # install script is {UPSTREAM_SCRIPT}, beside this one.\n\
         . (Join-Path $PSScriptRoot '{prelude}')\n\
         . (Join-Path $PSScriptRoot '{UPSTREAM_SCRIPT}')\n"
    ))
}

/// A character that needs no escaping in a URL path or a PowerShell string.
fn is_plain(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/')
}

/// `s` inside a single-quoted PowerShell string, which ends at any of the
/// four single quotes PowerShell accepts; each is doubled.
fn single_quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out
}

fn crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// The entry's folder (without the trailing separator) and file name.
fn split(name: &str) -> (&str, &str) {
    match name.rfind(['/', '\\']) {
        Some(i) => (&name[..i], &name[i + 1..]),
        None => ("", name),
    }
}

/// The package's `chocolateyInstall.ps1`, the first in entry order as
/// Chocolatey finds it, and its content.
fn install_script(nupkg: &Path) -> Result<(String, Vec<u8>), PreludeError> {
    let mut archive = open_archive(nupkg)?;
    let mut found = None;
    for i in 0..archive.len() {
        let Some(name) = entry_name(&archive, i)? else {
            continue;
        };
        let lower = name.to_ascii_lowercase().replace('\\', "/");
        let (_, file) = split(&lower);
        if lower.starts_with("internalizer/") || file == UPSTREAM_SCRIPT.to_ascii_lowercase() {
            return Err(PreludeError::Package(format!(
                "the package has an entry {name}, which the prelude would replace"
            )));
        }
        if found.is_none() && file == "chocolateyinstall.ps1" {
            found = Some((i, name));
        }
    }
    let Some((i, name)) = found else {
        return Err(PreludeError::Package(
            "files were attached, but the package has no chocolateyInstall.ps1 to load the \
             prelude from"
                .into(),
        ));
    };
    let file = archive
        .by_index(i)
        .map_err(|e| yanuget_pkg::Error::InvalidPackage(format!("{name}: {e}")))?;
    let mut script = Vec::new();
    file.take(MAX_SCRIPT_BYTES + 1)
        .read_to_end(&mut script)
        .map_err(|e| yanuget_pkg::Error::InvalidPackage(format!("{name}: {e}")))?;
    if script.len() as u64 > MAX_SCRIPT_BYTES {
        return Err(PreludeError::Package(format!(
            "{name} is larger than {} KiB",
            MAX_SCRIPT_BYTES / 1024
        )));
    }
    Ok((name, script))
}

#[cfg(test)]
mod tests {
    use super::*;
    use yanuget_pkg::report::{Checksum, ChecksumStatus};

    fn download(url: &str, file: Option<&str>, sha: &str) -> Download {
        Download {
            url: url.into(),
            helper: Some("Install-ChocolateyPackage".into()),
            file: file.map(str::to_string),
            size: Some(1),
            sha256: Some(sha.into()),
            checksum: Checksum {
                status: ChecksumStatus::Verified,
                algorithm: Some("sha256".into()),
            },
        }
    }

    fn feed() -> Url {
        Url::parse("https://nuget.example.com/feeds/choco").unwrap()
    }

    #[test]
    fn the_map_is_keyed_by_the_hash_of_the_url() {
        let sha = "ab".repeat(32);
        let text = render(
            "Git.Install",
            "2.51.0.0",
            &[download(
                "https://github.com/x/Git-2.51.0-64-bit.exe",
                Some("Git-2.51.0-64-bit.exe"),
                &sha,
            )],
            &feed(),
        )
        .unwrap()
        .unwrap();
        let key = hex::encode(Sha256::digest(
            b"https://github.com/x/Git-2.51.0-64-bit.exe",
        ));
        assert!(text.contains(&format!(
            "'{key}' = @('files/git.install/2.51.0/Git-2.51.0-64-bit.exe', '{sha}')"
        )));
        // The URL itself is never in the script.
        assert!(!text.contains("github.com"));
        assert!(text.contains("$YanugetFeedUrl = 'https://nuget.example.com/feeds/choco/'"));
        assert!(text.contains("\r\n") && !text.replace("\r\n", "").contains('\n'));
        assert!(!text.contains("@FILES@") && !text.contains("@FEED_URL@"));
    }

    #[test]
    fn nothing_attached_needs_no_prelude() {
        let d = download("https://example.com/a.exe", None, &"ab".repeat(32));
        assert!(render("a", "1.0.0", &[d], &feed()).unwrap().is_none());
        assert!(render("a", "1.0.0", &[], &feed()).unwrap().is_none());
    }

    #[test]
    fn quotes_in_the_feed_url_are_doubled() {
        let feed = Url::parse("https://example.com/it's/").unwrap();
        let text = render(
            "a",
            "1.0.0",
            &[download(
                "https://example.com/a.exe",
                Some("a.exe"),
                &"ab".repeat(32),
            )],
            &feed,
        )
        .unwrap()
        .unwrap();
        assert!(text.contains("$YanugetFeedUrl = 'https://example.com/it''s/'"));
        assert_eq!(single_quoted("a\u{2019}b"), "a\u{2019}\u{2019}b");
    }

    #[test]
    fn unsafe_names_and_hashes_are_refused() {
        let ok = "ab".repeat(32);
        for (file, sha) in [("a'b.exe", ok.as_str()), ("a.exe", "ab"), ("a.exe", "zz")] {
            let d = download("https://example.com/a.exe", Some(file), sha);
            assert!(render("a", "1.0.0", &[d], &feed()).is_err(), "{file} {sha}");
        }
    }

    #[test]
    fn the_loader_finds_the_prelude_from_its_folder() {
        assert!(loader(1).contains(". (Join-Path $PSScriptRoot '..\\internalizer\\prelude.ps1')"));
        assert!(loader(0).contains(". (Join-Path $PSScriptRoot 'internalizer\\prelude.ps1')"));
        assert!(loader(2).contains("'..\\..\\internalizer\\prelude.ps1'"));
        assert!(loader(1).contains(". (Join-Path $PSScriptRoot 'chocolateyInstall.upstream.ps1')"));
    }
}
