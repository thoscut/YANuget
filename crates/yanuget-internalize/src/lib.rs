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
}
