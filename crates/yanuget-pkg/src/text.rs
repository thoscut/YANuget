//! Characters that make untrusted text read differently from what it is.
//!
//! A package's report is shown to the person who approves it, and the
//! internalizer refuses such characters in what its capture records. Both
//! use [`is_hidden`], so what one refuses the other makes visible.

use std::borrow::Cow;

/// Whether `c` is invisible or reorders the text around it: the control
/// characters (C0, DEL, C1), the bidirectional marks, embeddings, overrides
/// and isolates, the zero-width and other invisible format characters, the
/// line and paragraph separators, the interlinear annotations, the
/// invisible fillers and the tag characters.
pub fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            // Bidirectional marks, embeddings, overrides and isolates.
            '\u{061C}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
                // Zero-width and invisible format characters.
                | '\u{00AD}'
                | '\u{034F}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200D}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{2060}'..='\u{2064}'
                | '\u{206A}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                // Tags.
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// `s` with every [`is_hidden`] character but tab, line feed and carriage
/// return written out as `[U+XXXX]`, so a reader sees it is there. Those
/// three are kept: they only space the text, and a script shown in a `<pre>`
/// needs them.
pub fn reveal(s: &str) -> Cow<'_, str> {
    let shown = |c: char| !is_hidden(c) || matches!(c, '\t' | '\n' | '\r');
    if s.chars().all(shown) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 16);
    for c in s.chars() {
        if shown(c) {
            out.push(c);
        } else {
            out.push_str(&format!("[U+{:04X}]", c as u32));
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_characters_are_written_out() {
        // The classic: an override that makes "exe.txt" read as "txt.exe".
        assert_eq!(reveal("setup\u{202E}txt.exe"), "setup[U+202E]txt.exe");
        assert_eq!(
            reveal("a\u{200B}b\u{FEFF}c\u{0}"),
            "a[U+200B]b[U+FEFF]c[U+0000]"
        );
        assert_eq!(reveal("\u{2066}x\u{2069}"), "[U+2066]x[U+2069]");
        assert_eq!(reveal("tag\u{E0041}"), "tag[U+E0041]");
        assert_eq!(reveal("bell\u{7}\u{9B}"), "bell[U+0007][U+009B]");
    }

    #[test]
    fn plain_text_is_left_alone() {
        for s in [
            "https://example.com/a b.exe",
            "Grüße, 日本語",
            "line\r\nnext\tcol",
            "",
        ] {
            assert!(matches!(reveal(s), Cow::Borrowed(_)), "{s:?}");
        }
        assert!(!is_hidden('é') && !is_hidden(' ') && !is_hidden('\u{2013}'));
    }
}
