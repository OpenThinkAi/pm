//! The little markdown pm reads structurally: ATX headings outside fenced
//! code blocks. Shared by section-level document writes (AGT-1573,
//! `pm/src/section.rs`) and `pm check`'s decision ids (AGT-1992,
//! [`crate::check::decision_ids`]), so both agree on what a heading is.

/// One heading line: its index, level and text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heading {
    pub line: usize,
    pub level: usize,
    pub text: String,
}

/// `(level, text)` for an ATX heading line, else `None`: up to three
/// spaces of indent, 1-6 `#`, then a space, a tab or the end of the line;
/// an optional closing run of `#` is not part of the text.
pub fn parse_heading(line: &str) -> Option<(usize, String)> {
    let line = line.trim_end_matches(['\n', '\r']);
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.len() - rest.trim_start_matches('#').len();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = &rest[level..];
    if !(after.is_empty() || after.starts_with([' ', '\t'])) {
        return None;
    }
    let mut text = after.trim();
    // A closing sequence: `## Goals ##` reads as "Goals"; `## C#` keeps
    // its `#` (the run must follow a space).
    let stripped = text.trim_end_matches('#');
    if stripped.is_empty() {
        text = "";
    } else if stripped.len() < text.len() && stripped.ends_with([' ', '\t']) {
        text = stripped.trim_end();
    }
    Some((level, text.to_string()))
}

/// The opening fence of a fenced code block: up to three spaces, then at
/// least three backticks or tildes. Returns the fence char and its length.
fn fence(line: &str) -> Option<(char, usize)> {
    let line = line.trim_end_matches(['\n', '\r']);
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let ch = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let n = rest.len() - rest.trim_start_matches(ch).len();
    (n >= 3).then_some((ch, n))
}

/// Every heading outside a fenced code block, in document order.
pub fn headings(lines: &[&str]) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut open: Option<(char, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some((ch, n)) = open {
            // A closing fence: the same char, at least as long, nothing after.
            if let Some((c, m)) = fence(line) {
                let rest = line.trim().trim_start_matches(c);
                if c == ch && m >= n && rest.is_empty() {
                    open = None;
                }
            }
            continue;
        }
        if let Some(f) = fence(line) {
            open = Some(f);
            continue;
        }
        if let Some((level, text)) = parse_heading(line) {
            out.push(Heading {
                line: i,
                level,
                text,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_parsing() {
        assert_eq!(parse_heading("## Goals ##\n"), Some((2, "Goals".into())));
        assert_eq!(parse_heading("## C#"), Some((2, "C#".into())));
        assert_eq!(parse_heading("#hashtag"), None);
        assert_eq!(parse_heading("    # code"), None);
        assert_eq!(parse_heading("####### seven"), None);
    }

    #[test]
    fn headings_skip_fenced_code() {
        let lines: Vec<&str> = "# A\n```\n# B\n```\n~~~~\n# C\n~~~\n~~~~\n## D\n"
            .lines()
            .collect();
        let found: Vec<(usize, usize, String)> = headings(&lines)
            .into_iter()
            .map(|h| (h.line, h.level, h.text))
            .collect();
        assert_eq!(found, [(0, 1, "A".into()), (8, 2, "D".into())]);
    }
}
