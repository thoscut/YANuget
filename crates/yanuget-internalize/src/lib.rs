//! `yanuget-internalize`: internalizes Chocolatey packages into a YANuget
//! feed. See ROADMAP.md ("Chocolatey internalizer") for the plan this crate
//! follows; nothing here is released yet.
//!
//! The sandbox the package scripts run in sits behind [`sandbox::Sandbox`],
//! so everything around it builds and is tested on any host against
//! [`sandbox::fake::FakeSandbox`].

pub mod check;
pub mod cli;
pub mod config;
pub mod outcome;
pub mod sandbox;
pub mod upstream;

/// What the tool calls itself in HTTP requests.
pub const USER_AGENT: &str = concat!("yanuget-internalize/", env!("CARGO_PKG_VERSION"));
