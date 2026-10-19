//! From a capture to each package's report and files: what the capture says
//! about the packages it installed, then the downloads it asked for, fetched
//! and named.
//!
//! A capture covers a whole closure: the root's install runs its
//! dependencies' scripts too, and the capture helpers attribute each call to
//! the package whose script made it. So a capture that cannot be trusted as
//! a whole (the sandbox failed, the install failed or ran out of time, the
//! capture is malformed) marks every package it was to tell about; a single
//! call only marks its own package.

use std::path::PathBuf;

use futures_util::StreamExt;
use reqwest::Url;
use yanuget_pkg::report::Download;
use yanuget_pkg::NuGetVersion;

use crate::capture::{self, Call, Checksum, Helper, Origin};
use crate::download::Downloader;
use crate::naming::{Kind, Names, Source};
use crate::report::Draft;
use crate::sandbox::{RawCapture, SandboxError};

/// The exit codes Chocolatey ends a successful install with: done, and done
/// with a reboot pending or started.
const SUCCESS_CODES: [i32; 3] = [0, 1641, 3010];

/// One download a script asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wanted {
    pub url: Url,
    pub checksum: Option<Checksum>,
    /// The helper that asked; `None` for a download an override adds.
    pub helper: Option<Helper>,
    /// The captured `fileType`.
    pub file_type: Option<String>,
    /// Where to fetch it instead, by an override's rewrite. The package keeps
    /// asking for `url`.
    pub fetch_from: Option<Url>,
}

/// A downloaded file and the name it is attached under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    /// Lower-case hex.
    pub sha256: String,
}

/// Apply a capture to the drafts of the packages it was to tell about, and
/// return the downloads each of them asked for, in the order of `drafts`.
///
/// `closure` is every package the install saw, `(id, version)`, those the
/// target holds already included: their calls are attributed, then left
/// alone. `timeout_secs` is the sandbox's limit, for the report.
pub fn assess(
    capture: Result<RawCapture, SandboxError>,
    closure: &[(&str, &NuGetVersion)],
    drafts: &mut [Draft],
    timeout_secs: u64,
) -> Vec<Vec<Wanted>> {
    let mut wanted = vec![Vec::new(); drafts.len()];
    let raw = match capture {
        Ok(raw) => raw,
        Err(e) => {
            for d in drafts.iter_mut() {
                d.fail(format!("the capture failed: {e}"));
            }
            return wanted;
        }
    };
    for d in drafts.iter_mut() {
        d.environment(raw.environment.clone());
    }
    // A failed or unfinished install may have stopped before a script asked
    // for everything it needs: what was captured is not the whole story.
    let incomplete = if raw.timed_out {
        Some(format!(
            "the install did not finish within sandbox.timeout_secs ({timeout_secs} s)"
        ))
    } else {
        match raw.exit_code {
            Some(code) if SUCCESS_CODES.contains(&code) => None,
            Some(code) => Some(format!("the install failed with exit code {code}")),
            None => Some("the install did not report an exit code".to_string()),
        }
    };
    if let Some(why) = &incomplete {
        for d in drafts.iter_mut() {
            d.needs_override(why.clone());
        }
    }
    let calls = match capture::parse(&raw.calls, closure) {
        Ok(calls) => calls,
        Err(rejected) => {
            for d in drafts.iter_mut() {
                for reason in &rejected.reasons {
                    d.needs_override(format!("the capture is not usable: {reason}"));
                }
            }
            return wanted;
        }
    };
    for call in calls {
        let version = call.version.normalized();
        let Some(i) = drafts.iter().position(|d| d.is(&call.package, &version)) else {
            continue;
        };
        add_call(&mut drafts[i], &mut wanted[i], call);
    }
    wanted
}

fn add_call(draft: &mut Draft, wanted: &mut Vec<Wanted>, call: Call) {
    let Call {
        helper,
        url,
        url64,
        checksum,
        checksum64,
        file_type,
        origin,
        ..
    } = call;
    let downloads = [(url, checksum), (url64, checksum64)];
    if downloads.iter().all(|(u, _)| u.is_none()) {
        // A helper that works on a local file: nothing to fetch.
        return;
    }
    // The prelude redirects the package's own calls; a call from anywhere
    // else would still go to the internet at install time.
    match &origin {
        Origin::Package => {}
        Origin::Extension(name) => {
            draft.needs_override(format!(
                "{} was called from inside the extension {name}, which the prelude cannot redirect",
                helper.as_str()
            ));
        }
        Origin::Other(caller) => {
            draft.needs_override(format!(
                "{} was called from outside the package's scripts ({})",
                helper.as_str(),
                caller.as_deref().unwrap_or("the caller is unknown")
            ));
        }
    }
    for (url, checksum) in downloads {
        let Some(url) = url else { continue };
        let w = Wanted {
            url,
            checksum,
            helper: Some(helper),
            file_type: file_type.clone(),
            fetch_from: None,
        };
        // `url64` is often `url` again; one download serves both.
        if !wanted
            .iter()
            .any(|x| x.url == w.url && x.checksum == w.checksum)
        {
            wanted.push(w);
        }
    }
}

