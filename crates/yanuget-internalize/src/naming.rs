//! Names for attached files.
//!
//! The feed takes 1 to 128 characters of `A-Z a-z 0-9 . _ -` with an allowed
//! extension ([`yanuget_pkg::files`]). Upstream names carry spaces and `+`, or
//! there is none at all (`?product=…&os=win64`). The URL's own name is kept
//! where it is valid and its extension fits the content; otherwise a name is
//! made from `Content-Disposition`, the URL after redirects, the URL as
//! captured and, last, the package, with the extension taken from the
//! captured file type or the content itself (`MZ`, the OLE header of an
//! `.msi`, `PK`, …). Two different files that would share a name in one
//! version are told apart by a prefix of their SHA-256. A name whose
//! extension the feed does not accept is reported, never changed to pass.

use std::collections::HashMap;

use reqwest::Url;
use yanuget_pkg::files::{extension_allowed, file_name_problem, MAX_FILE_NAME_LENGTH};

/// What a file's first bytes say it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `MZ`: an executable.
    Executable,
    /// The OLE compound file header: `.msi`, `.msp`.
    Ole,
    /// `PK`: a zip, and every format built on one (`.vsix`, `.nupkg`, …).
    Zip,
    SevenZip,
    /// `MSCF`: a cabinet, `.msu` included.
    Cabinet,
    Gzip,
    /// None of these: the extension is not checked.
    Other,
}

impl Kind {
    /// Read the kind from a file's first eight bytes or more.
    pub fn sniff(head: &[u8]) -> Kind {
        if head.starts_with(b"MZ") {
            Kind::Executable
        } else if head.starts_with(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
            Kind::Ole
        } else if head.starts_with(b"PK\x03\x04") {
            Kind::Zip
        } else if head.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]) {
            Kind::SevenZip
        } else if head.starts_with(b"MSCF") {
            Kind::Cabinet
        } else if head.starts_with(&[0x1F, 0x8B]) {
            Kind::Gzip
        } else {
            Kind::Other
        }
    }

    /// The extensions a file of this kind may carry; empty for `Other`.
    fn extensions(self) -> &'static [&'static str] {
        match self {
            Kind::Executable => &["exe", "dll", "sys", "scr"],
            Kind::Ole => &["msi", "msp", "mst"],
            Kind::Zip => &[
                "zip",
                "vsix",
                "nupkg",
                "msix",
                "msixbundle",
                "appx",
                "appxbundle",
                "jar",
                "xpi",
            ],
            Kind::SevenZip => &["7z"],
            Kind::Cabinet => &["cab", "msu"],
            Kind::Gzip => &["gz", "tgz"],
            Kind::Other => &[],
        }
    }

    /// Whether `ext` fits the content. An extension of a known kind fits
    /// only that kind, so `.exe` on a zip does not pass; any other extension
    /// (`.iso`, `.ps1`) fits content of no known kind.
    fn fits(self, ext: &str) -> bool {
        let ext = ext.to_ascii_lowercase();
        match self {
            Kind::Other => !KNOWN.iter().any(|k| k.extensions().contains(&ext.as_str())),
            k => k.extensions().contains(&ext.as_str()),
        }
    }
}

const KNOWN: [Kind; 6] = [
    Kind::Executable,
    Kind::Ole,
    Kind::Zip,
    Kind::SevenZip,
    Kind::Cabinet,
    Kind::Gzip,
];

/// What is known about one downloaded file.
#[derive(Debug, Clone)]
pub struct Source<'a> {
    /// The URL as the script asked for it.
    pub url: &'a Url,
    /// Where it came from after redirects.
    pub final_url: Option<&'a Url>,
    pub content_disposition: Option<&'a str>,
    /// The captured `fileType`.
    pub file_type: Option<&'a str>,
    /// What the file's first bytes say.
    pub kind: Kind,
    /// Lower-case hex.
    pub sha256: &'a str,
    /// The last resort for a name: the package, as `git.install-2.51.0`.
    pub fallback: &'a str,
}

/// The names given within one version so far, by lower-case name, with the
/// SHA-256 of the file each names.
#[derive(Debug, Default)]
pub struct Names {
    taken: HashMap<String, String>,
}

impl Names {
    pub fn new() -> Self {
        Self::default()
    }

    /// The name for `file`, or why it cannot be attached. The same content
    /// gets the name it already has.
    pub fn name(&mut self, file: &Source<'_>, allowed: &[String]) -> Result<String, String> {
        if let Some((name, _)) = self.taken.iter().find(|(_, sha)| *sha == file.sha256) {
            return Ok(name.clone());
        }
        let mut name = pick(file);
        let ext = name
            .rsplit_once('.')
            .map(|(_, e)| e.to_string())
            .unwrap_or_default();
        if !extension_allowed(allowed, &ext) {
            return Err(format!(
                "{name}: the feed takes no .{ext} files (allowed: {})",
                allowed.join(", ")
            ));
        }
        if self.taken.contains_key(&name.to_ascii_lowercase()) {
            let prefix = file.sha256.get(..8).unwrap_or(file.sha256);
            name = fit(&format!("{prefix}-{name}"));
        }
        if let Some(why) = file_name_problem(&name, allowed) {
            return Err(format!("{name}: {why}"));
        }
        self.taken
            .insert(name.to_ascii_lowercase(), file.sha256.to_string());
        Ok(name)
    }
}

