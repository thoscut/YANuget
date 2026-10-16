//! NuGet package handling shared by the YANuget server and its tools.
//!
//! Nothing here depends on a web framework or a database: versions and
//! version ranges, the `.nuspec` parser, `.nupkg` reading, and the address
//! policy for upstream fetches. Errors are this crate's own [`Error`].

pub mod address;
pub mod error;
pub mod models;
pub mod nupkg;
pub mod nuspec;
pub mod range;
pub mod version;

pub use error::{Error, Result};
pub use range::VersionRange;
pub use version::NuGetVersion;
