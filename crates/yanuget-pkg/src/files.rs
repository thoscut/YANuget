//! Names of files attached to a package version: the one rule the server
//! enforces on an attach and the internalizer names its downloads by.
//!
//! The name ends up in a URL, in a `Content-Disposition` header, on disk on
//! the machine that downloads it, and in the inbox directory an uploader
//! writes to, so it is held to the narrowest set that works everywhere:
//! `A-Z a-z 0-9 . _ -`, starting with a letter or digit, not ending in a dot
//! or dash, not a Windows device name, and with an allowed extension.

/// Longest name an attached file may have.
pub const MAX_FILE_NAME_LENGTH: usize = 128;

/// Whether `segment` names a Windows device, with or without an extension
/// (`nul`, `COM1.pdb`, `lpt9.txt`).
pub fn is_windows_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or("").trim_end();
    let stem = stem.to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}

/// Whether `ext` (without the dot) is in `allowed`, ignoring ASCII case and a
/// leading dot on the entries.
pub fn extension_allowed(allowed: &[String], ext: &str) -> bool {
    allowed
        .iter()
        .any(|a| a.trim_start_matches('.').eq_ignore_ascii_case(ext))
}

/// Why `name` cannot name an attached file under the extensions `allowed`,
/// or `None` when it can.
pub fn file_name_problem(name: &str, allowed: &[String]) -> Option<String> {
    if name.is_empty() || name.len() > MAX_FILE_NAME_LENGTH {
        return Some(format!("use 1 to {MAX_FILE_NAME_LENGTH} characters"));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Some("use only letters, digits, '.', '_' and '-'".into());
    }
    if !name.as_bytes()[0].is_ascii_alphanumeric() || name.ends_with(['.', '-']) {
        return Some("start with a letter or digit, and do not end in '.' or '-'".into());
    }
    if is_windows_device_name(name) {
        return Some("that is a Windows device name".into());
    }
    if name.eq_ignore_ascii_case("index.json") {
        return Some("that name lists a version's files".into());
    }
    match name.rsplit_once('.') {
        Some((_, ext)) if extension_allowed(allowed, ext) => None,
        _ => Some(format!(
            "the extension must be one of {}",
            allowed.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_names_are_recognised() {
        for device in ["nul", "CON.txt", "com1.pdb", "LPT9", "aux .x"] {
            assert!(is_windows_device_name(device), "{device}");
        }
        for name in ["null", "com0", "com10", "lpt", "console.exe"] {
            assert!(!is_windows_device_name(name), "{name}");
        }
    }

    #[test]
    fn names_follow_the_rule() {
        let allowed = vec![".EXE".to_string(), "msi".to_string()];
        assert_eq!(file_name_problem("Git-2.51.0-64-bit.exe", &allowed), None);
        assert_eq!(file_name_problem("setup.MSI", &allowed), None);
        for bad in [
            "",
            "Git 2.51.exe",
            "a+b.exe",
            "-x.exe",
            "x.exe.",
            "nul.exe",
            "index.json",
            "setup.zip",
            "setup",
        ] {
            assert!(file_name_problem(bad, &allowed).is_some(), "{bad:?}");
        }
        let long = format!("{}.exe", "a".repeat(MAX_FILE_NAME_LENGTH));
        assert!(file_name_problem(&long, &allowed).is_some());
    }
}