/// Download what each package asked for, name the files, and record both in
/// its draft. Returns each package's attachments, in the order of `drafts`;
/// a file several downloads share is attached once.
///
/// `allowed` is the target feed's `allowed_extensions`; `concurrency` bounds
/// the downloads in flight across all packages.
pub async fn fetch(
    wanted: Vec<Vec<Wanted>>,
    drafts: &mut [Draft],
    downloader: &Downloader,
    allowed: &[String],
    concurrency: usize,
) -> Vec<Vec<Attachment>> {
    let jobs: Vec<(usize, Wanted)> = wanted
        .into_iter()
        .enumerate()
        .flat_map(|(i, ws)| ws.into_iter().map(move |w| (i, w)))
        .collect();
    let results: Vec<_> = futures_util::stream::iter(jobs)
        .map(|(i, w)| async move {
            let from = w.fetch_from.as_ref().unwrap_or(&w.url);
            let got = downloader.fetch(from, w.checksum.as_ref()).await;
            (i, w, got)
        })
        .buffered(concurrency.max(1))
        .collect()
        .await;

    let mut names: Vec<Names> = drafts.iter().map(|_| Names::new()).collect();
    let mut attachments = vec![Vec::<Attachment>::new(); drafts.len()];
    for (i, w, got) in results {
        let draft = &mut drafts[i];
        let file = match got {
            Ok(file) => file,
            Err(e) => {
                draft.fail(e.to_string());
                continue;
            }
        };
        let fallback = format!("{}-{}", draft.package().id, draft.package().version);
        let named = names[i].name(
            &Source {
                url: &w.url,
                final_url: file.final_url.as_ref(),
                content_disposition: file.content_disposition.as_deref(),
                file_type: w.file_type.as_deref(),
                kind: Kind::sniff(&file.head),
                sha256: &file.sha256,
                fallback: &fallback,
            },
            allowed,
        );
        let name = match named {
            Ok(name) => {
                if !attachments[i].iter().any(|a| a.name == name) {
                    attachments[i].push(Attachment {
                        name: name.clone(),
                        path: file.path.clone(),
                        size: file.size,
                        sha256: file.sha256.clone(),
                    });
                }
                Some(name)
            }
            Err(why) => {
                // Renaming it to pass would hide what it is; a person decides.
                draft.needs_override(format!("{} is not attached: {why}", w.url));
                None
            }
        };
        draft.download(Download {
            url: w.url.to_string(),
            helper: Some(w.helper.map_or("override", Helper::as_str).to_string()),
            file: name,
            size: Some(file.size),
            sha256: Some(file.sha256),
            checksum: file.checksum,
        });
    }
    attachments
}

#[cfg(test)]
mod tests {
    use super::*;
    use yanuget_pkg::report::{Environment, PackageRef, Status, Upstream};

    fn v(s: &str) -> NuGetVersion {
        NuGetVersion::parse(s).unwrap()
    }

    fn draft(id: &str, version: &str) -> Draft {
        Draft::new(
            PackageRef {
                id: id.into(),
                version: version.into(),
            },
            Upstream {
                source: "https://community.chocolatey.org/api/v2/".into(),
                sha512: None,
            },
        )
    }

    fn line(package: &str, version: &str, url: &str, extra: &str) -> String {
        format!(
            r#"{{"schema":1,"package":"{package}","version":"{version}","helper":"Install-ChocolateyPackage","url":"{url}","caller":"C:\\ProgramData\\chocolatey\\lib\\{package}\\tools\\chocolateyInstall.ps1"{extra}}}"#
        )
    }

    fn raw(lines: &[String]) -> RawCapture {
        RawCapture {
            exit_code: Some(0),
            timed_out: false,
            calls: lines.join("\n").into_bytes(),
            log: String::new(),
            environment: Environment {
                image_sha256: Some("00".repeat(32)),
                chocolatey_version: Some("2.4.3".into()),
                tool_version: None,
            },
        }
    }

