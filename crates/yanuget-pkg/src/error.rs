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

    /// Reading the package from disk failed. The message is the I/O error's
    /// own: a text here that repeated it, with the error as the source as
    /// well, printed it twice wherever the chain is shown.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A blocking task the work was handed to did not finish (it panicked or
    /// was cancelled). A fault of the process, not of the package.
    #[error("{0}")]
    Task(String),
}

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// An error with its causes, `: `-separated, for a message that has room
/// for one line only (a log line, a check report, a sync's reason). A cause
/// already in the text is not repeated, since some errors, reqwest's among
/// them, name their cause in their own message as well.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, thiserror::Error)]
    enum Outer {
        #[error("cannot save the state")]
        Save(#[source] Error),
        #[error("cannot save the state: {0}")]
        Repeated(#[source] Error),
    }

    fn denied() -> Error {
        std::io::Error::new(std::io::ErrorKind::PermissionDenied, "permission denied").into()
    }

    /// What `{:#}` of an `anyhow::Error` prints: every cause, repeated or
    /// not.
    fn every_cause(e: &dyn std::error::Error) -> String {
        let mut printed = e.to_string();
        let mut source = e.source();
        while let Some(cause) = source {
            printed.push_str(&format!(": {cause}"));
            source = cause.source();
        }
        printed
    }

    #[test]
    fn a_chain_names_each_cause_once() {
        assert_eq!(
            error_chain(&Outer::Save(denied())),
            "cannot save the state: permission denied"
        );
        // An error that spells its cause out itself is not followed by it.
        assert_eq!(
            error_chain(&Outer::Repeated(denied())),
            "cannot save the state: permission denied"
        );
        assert_eq!(
            error_chain(&Error::InvalidPackage("no .nuspec".into())),
            "invalid package: no .nuspec"
        );
    }

    /// `MNT-30`: an I/O error says what failed once, however the chain is
    /// printed.
    #[test]
    fn an_io_error_is_not_printed_twice() {
        let e = denied();
        assert_eq!(e.to_string(), "permission denied");
        assert_eq!(every_cause(&e), "permission denied");
        assert_eq!(
            every_cause(&Outer::Save(denied())),
            "cannot save the state: permission denied"
        );
    }
}
