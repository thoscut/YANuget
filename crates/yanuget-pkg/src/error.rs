//! The crate's error type.

/// What can go wrong reading a package.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The package, or its manifest, is malformed or refused.
    #[error("invalid package: {0}")]
    InvalidPackage(String),

    /// A rewrite asked for something the package cannot take: a name that is
    /// not allowed, an entry it does not have, a manifest that does not parse.
    /// A fault of the caller, not of the package.
    #[error("invalid rewrite: {0}")]
    InvalidRewrite(String),

    /// Reading the package from disk failed.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A blocking task the work was handed to did not finish (it panicked or
    /// was cancelled). A fault of the process, not of the package.
    #[error("{0}")]
    Task(String),
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
