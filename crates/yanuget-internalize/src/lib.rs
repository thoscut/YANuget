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

/// An error with its causes, `: `-separated, for a message that has room
/// for one line only (a check report, a sync's reason). A cause already in
/// the text is not repeated.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let c = cause.to_string();
        if !s.contains(&c) {
            s.push_str(": ");
            s.push_str(&c);
        }
        source = cause.source();
    }
    s
}

/// What the tool calls itself in HTTP requests.
pub const USER_AGENT: &str = concat!("yanuget-internalize/", env!("CARGO_PKG_VERSION"));
