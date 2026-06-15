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

pub mod database;
pub mod error;
pub mod models;
pub mod nuget;
pub mod nupkg;
pub mod nuspec;
pub mod storage;
pub mod streaming;
pub mod version;

pub use error::{Error, Result};
pub use version::NuGetVersion;
