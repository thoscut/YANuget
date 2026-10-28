//! YANuget — Yet Another NuGet server.
//!
//! A fast, streaming implementation of the NuGet V3 HTTP protocol, written in
//! Rust as a reimplementation of [BaGetter](https://github.com/bagetter/BaGetter).
//!
//! The crate is organized into focused modules:
//!
//! * [`version`] — NuGet version parsing, normalization and ordering.
//! * [`error`] — the crate-wide error type.
//!
//! More modules (models, storage, database, protocol, web) are wired in as the
//! server is built up.

pub mod admin_client;
pub mod auth;
pub mod backup;
pub mod config;
pub mod config_check;
pub mod database;
pub mod error;
pub mod feed_data;
pub mod inbox;
pub mod indexing;
pub mod locks;
pub mod migrate;
pub mod mirror;
pub mod models;
pub mod nuget;
pub mod pdb;
pub mod pe;
pub mod policy;
pub mod proxy;
pub mod ratelimit;
pub mod retention;
pub mod server;
pub mod storage;
pub mod streaming;
pub mod symbols;
pub mod tls;
pub mod validation;
pub mod web;

pub use error::{Error, Result};
pub use version::NuGetVersion;

// Package handling lives in the `yanuget-pkg` crate, shared with the tools that
// build packages for the server; these keep the server's paths unchanged.
pub use yanuget_pkg::{nupkg, nuspec, range, report, version};
