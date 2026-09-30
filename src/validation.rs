//! Validation of NuGet identifiers.

use crate::error::{Error, Result};

/// Maximum length of a package id, per the NuGet client.
pub const MAX_ID_LENGTH: usize = 100;

/// Validate a package id against NuGet's rule `^\w+([.\-_]\w+)*$` with a length
/// cap. In short: word runs (`A-Za-z0-9_`) separated by single `.` or `-`,
/// never starting/ending with a separator and never with two in a row.
///
/// On top of NuGet's rule, an id whose first dotted part is a Windows device
/// name (`Aux.Core`, `Con.Utils`, `COM1.Sdk`) is refused: the store keeps each
/// id in a directory of that name, which Windows opens as the device, so the
/// store refuses it on every system. Saying so here gives the pusher a clear
/// reason up front instead of a late error from the store.
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
    if crate::storage::filesystem::is_windows_device_name(id) {
        let device = id.split('.').next().unwrap_or(id);
        return Err(Error::InvalidPackage(format!(
            "invalid package id: {id:?} starts with {device:?}, a Windows device name, \
             which cannot be stored"
        )));
    }
    Ok(())
}

fn invalid(id: &str) -> Error {
    Error::InvalidPackage(format!("invalid package id: {id:?}"))
}

/// Longest name an attached file may have.
pub const MAX_FILE_NAME_LENGTH: usize = 128;

/// Validate the name of a file attached to a package version.
///
/// The name ends up in a URL, in a `Content-Disposition` header, on disk on
/// the machine that downloads it, and in the inbox directory an uploader
/// writes to — so it is held to the narrowest set that works everywhere:
/// `A-Z a-z 0-9 . _ -`, starting with a letter or digit, not ending in a dot or
/// dash, not a Windows device name, and with one of `allowed` as its
/// extension. (The stored bytes are addressed by their hash, never by this
/// name, so it cannot steer where anything is written on the server.)
pub fn validate_file_name(name: &str, allowed: &crate::config::FilesConfig) -> Result<()> {
    let bad = |why: &str| {
        Err(Error::BadRequest(format!(
            "invalid file name {name:?}: {why}"
        )))
    };
    if name.is_empty() || name.len() > MAX_FILE_NAME_LENGTH {
        return bad(&format!("use 1 to {MAX_FILE_NAME_LENGTH} characters"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return bad("use only letters, digits, '.', '_' and '-'");
    }
    if !name.as_bytes()[0].is_ascii_alphanumeric() || name.ends_with(['.', '-']) {
        return bad("start with a letter or digit, and do not end in '.' or '-'");
    }
    if crate::storage::filesystem::is_windows_device_name(name) {
        return bad("that is a Windows device name");
    }
    if name.eq_ignore_ascii_case("index.json") {
        return bad("that name lists a version's files");
    }
    match name.rsplit_once('.') {
        Some((_, ext)) if allowed.allows_extension(ext) => Ok(()),
        _ => bad(&format!(
            "the extension must be one of {}",
            allowed.allowed_extensions.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_are_held_to_what_works_everywhere() {
        let files = crate::config::FilesConfig::default();
        for good in ["base.wim", "Base-Image_2.WIM", "disk.part1.swm", "tools.7z"] {
            assert!(validate_file_name(good, &files).is_ok(), "{good:?} refused");
        }
        for bad in [
            "",
            "../x.wim",
            "a/b.wim",
            "a\\b.wim",
            "c:x.wim",
            "x.wim:stream",
            ".hidden.wim",
            "-flag.wim",
            "trailing.wim.",
            "nul.wim",
            "com1.wim",
            "space name.wim",
            "caf\u{e9}.wim",
            "index.json",
            "page.html",
            "image.svg",
            "noextension",
        ] {
            assert!(validate_file_name(bad, &files).is_err(), "{bad:?} accepted");
        }
        let long = format!("{}.wim", "a".repeat(MAX_FILE_NAME_LENGTH));
        assert!(validate_file_name(&long, &files).is_err());
        // The extension list is the operator's.
        let only_iso = crate::config::FilesConfig {
            allowed_extensions: vec![".ISO".into()],
            ..Default::default()
        };
        assert!(validate_file_name("dvd.iso", &only_iso).is_ok());
        assert!(validate_file_name("base.wim", &only_iso).is_err());
    }

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
    fn rejects_ids_that_are_windows_device_names() {
        for id in ["Aux.Core", "Con.Utils", "COM1.Sdk", "nul", "LPT9", "prn.x"] {
            let err = validate_package_id(id).unwrap_err().to_string();
            assert!(err.contains("Windows device name"), "{id}: {err}");
        }
        // Only the whole first part counts, as in the store.
        for id in [
            "Console.Utils",
            "Contoso.Aux",
            "Com10.Sdk",
            "Nullable",
            "Con-Utils",
        ] {
            assert!(validate_package_id(id).is_ok(), "{id} should be valid");
        }
    }

    #[test]
    fn rejects_overlong_id() {
        let long = "a".repeat(MAX_ID_LENGTH + 1);
        assert!(validate_package_id(&long).is_err());
    }
}
