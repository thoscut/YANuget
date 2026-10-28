//! Overrides (`INT-25`): what a person tells the tool about one package that
//! the capture cannot find out, one TOML file per package id in
//! `overrides_dir`.
//!
//! ```toml
//! # overrides/git.install.toml
//! versions = "[2.51,)"              # the versions it applies to; all without
//! package_parameters = "/NoShellIntegration"
//! script = "git.install.ps1"        # replaces chocolateyInstall.ps1
//!
//! [[download]]                      # a download the capture misses
//! url = "https://example.com/extra.cab"
//! sha256 = "…"
//!
//! [[rewrite]]                       # fetch a captured URL from elsewhere
//! from = "https://example.com/gone/setup.exe"
//! to = "https://mirror.example.com/setup.exe"
//! ```
//!
//! or, instead of all that, `skip = "why"`: the package is left out of runs.
//! [`crate::sandbox::job`] puts the script and the root's parameters into
//! the capture, and [`crate::assess::assess`] adds the downloads and
//! rewrites to what it asked for. Every override used goes into the
//! package's report, and one that no longer matches what the capture saw is
//! a warning, so stale overrides surface.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use reqwest::Url;
use serde::Deserialize;
use yanuget_pkg::{NuGetVersion, VersionRange};

use crate::assess::Wanted;
use crate::capture::{Checksum, ChecksumType};
use crate::report::Draft;

/// The largest replacement script taken.
pub const MAX_SCRIPT_BYTES: u64 = 1024 * 1024;

/// The longest package parameters taken.
const MAX_PARAMETER_CHARS: usize = 4096;

/// Why the overrides cannot be used.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct OverrideError(pub String);

/// An override file as written.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    versions: Option<String>,
    skip: Option<String>,
    package_parameters: Option<String>,
    script: Option<String>,
    #[serde(default)]
    download: Vec<ExtraFile>,
    #[serde(default)]
    rewrite: Vec<RewriteFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExtraFile {
    url: String,
    sha256: Option<String>,
    file_type: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RewriteFile {
    from: String,
    to: String,
}

/// One package's override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Override {
    /// The package id, from the file name.
    pub id: String,
    /// The versions it applies to; all when `None`.
    pub versions: Option<VersionRange>,
    /// Leave the package out of runs, for this reason.
    pub skip: Option<String>,
    /// Passed to the install as `--params` when the package is the root.
    pub package_parameters: Option<String>,
    /// A replacement for `chocolateyInstall.ps1`: its file name and content.
    pub script: Option<(String, Vec<u8>)>,
    /// Downloads the capture misses.
    pub downloads: Vec<Extra>,
    /// Captured URLs to fetch from elsewhere.
    pub rewrites: Vec<UrlRewrite>,
}

/// A download the capture misses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extra {
    pub url: Url,
    pub sha256: Option<String>,
    pub file_type: Option<String>,
}

/// A captured URL fetched from elsewhere. The package keeps asking for
/// `from`, which the prelude maps to the attached file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlRewrite {
    pub from: Url,
    pub to: Url,
}

/// Every override in a directory, by lower-case package id.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    by_id: BTreeMap<String, Override>,
}

impl Overrides {
    /// No overrides.
    pub fn none() -> Self {
        Self::default()
    }

    /// Every `*.toml` file in `dir`, each named for its package id. Any file
    /// that does not parse or check out fails the whole set, so a run never
    /// goes ahead with an override silently left out.
    pub fn load(dir: &Path) -> Result<Self, OverrideError> {
        let entries = std::fs::read_dir(dir)
            .map_err(|e| OverrideError(format!("overrides_dir {}: {e}", dir.display())))?;
        let mut by_id = BTreeMap::new();
        let mut errors = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|e| OverrideError(format!("overrides_dir {}: {e}", dir.display())))?
                .path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".toml"))
            else {
                continue;
            };
            match load_one(dir, id, &path) {
                Ok(o) => {
                    // Ids ignore case, so `Git.toml` and `git.toml` would
                    // have one replace the other at random.
                    if let Some(earlier) = by_id.insert(id.to_ascii_lowercase(), o) {
                        let mut names = [format!("{}.toml", earlier.id), format!("{id}.toml")];
                        names.sort();
                        errors.push(format!(
                            "{} and {} are for the same package; keep one",
                            names[0], names[1]
                        ));
                    }
                }
                Err(e) => errors.push(e),
            }
        }
        if errors.is_empty() {
            Ok(Overrides { by_id })
        } else {
            errors.sort();
            Err(OverrideError(errors.join("; ")))
        }
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// The override for `id` `version`, if one applies to that version.
    pub fn get(&self, id: &str, version: &NuGetVersion) -> Option<&Override> {
        self.by_id
            .get(&id.to_ascii_lowercase())
            .filter(|o| o.versions.as_ref().is_none_or(|r| r.contains(version)))
    }

    /// A hash of every override, scripts included: it changes when any file
    /// does, which tells `sync` (`INT-27`) to try a package that needed an
    /// override again.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(format!("{:?}", self.by_id)))
    }
}