/// The best name the sources give, with an extension that fits the content.
fn pick(file: &Source<'_>) -> String {
    // The URL's own name, kept as it is when it is valid and fits.
    if let Some(original) = last_segment(file.url) {
        if is_clean(&original)
            && original
                .rsplit_once('.')
                .is_some_and(|(_, ext)| file.kind.fits(ext))
        {
            return original;
        }
    }
    let candidates = [
        file.content_disposition.and_then(disposition_name),
        file.final_url.and_then(last_segment),
        last_segment(file.url),
    ];
    let stem_and_ext = candidates.iter().flatten().find_map(|c| {
        let clean = sanitize(c);
        let (stem, ext) = match clean.rsplit_once('.') {
            Some((s, e)) if !s.is_empty() => (s, Some(e.to_string())),
            _ => (clean.as_str(), None),
        };
        let stem = stem.trim_end_matches(['.', '-', '_']).to_string();
        (!stem.is_empty()).then_some((stem, ext))
    });
    let (stem, ext) = stem_and_ext.unwrap_or_else(|| (sanitize(file.fallback), None));
    let ext = ext
        .filter(|e| file.kind.fits(e))
        .or_else(|| {
            file.file_type
                .map(str::to_ascii_lowercase)
                .filter(|t| file.kind.fits(t))
        })
        .or_else(|| file.kind.extensions().first().map(|e| e.to_string()))
        .unwrap_or_default();
    if ext.is_empty() {
        return fit(&stem);
    }
    fit(&format!("{stem}.{ext}"))
}

/// Valid under the feed's rule, the extension aside.
fn is_clean(name: &str) -> bool {
    file_name_problem(
        name,
        &[name.rsplit_once('.').map_or("", |(_, e)| e).to_string()],
    )
    .is_none()
}

/// The last path segment of `url`, percent-decoded.
fn last_segment(url: &Url) -> Option<String> {
    let segment = url.path_segments()?.rev().find(|s| !s.is_empty())?;
    Some(percent_decode(segment))
}

