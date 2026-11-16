//! `yanuget-internalize`: internalizes Chocolatey packages into a YANuget
//! feed. See ROADMAP.md ("Chocolatey internalizer") for the plan this crate
//! follows; nothing here is released yet.
//!
//! The sandbox the package scripts run in sits behind [`sandbox::Sandbox`],
//! so everything around it builds and is tested on any host against
//! [`sandbox::fake::FakeSandbox`].

pub mod assess;
pub mod capture;
pub mod check;
pub mod cli;
pub mod closure;
pub mod config;
pub mod crosscheck;
pub mod download;
pub mod naming;
pub mod outcome;
pub mod overrides;
pub mod prelude;
pub mod publish;
pub mod report;
pub mod sandbox;
pub mod sync;
pub mod target;
pub mod upstream;

/// An error with its causes on one line, as the shared crate has it.
pub use yanuget_pkg::error::error_chain;

/// Name what failed in `e` when it is an I/O error, as in `cannot read the
/// package /work/a.1.0.0.nupkg: No such file or directory (os error 2)`.
///
/// The shared crate's I/O error is the operating system's message and
/// nothing else, which does not say which file a run was working on. The
/// context goes into the I/O error itself, so the error type, the kind and
/// the cause behind it stay what they were.
pub(crate) fn io_context(
    what: impl FnOnce() -> String,
) -> impl FnOnce(yanuget_pkg::Error) -> yanuget_pkg::Error {
    move |e| match e {
        yanuget_pkg::Error::Io(source) => yanuget_pkg::Error::Io(std::io::Error::new(
            source.kind(),
            IoContext {
                what: what(),
                source,
            },
        )),
        e => e,
    }
}

/// What [`io_context`] puts in front of an I/O error.
#[derive(Debug, thiserror::Error)]
#[error("{what}")]
struct IoContext {
    what: String,
    #[source]
    source: std::io::Error,
}

/// What the tool calls itself in HTTP requests.
pub const USER_AGENT: &str = concat!("yanuget-internalize/", env!("CARGO_PKG_VERSION"));

#[cfg(test)]
mod tests {
    use super::*;

    fn denied() -> std::io::Error {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied")
    }

    /// `MNT-30`: an error that has its cause as a source does not spell it
    /// out in its own text as well, so the cause is printed once, by `{:#}`
    /// (as `main` prints what a command returns) and by [`error_chain`]
    /// alike.
    #[test]
    fn a_cause_is_printed_once() {
        let pkg = || yanuget_pkg::Error::from(denied());
        let cases: Vec<(anyhow::Error, &str)> = vec![
            (
                upstream::UpstreamError::from(denied()).into(),
                "cache: permission denied",
            ),
            (
                download::DownloadError::from(denied()).into(),
                "download cache: permission denied",
            ),
            (
                download::DownloadError::from(upstream::UpstreamError::from(denied())).into(),
                "cache: permission denied",
            ),
            (
                sandbox::SandboxError::from(denied()).into(),
                "sandbox I/O error: permission denied",
            ),
            (
                publish::PublishError::from(denied()).into(),
                "work directory: permission denied",
            ),
            (
                publish::PublishError::from(pkg()).into(),
                "cannot rewrite the package: permission denied",
            ),
            (
                publish::PublishError::from(yanuget_pkg::Error::InvalidRewrite("no".into())).into(),
                "cannot rewrite the package: invalid rewrite: no",
            ),
            (
                prelude::PreludeError::from(pkg()).into(),
                "permission denied",
            ),
            (pkg().into(), "permission denied"),
        ];
        for (e, want) in cases {
            assert_eq!(format!("{e:#}"), want);
            assert_eq!(error_chain(e.as_ref()), want);
        }
    }

    /// An I/O error from the shared crate says which package it was reading,
    /// and is still the I/O error it was, with its kind and its cause.
    #[tokio::test]
    async fn an_unreadable_package_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone.1.0.0.nupkg");
        let named = format!("cannot read the package {}", missing.display());
        let scan = crosscheck::scan(&missing).await.unwrap_err();
        let replaced = overrides::with_script(&missing, &dir.path().join("out.nupkg"), Vec::new())
            .await
            .unwrap_err();
        for e in [scan, replaced] {
            let shown = error_chain(&e);
            assert!(shown.starts_with(&format!("{named}: ")), "{shown}");
            // The operating system's message follows, once (ENOENT and
            // ERROR_FILE_NOT_FOUND are both 2).
            assert!(shown.ends_with("(os error 2)"), "{shown}");
            assert_eq!(shown.matches("os error").count(), 1, "{shown}");
            assert!(
                matches!(&e, yanuget_pkg::Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound),
                "{e:?}"
            );
        }
    }
}