    #[test]
    fn calls_go_to_their_packages() {
        let (git, core, held) = (v("2.51.0"), v("2.4.0"), v("1.0.0"));
        let closure = [
            ("git.install", &git),
            ("chocolatey-core.extension", &core),
            ("held", &held),
        ];
        let mut drafts = [
            draft("git.install", "2.51.0"),
            draft("chocolatey-core.extension", "2.4.0"),
        ];
        let sum = "a".repeat(64);
        let capture = raw(&[
            line(
                "Git.Install",
                "2.51.0",
                "https://example.com/git-64.exe",
                &format!(
                    r#","url64":"https://example.com/git-64.exe","checksum":"{sum}","checksumType":"sha256","checksum64":"{sum}","checksumType64":"sha256""#
                ),
            ),
            line(
                "git.install",
                "2.51.0",
                "https://example.com/git-32.exe",
                "",
            ),
            // The package the target holds: attributed, then left alone.
            line("held", "1.0.0", "https://example.com/held.exe", ""),
        ]);
        let wanted = assess(Ok(capture), &closure, &mut drafts, 60);
        let urls: Vec<Vec<&str>> = wanted
            .iter()
            .map(|ws| ws.iter().map(|w| w.url.as_str()).collect())
            .collect();
        assert_eq!(
            urls,
            [
                vec![
                    "https://example.com/git-64.exe",
                    "https://example.com/git-32.exe"
                ],
                vec![]
            ]
        );
        assert_eq!(wanted[0][0].checksum.as_ref().unwrap().hex, sum);
        let report = drafts[0].finish("t");
        assert_eq!(report.status, Status::Unchanged, "nothing downloaded yet");
        assert_eq!(
            report.environment.unwrap().chocolatey_version.as_deref(),
            Some("2.4.3")
        );
        assert_eq!(drafts[1].status(), Status::Unchanged);
    }

    #[test]
    fn calls_from_elsewhere_need_an_override() {
        let git = v("2.51.0");
        let closure = [("git.install", &git)];
        let mut drafts = [draft("git.install", "2.51.0")];
        let from = |caller: &str| {
            format!(
                r#"{{"schema":1,"package":"git.install","version":"2.51.0","helper":"Get-ChocolateyWebFile","url":"https://example.com/a.zip","caller":"{caller}"}}"#
            )
        };
        let capture = raw(&[
            from(r"C:\\ProgramData\\chocolatey\\extensions\\chocolatey-core\\x.psm1"),
            from(r"C:\\Temp\\x.ps1"),
            // No URL: a local file, whoever calls.
            r#"{"schema":1,"package":"git.install","version":"2.51.0","helper":"Get-ChocolateyUnzip","caller":"C:\\Temp\\x.ps1"}"#.to_string(),
        ]);
        assess(Ok(capture), &closure, &mut drafts, 60);
        let r = drafts[0].finish("t");
        assert_eq!(r.status, Status::NeedsOverride);
        assert_eq!(
            r.reasons,
            [
                "Get-ChocolateyWebFile was called from inside the extension chocolatey-core, which the prelude cannot redirect",
                r"Get-ChocolateyWebFile was called from outside the package's scripts (C:\Temp\x.ps1)",
            ]
        );
    }

    #[test]
    fn an_untrustworthy_capture_marks_every_package() {
        let (a, b) = (v("1.0.0"), v("2.0.0"));
        let closure = [("a", &a), ("b", &b)];
        let drafts = || [draft("a", "1.0.0"), draft("b", "2.0.0")];
        let reasons = |capture: Result<RawCapture, SandboxError>| {
            let mut ds = drafts();
            assess(capture, &closure, &mut ds, 600);
            ds.map(|d| {
                let r = d.finish("t");
                (r.status, r.reasons)
            })
        };

        let [(s, why), (s2, _)] = reasons(Err(SandboxError::Failed("guest crashed".into())));
        assert_eq!((s, s2), (Status::Failed, Status::Failed));
        assert_eq!(why, ["the capture failed: sandbox failed: guest crashed"]);

        let mut timed_out = raw(&[]);
        timed_out.timed_out = true;
        timed_out.exit_code = None;
        let [(s, why), _] = reasons(Ok(timed_out));
        assert_eq!(s, Status::NeedsOverride);
        assert_eq!(
            why,
            ["the install did not finish within sandbox.timeout_secs (600 s)"]
        );

        let mut failed = raw(&[]);
        failed.exit_code = Some(1);
        let [(s, why), (s2, _)] = reasons(Ok(failed));
        assert_eq!((s, s2), (Status::NeedsOverride, Status::NeedsOverride));
        assert_eq!(why, ["the install failed with exit code 1"]);

        let mut reboot = raw(&[]);
        reboot.exit_code = Some(3010);
        assert_eq!(reasons(Ok(reboot))[0].0, Status::Unchanged);

        let bad = raw(&["{\"schema\":2}".to_string()]);
        let [(s, why), (s2, _)] = reasons(Ok(bad));
        assert_eq!((s, s2), (Status::NeedsOverride, Status::NeedsOverride));
        assert_eq!(why.len(), 1);
        assert!(
            why[0].starts_with("the capture is not usable: line 1:"),
            "{why:?}"
        );
    }
}
