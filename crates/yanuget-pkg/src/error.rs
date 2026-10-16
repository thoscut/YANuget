//! The crate's error type.

/// What can go wrong reading a package.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The package, or its manifest, is malformed or refused.
    #[error("invalid package: {0}")]
    InvalidPackage(String),

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
