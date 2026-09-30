//! Terminal-safe text (AGT-1465). Ticket titles, descriptions, comments,
//! hold reasons and the hub's error bodies arrive from other actors
//! through `pm sync`, so anything printed for a human could carry ANSI/OSC
//! escape sequences (rewrite the screen, set the window title, write the
//! clipboard), C1 controls, or bidi overrides that reorder what the
//! reader sees. AGT-1468 extended that to every synced field: actors,
//! assignees, labels, repos, project metadata, states, model labels and
//! the hub's claim refusals (`tests/output_fields.rs` pins each). Every human-readable (non-`--json`) sink routes such text
//! through [`printable`] or [`inline`]; `--json` stays byte-for-byte
//! as stored (JSON escapes the C0 range itself, and a consumer is not a
//! terminal).

use std::borrow::Cow;

/// True for characters that steer a terminal or reorder displayed text:
/// C0/C1 controls (ESC, CSI, OSC introducers, CR, BEL, DEL...), the bidi
/// embedding/override/isolate marks, and the line/paragraph separators.
/// `\n` and `\t` are the callers' business (see [`printable`]).
fn is_unsafe(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{061C}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2066}'..='\u{2069}'
        )
}

/// `s` with every terminal-steering character dropped, keeping `\n` and
/// `\t` so multi-line output (descriptions, comments) keeps its shape. An
/// ESC is dropped but the printable tail of its sequence stays (harmless
/// text such as `[31m`); nothing can execute once the introducer is gone.
pub(crate) fn printable(s: &str) -> Cow<'_, str> {
    if s.chars().all(|c| !is_unsafe(c) || c == '\n' || c == '\t') {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.chars()
            .filter(|&c| !is_unsafe(c) || c == '\n' || c == '\t')
            .collect(),
    )
}

/// [`printable`] for a single-line sink (a list row, a message): newlines
/// and tabs become spaces too, so one ticket cannot fake extra rows.
pub(crate) fn inline(s: &str) -> String {
    s.chars()
        .filter_map(|c| match c {
            '\n' | '\t' => Some(' '),
            c if is_unsafe(c) => None,
            c => Some(c),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ansi_osc_c1_and_bidi() {
        assert_eq!(printable("a\x1b[2Jb"), "a[2Jb");
        assert_eq!(printable("x\x1b]0;pwned\x07y"), "x]0;pwnedy");
        assert_eq!(printable("c1\u{9b}31m\u{85}\u{9d}"), "c131m");
        assert_eq!(printable("cr\rover\x7f"), "crover");
        assert_eq!(printable("a\u{202E}gnp.exe\u{2066}b\u{200F}"), "agnp.exeb");
    }

    #[test]
    fn printable_keeps_newlines_and_tabs_and_borrows_clean_text() {
        assert!(matches!(printable("ok\n\tfine – ünï"), Cow::Borrowed(_)));
        assert_eq!(printable("a\r\nb\tc\x1b"), "a\nb\tc");
    }

    #[test]
    fn inline_flattens_whitespace_controls() {
        assert_eq!(inline("one\ntwo\tthree\x1b[0m"), "one two three[0m");
    }
}