/// The file name a `Content-Disposition` header gives: `filename*` (RFC
/// 5987) before `filename`.
fn disposition_name(header: &str) -> Option<String> {
    let mut plain = None;
    let mut extended = None;
    for part in header.split(';').skip(1) {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                extended = value.split_once("''").map(|(_, v)| percent_decode(v));
            }
            "filename" => plain = Some(value.to_string()),
            _ => {}
        }
    }
    let name = extended.or(plain)?;
    // A path in the header names its last segment only.
    name.rsplit(['/', '\\']).next().map(str::to_string)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && bytes[i + 1].is_ascii_hexdigit()
            && bytes[i + 2].is_ascii_hexdigit()
        {
            let hex = |b: u8| (b as char).to_digit(16).expect("a hex digit") as u8;
            out.push(hex(bytes[i + 1]) << 4 | hex(bytes[i + 2]));
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Runs of anything but `A-Z a-z 0-9 . _ -` become one `_`; leading
/// characters that are not a letter or digit go, as do trailing dots and
/// dashes.
fn sanitize(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    out.trim_end_matches(['.', '-', '_']).to_string()
}

/// Cut the stem so the whole name fits, keeping the extension.
fn fit(name: &str) -> String {
    if name.len() <= MAX_FILE_NAME_LENGTH {
        return name.to_string();
    }
    // Everything here is ASCII by now, so any byte is a char boundary.
    match name.rsplit_once('.') {
        Some((stem, ext)) => {
            let room = MAX_FILE_NAME_LENGTH.saturating_sub(ext.len() + 1);
            let stem = stem[..room.min(stem.len())].trim_end_matches(['.', '-', '_']);
            format!("{stem}.{ext}")
        }
        None => name[..MAX_FILE_NAME_LENGTH].to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "aaaaaaaa11111111111111111111111111111111111111111111111111111111";
    const SHA_B: &str = "bbbbbbbb22222222222222222222222222222222222222222222222222222222";

    fn allowed() -> Vec<String> {
        ["exe", "msi", "zip", "7z", "msu", "iso"]
            .map(String::from)
            .to_vec()
    }

    fn source<'a>(url: &'a Url, kind: Kind, sha256: &'a str) -> Source<'a> {
        Source {
            url,
            final_url: None,
            content_disposition: None,
            file_type: None,
            kind,
            sha256,
            fallback: "app-1.0.0",
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn content_is_recognised() {
        assert_eq!(Kind::sniff(b"MZ\x90\x00"), Kind::Executable);
        assert_eq!(
            Kind::sniff(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]),
            Kind::Ole
        );
        assert_eq!(Kind::sniff(b"PK\x03\x04rest"), Kind::Zip);
        assert_eq!(Kind::sniff(b"MSCF\0\0"), Kind::Cabinet);
        assert_eq!(Kind::sniff(b"#!/bin"), Kind::Other);
        assert_eq!(Kind::sniff(b""), Kind::Other);
    }

    #[test]
    fn a_valid_original_name_is_kept() {
        let u = url("https://github.com/git-for-windows/git/releases/download/v2.51.0/Git-2.51.0-64-bit.exe");
        let name = Names::new().name(&source(&u, Kind::Executable, SHA_A), &allowed());
        assert_eq!(name.unwrap(), "Git-2.51.0-64-bit.exe");
        let u = url("https://example.com/dvd.iso");
        let name = Names::new().name(&source(&u, Kind::Other, SHA_A), &allowed());
        assert_eq!(name.unwrap(), "dvd.iso");
    }

    #[test]
    fn names_are_made_from_what_is_known() {
        // Spaces and `+`.
        let u = url("https://example.com/files/Setup%201.0+x64.exe");
        let n = Names::new().name(&source(&u, Kind::Executable, SHA_A), &allowed());
        assert_eq!(n.unwrap(), "Setup_1.0_x64.exe");

        // No name in the URL: Content-Disposition, `filename*` first.
        let u = url("https://example.com/download?product=app&os=win64");
        let mut s = source(&u, Kind::Ole, SHA_A);
        s.content_disposition =
            Some("attachment; filename=\"fallback.msi\"; filename*=UTF-8''App%20Installer.msi");
        assert_eq!(
            Names::new().name(&s, &allowed()).unwrap(),
            "App_Installer.msi"
        );

        // The URL after redirects, and the captured file type for the
        // extension.
        let u = url("https://example.com/latest");
        let fin = url("https://cdn.example.com/builds/app-x64");
        let mut s = source(&u, Kind::Executable, SHA_A);
        s.final_url = Some(&fin);
        s.file_type = Some("EXE");
        assert_eq!(Names::new().name(&s, &allowed()).unwrap(), "app-x64.exe");

        // Nothing at all: the package, and the content's own extension.
        let u = url("https://example.com/");
        let s = source(&u, Kind::Executable, SHA_A);
        assert_eq!(Names::new().name(&s, &allowed()).unwrap(), "app-1.0.0.exe");
    }

    #[test]
    fn the_extension_follows_the_content() {
        // A zip served as `.exe` is not an `.exe`.
        let u = url("https://example.com/tool.exe");
        let n = Names::new().name(&source(&u, Kind::Zip, SHA_A), &allowed());
        assert_eq!(n.unwrap(), "tool.zip");
        // An `.msu` is a cabinet, and keeps its name.
        let u = url("https://example.com/windows6.1-kb3033929-x64.msu");
        let n = Names::new().name(&source(&u, Kind::Cabinet, SHA_A), &allowed());
        assert_eq!(n.unwrap(), "windows6.1-kb3033929-x64.msu");
    }

    #[test]
    fn collisions_get_a_hash_prefix_and_repeats_their_name() {
        let mut names = Names::new();
        let a = url("https://one.example.com/setup.exe");
        let b = url("https://two.example.com/setup.exe");
        assert_eq!(
            names
                .name(&source(&a, Kind::Executable, SHA_A), &allowed())
                .unwrap(),
            "setup.exe"
        );
        assert_eq!(
            names
                .name(&source(&b, Kind::Executable, SHA_B), &allowed())
                .unwrap(),
            "bbbbbbbb-setup.exe"
        );
        // The same file asked for again keeps its name.
        assert_eq!(
            names
                .name(&source(&b, Kind::Executable, SHA_A), &allowed())
                .unwrap(),
            "setup.exe"
        );
    }

    #[test]
    fn a_refused_extension_is_reported_not_renamed() {
        let u = url("https://example.com/addin.vsix");
        let err = Names::new()
            .name(&source(&u, Kind::Zip, SHA_A), &allowed())
            .unwrap_err();
        assert!(err.contains("the feed takes no .vsix files"), "{err}");
        let u = url("https://example.com/script");
        let err = Names::new()
            .name(&source(&u, Kind::Other, SHA_A), &allowed())
            .unwrap_err();
        assert!(err.contains("takes no ."), "{err}");
    }

    #[test]
    fn long_and_odd_names_are_cut_to_fit() {
        let long = format!("https://example.com/{}.exe", "a".repeat(300));
        let u = url(&long);
        let n = Names::new()
            .name(&source(&u, Kind::Executable, SHA_A), &allowed())
            .unwrap();
        assert_eq!(n.len(), MAX_FILE_NAME_LENGTH);
        assert!(n.ends_with(".exe"));
        let u = url("https://example.com/caf%C3%A9%20%E2%80%AE.exe");
        let n = Names::new()
            .name(&source(&u, Kind::Executable, SHA_A), &allowed())
            .unwrap();
        assert_eq!(n, "caf.exe");
        let u = url("https://example.com/con.exe");
        let err = Names::new().name(&source(&u, Kind::Executable, SHA_A), &allowed());
        assert!(err.is_err(), "{err:?}");
    }
}