fn load_one(dir: &Path, id: &str, path: &Path) -> Result<Override, String> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let fail = |m: String| format!("{name}: {m}");
    if !crate::cli::is_valid_id(id) {
        return Err(fail("the file name is not a package id".into()));
    }
    let text = std::fs::read_to_string(path).map_err(|e| fail(e.to_string()))?;
    let file: File = toml::from_str(&text).map_err(|e| fail(e.message().to_string()))?;

    let versions = file
        .versions
        .map(|v| VersionRange::parse(&v).map_err(|e| fail(format!("versions {v:?}: {e}"))))
        .transpose()?;
    let skip = file
        .skip
        .map(|s| s.trim().to_string())
        .map(|s| {
            if s.is_empty() || s.chars().any(char::is_control) {
                Err(fail("skip needs a reason on one line".into()))
            } else {
                Ok(s)
            }
        })
        .transpose()?;
    let package_parameters = file
        .package_parameters
        .map(|p| {
            if p.chars().count() > MAX_PARAMETER_CHARS || p.chars().any(char::is_control) {
                Err(fail(format!(
                    "package_parameters must be one line of at most {MAX_PARAMETER_CHARS} characters"
                )))
            } else {
                Ok(p)
            }
        })
        .transpose()?;
    let script = file
        .script
        .map(|s| read_script(dir, &s).map_err(fail))
        .transpose()?;
    let downloads = file
        .download
        .into_iter()
        .map(|d| {
            Ok(Extra {
                url: url_of("download.url", &d.url).map_err(fail)?,
                sha256: d
                    .sha256
                    .map(|h| {
                        let hex = h.trim().to_ascii_lowercase();
                        if hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                            Ok(hex)
                        } else {
                            Err(fail(format!(
                                "download.sha256 {h:?} is not a SHA-256 in hex"
                            )))
                        }
                    })
                    .transpose()?,
                file_type: d
                    .file_type
                    .map(|t| {
                        if !t.is_empty()
                            && t.len() <= 16
                            && t.bytes().all(|b| b.is_ascii_alphanumeric())
                        {
                            Ok(t.to_ascii_lowercase())
                        } else {
                            Err(fail(format!("download.file_type {t:?} is not a file type")))
                        }
                    })
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let rewrites = file
        .rewrite
        .into_iter()
        .map(|r| {
            Ok(UrlRewrite {
                from: url_of("rewrite.from", &r.from).map_err(fail)?,
                to: url_of("rewrite.to", &r.to).map_err(fail)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    if skip.is_some()
        && (package_parameters.is_some()
            || script.is_some()
            || !downloads.is_empty()
            || !rewrites.is_empty())
    {
        return Err(fail(
            "skip leaves the package out, so nothing else in the file would apply".into(),
        ));
    }
    Ok(Override {
        id: id.to_string(),
        versions,
        skip,
        package_parameters,
        script,
        downloads,
        rewrites,
    })
}

fn url_of(name: &str, raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("{name} {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none_or(str::is_empty) {
        return Err(format!("{name} {raw:?} must be an http or https URL"));
    }
    Ok(url)
}

/// A replacement script: a plain file name or relative path under `dir`.
fn read_script(dir: &Path, raw: &str) -> Result<(String, Vec<u8>), String> {
    let rel = Path::new(raw);
    if raw.is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(format!(
            "script {raw:?} must be a path inside the overrides directory"
        ));
    }
    let path = dir.join(rel);
    let size = std::fs::metadata(&path)
        .map_err(|e| format!("script {raw:?}: {e}"))?
        .len();
    if size > MAX_SCRIPT_BYTES {
        return Err(format!(
            "script {raw:?} is larger than {} KiB",
            MAX_SCRIPT_BYTES / 1024
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("script {raw:?}: {e}"))?;
    Ok((raw.to_string(), bytes))
}

/// Where Chocolatey finds the install script when a package has none.
const INSTALL_SCRIPT: &str = "tools/chocolateyInstall.ps1";

/// Write the package at `nupkg` to `out` with its `chocolateyInstall.ps1`
/// replaced by `script`, so the capture runs the override's script and the
/// feed gets it. The package's own entry keeps its name; a package without
/// one gets `tools/chocolateyInstall.ps1`.
pub async fn with_script(
    nupkg: &Path,
    out: &Path,
    script: Vec<u8>,
) -> Result<(), yanuget_pkg::Error> {
    let (nupkg, out) = (nupkg.to_path_buf(), out.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let archive = yanuget_pkg::nupkg::open_archive(&nupkg)?;
        let mut existing = None;
        for i in 0..archive.len() {
            if let Some(name) = yanuget_pkg::nupkg::entry_name(&archive, i)? {
                let file = name.rsplit(['/', '\\']).next().unwrap_or(&name);
                if file.eq_ignore_ascii_case("chocolateyInstall.ps1") {
                    existing = Some(name);
                    break;
                }
            }
        }
        drop(archive);
        let name = existing.unwrap_or_else(|| INSTALL_SCRIPT.to_string());
        yanuget_pkg::rewrite::rewrite_blocking(
            &nupkg,
            &out,
            &yanuget_pkg::rewrite::Rewrite::new()
                .put(name, yanuget_pkg::rewrite::Source::Bytes(script)),
        )
        .map(|_| ())
    })
    .await
    .map_err(|e| yanuget_pkg::Error::Task(format!("script replacement panicked: {e}")))?
}

impl Override {
    /// Apply the downloads and rewrites to what the capture asked for, and
    /// note in the report what was used and what no longer matches.
    /// `root` says whether the package was the capture's root, the only one
    /// whose install got the package parameters.
    pub fn apply(&self, draft: &mut Draft, wanted: &mut Vec<Wanted>, root: bool) {
        if let Some((name, _)) = &self.script {
            draft.overridden(format!("chocolateyInstall.ps1 replaced by {name}"));
        }
        match &self.package_parameters {
            Some(p) if root => draft.overridden(format!("package parameters {p:?}")),
            Some(p) => draft.warn(format!(
                "the override's package parameters {p:?} were not passed: \
                 the package was installed as a dependency, and only the root gets --params"
            )),
            None => {}
        }
        for r in &self.rewrites {
            let mut hit = false;
            for w in wanted.iter_mut().filter(|w| w.url == r.from) {
                w.fetch_from = Some(r.to.clone());
                hit = true;
            }
            if hit {
                draft.overridden(format!("{} fetched from {}", r.from, r.to));
            } else {
                draft.warn(format!(
                    "the override's rewrite of {} matches no captured URL; it may be stale",
                    r.from
                ));
            }
        }
        for d in &self.downloads {
            if wanted.iter().any(|w| w.url == d.url) {
                draft.warn(format!(
                    "the override's download {} was captured anyway; it may be stale",
                    d.url
                ));
                continue;
            }
            draft.overridden(format!("download {} added", d.url));
            wanted.push(Wanted {
                url: d.url.clone(),
                checksum: d.sha256.as_ref().map(|hex| Checksum {
                    kind: ChecksumType::Sha256,
                    hex: hex.clone(),
                }),
                helper: None,
                file_type: d.file_type.clone(),
                fetch_from: None,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::Helper;
    use yanuget_pkg::report::{PackageRef, Upstream};

    fn dir_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    fn v(s: &str) -> NuGetVersion {
        NuGetVersion::parse(s).unwrap()
    }

    #[test]
    fn a_directory_of_overrides_loads() {
        let dir = dir_with(&[
            (
                "Git.Install.toml",
                r#"
versions = "[2.51,)"
package_parameters = "/NoShellIntegration"
script = "git.ps1"

[[download]]
url = "https://example.com/extra.cab"
sha256 = "AA"
"#,
            ),
            ("git.ps1", "Write-Host internal"),
            ("readme.md", "not an override"),
            ("vlc.toml", "skip = \"installed by the desktop team\""),
        ]);
        // The bad SHA-256 fails the whole set, named by file.
        let err = Overrides::load(dir.path()).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Git.Install.toml: download.sha256 \"AA\""),
            "{err}"
        );

        std::fs::write(
            dir.path().join("Git.Install.toml"),
            format!(
                "versions = \"[2.51,)\"\nscript = \"git.ps1\"\n[[download]]\nurl = \"https://example.com/extra.cab\"\nsha256 = \"{}\"\n",
                "A".repeat(64)
            ),
        )
        .unwrap();
        let o = Overrides::load(dir.path()).unwrap();
        assert_eq!(o.len(), 2);
        let git = o.get("git.install", &v("2.51.0")).unwrap();
        assert_eq!(git.id, "Git.Install");
        assert_eq!(git.downloads[0].sha256.as_deref(), Some(&*"a".repeat(64)));
        assert_eq!(
            git.script,
            Some(("git.ps1".into(), b"Write-Host internal".to_vec()))
        );
        assert!(o.get("git.install", &v("2.50.0")).is_none(), "out of range");
        assert_eq!(
            o.get("VLC", &v("3.0.0")).unwrap().skip.as_deref(),
            Some("installed by the desktop team")
        );
    }

    #[test]
    fn bad_files_are_refused() {
        for (name, text, why) in [
            ("a..b.toml", "skip = \"x\"", "not a package id"),
            ("a.toml", "skipp = \"x\"", "unknown field"),
            ("a.toml", "skip = \" \"", "skip needs a reason"),
            ("a.toml", "versions = \"[2,1]\"", "versions"),
            (
                "a.toml",
                "script = \"../outside.ps1\"",
                "inside the overrides",
            ),
            ("a.toml", "script = \"missing.ps1\"", "missing.ps1"),
            (
                "a.toml",
                "skip = \"x\"\npackage_parameters = \"/a\"",
                "nothing else",
            ),
            (
                "a.toml",
                "[[rewrite]]\nfrom = \"ftp://x/a\"\nto = \"https://y/a\"",
                "http or https",
            ),
            ("a.toml", "package_parameters = \"a\\nb\"", "one line"),
        ] {
            let dir = dir_with(&[(name, text)]);
            let err = Overrides::load(dir.path()).unwrap_err().to_string();
            assert!(err.contains(why), "{name} {text:?}: {err}");
        }
        assert!(Overrides::load(Path::new("/nonexistent/overrides")).is_err());

        // Two files for one id: neither silently wins. (On a file system
        // that ignores case they cannot both exist.)
        let dir = dir_with(&[("git.toml", "skip = \"a\""), ("Git.toml", "skip = \"b\"")]);
        if std::fs::read_dir(dir.path()).unwrap().count() == 2 {
            let err = Overrides::load(dir.path()).unwrap_err().to_string();
            assert!(
                err.contains("Git.toml and git.toml are for the same package"),
                "{err}"
            );
        }
    }

    fn wanted(url: &str) -> Wanted {
        Wanted {
            url: Url::parse(url).unwrap(),
            checksum: None,
            helper: Some(Helper::InstallChocolateyPackage),
            file_type: None,
            fetch_from: None,
        }
    }

    #[test]
    fn overrides_change_the_downloads_and_say_so() {
        let o = Override {
            id: "tool".into(),
            versions: None,
            skip: None,
            package_parameters: Some("/Quiet".into()),
            script: None,
            downloads: vec![
                Extra {
                    url: Url::parse("https://example.com/extra.cab").unwrap(),
                    sha256: Some("b".repeat(64)),
                    file_type: Some("cab".into()),
                },
                Extra {
                    url: Url::parse("https://example.com/setup.exe").unwrap(),
                    sha256: None,
                    file_type: None,
                },
            ],
            rewrites: vec![
                UrlRewrite {
                    from: Url::parse("https://example.com/setup.exe").unwrap(),
                    to: Url::parse("https://mirror.example.com/setup.exe").unwrap(),
                },
                UrlRewrite {
                    from: Url::parse("https://example.com/old.exe").unwrap(),
                    to: Url::parse("https://mirror.example.com/old.exe").unwrap(),
                },
            ],
        };
        let mut draft = Draft::new(
            PackageRef {
                id: "tool".into(),
                version: "1.0.0".into(),
            },
            Upstream {
                source: "s".into(),
                sha512: None,
            },
        );
        let mut ws = vec![wanted("https://example.com/setup.exe")];
        o.apply(&mut draft, &mut ws, true);

        assert_eq!(ws.len(), 2);
        assert_eq!(
            ws[0].fetch_from.as_ref().unwrap().as_str(),
            "https://mirror.example.com/setup.exe"
        );
        assert_eq!(ws[1].url.as_str(), "https://example.com/extra.cab");
        assert_eq!(ws[1].checksum.as_ref().unwrap().kind, ChecksumType::Sha256);
        assert_eq!(ws[1].helper, None);

        let r = draft.finish("t");
        assert_eq!(
            r.overrides,
            [
                "package parameters \"/Quiet\"",
                "https://example.com/setup.exe fetched from https://mirror.example.com/setup.exe",
                "download https://example.com/extra.cab added",
            ]
        );
        assert_eq!(
            r.warnings,
            [
                "the override's rewrite of https://example.com/old.exe matches no captured URL; it may be stale",
                "the override's download https://example.com/setup.exe was captured anyway; it may be stale",
            ]
        );
    }
}
