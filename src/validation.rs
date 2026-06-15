//! Validation of NuGet identifiers.

use crate::error::{Error, Result};

/// Maximum length of a package id, per the NuGet client.
pub const MAX_ID_LENGTH: usize = 100;

/// Validate a package id against NuGet's rule `^\w+([.\-_]\w+)*$` with a length
/// cap. In short: word runs (`A-Za-z0-9_`) separated by single `.` or `-`,
/// never starting/ending with a separator and never with two in a row.
pub fn validate_package_id(id: &str) -> Result<()> {
    if id.is_empty() {
        return Err(Error::InvalidPackage("package id is empty".into()));
    }
    if id.len() > MAX_ID_LENGTH {
        return Err(Error::InvalidPackage(format!(
            "package id exceeds {MAX_ID_LENGTH} characters"
        )));
    }

    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let is_sep = |b: u8| b == b'.' || b == b'-';

    let bytes = id.as_bytes();
    let mut prev_sep = false;
    for (i, &b) in bytes.iter().enumerate() {
        if is_word(b) {
            prev_sep = false;
        } else if is_sep(b) {
            // Separators may not start the id nor immediately follow another.
            if i == 0 || prev_sep {
                return Err(invalid(id));
            }
            prev_sep = true;
        } else {
            return Err(invalid(id));
        }
    }
    // Must not end on a separator.
    if prev_sep {
        return Err(invalid(id));
    }
    Ok(())
}

fn invalid(id: &str) -> Error {
    Error::InvalidPackage(format!("invalid package id: {id:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_ids() {
        for id in [
            "Newtonsoft.Json",
            "Microsoft.Extensions.Logging",
            "My-Package",
            "a",
            "A1",
            "with_underscore",
            "Mix.Of-All_3",
        ] {
            assert!(validate_package_id(id).is_ok(), "{id} should be valid");
        }
    }

    #[test]
    fn rejects_invalid_ids() {
        for id in [
            "",
            ".leading",
            "trailing.",
            "double..dot",
            "has space",
            "bad!char",
            "-dash",
            "a--b",
            "slash/in/id",
        ] {
            assert!(validate_package_id(id).is_err(), "{id} should be invalid");
        }
    }

    #[test]
    fn rejects_overlong_id() {
        let long = "a".repeat(MAX_ID_LENGTH + 1);
        assert!(validate_package_id(&long).is_err());
    }
}
