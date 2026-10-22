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

/// What the tool calls itself in HTTP requests.
pub const USER_AGENT: &str = concat!("yanuget-internalize/", env!("CARGO_PKG_VERSION"));
