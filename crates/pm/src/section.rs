//! Section-level document writes (AGT-1573): `pm project edit … --section
//! "<heading>" --from-file` replaces (or, with `--append`, adds to) the
//! text under one markdown heading, leaving every other byte of the
//! document as it was.
//!
//! A section is an ATX heading (`#`..`######` then a space, outside a
//! fenced code block) and the lines after it up to the next heading of the
//! same or a higher level (fewer `#`), or the end of the document — so a
//! `## A` section owns its `### sub` headings. Its **text** is those lines
//! less the blank lines at either end, which is what `pm show --section`
//! prints for a ticket; those surrounding blank lines are the separators,
//! and a write keeps them.
//!
//! The heading is matched case-insensitively on its text. `"Goals"`
//! matches a heading of that text at any level; `"## Goals"` only a level-2
//! one. No match is "not found" (exit 3); more than one is a usage error
//! (exit 2) — the write never guesses.

use pm_core::markdown::{Heading, headings, parse_heading};

use crate::exit::{CliError, Result};

/// How the file's text lands in the section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// The file's text (blank lines at either end trimmed) becomes the
    /// section's text.
    Replace,
    /// The file's text (trailing blank lines trimmed; leading ones kept,
    /// so a leading blank line starts a new paragraph) goes after the
    /// section's last non-blank line.
    Append,
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// `text` without its leading (when `leading`) and trailing blank lines;
/// what is left ends with a newline, or is empty.
fn trim_blank_lines(text: &str, leading: bool) -> String {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut start = 0;
    if leading {
        while start < lines.len() && is_blank(lines[start]) {
            start += 1;
        }
    }
    let mut end = lines.len();
    while end > start && is_blank(lines[end - 1]) {
        end -= 1;
    }
    let mut out: String = lines[start..end].concat();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `doc` with the section under `heading` rewritten from `file` per
/// `mode`. Everything outside the section's text is byte-identical.
pub(crate) fn write(doc: &str, heading: &str, file: &str, mode: Mode) -> Result<String> {
    let lines: Vec<&str> = doc.split_inclusive('\n').collect();
    let all = headings(&lines);
    let want = heading.trim();
    let (want_level, want_text) = match parse_heading(want) {
        Some((level, text)) if want.starts_with('#') => (Some(level), text),
        _ => (None, want.to_string()),
    };
    if want_text.is_empty() {
        return Err(CliError::usage("--section needs a heading's text"));
    }
    let matches: Vec<&Heading> = all
        .iter()
        .filter(|h| {
            h.text.eq_ignore_ascii_case(&want_text) && want_level.is_none_or(|l| l == h.level)
        })
        .collect();
    let found = match matches.as_slice() {
        [one] => *one,
        [] => {
            let known: Vec<String> = all
                .iter()
                .map(|h| format!("{} {}", "#".repeat(h.level), h.text))
                .collect();
            let hint = if known.is_empty() {
                "; the document has no headings".to_string()
            } else {
                format!("; headings: {}", known.join(", "))
            };
            return Err(CliError::not_found(format!(
                "no '{}' section{}",
                crate::text::inline(want),
                crate::text::inline(&hint)
            )));
        }
        many => {
            let levels: Vec<String> = many
                .iter()
                .map(|h| format!("'{} {}'", "#".repeat(h.level), h.text))
                .collect();
            return Err(CliError::usage(format!(
                "--section '{}' matches {} headings ({}); name one by its level, e.g. \
                 --section '{} {}', or rename one",
                crate::text::inline(want),
                many.len(),
                crate::text::inline(&levels.join(", ")),
                "#".repeat(many[0].level),
                crate::text::inline(&many[0].text)
            )));
        }
    };

    // The section's lines: after its heading, up to the next heading of the
    // same or a higher level.
    let first = found.line + 1;
    let end = all
        .iter()
        .find(|h| h.line > found.line && h.level <= found.level)
        .map_or(lines.len(), |h| h.line);
    let body = &lines[first..end];
    let core_start = body.iter().position(|l| !is_blank(l));
    let offset = |line: usize| -> usize { lines[..line].iter().map(|l| l.len()).sum() };

    let Some(core_start) = core_start else {
        // An empty section: the text goes after the first separating blank
        // line (if any), and a blank line is kept before a following heading.
        let new = trim_blank_lines(file, mode == Mode::Replace);
        if new.is_empty() {
            return Ok(doc.to_string());
        }
        let at_line = if body.is_empty() { first } else { first + 1 };
        let at = offset(at_line);
        let (before, after) = doc.split_at(at);
        let mut out = String::with_capacity(doc.len() + new.len() + 2);
        out.push_str(before);
        if !before.is_empty() && !before.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&new);
        if after.lines().next().is_some_and(|l| !is_blank(l)) {
            out.push('\n');
        }
        out.push_str(after);
        return Ok(out);
    };
    let core_start = first + core_start;
    let core_end = first + body.iter().rposition(|l| !is_blank(l)).unwrap_or(0) + 1;
    let (start_at, end_at) = (offset(core_start), offset(core_end));
    let core = &doc[start_at..end_at];
    let terminated = core.ends_with('\n');

    let mut new = match mode {
        Mode::Replace => trim_blank_lines(file, true),
        Mode::Append => {
            let added = trim_blank_lines(file, false);
            if added.is_empty() {
                return Ok(doc.to_string());
            }
            let mut joined = core.to_string();
            if !terminated {
                joined.push('\n');
            }
            joined.push_str(&added);
            joined
        }
    };
    // The text keeps the original's last-line termination, so the bytes
    // after it (a separator, the next heading, or the end) are unchanged.
    if !terminated && new.ends_with('\n') {
        new.pop();
    }
    let mut out = String::with_capacity(doc.len() + new.len());
    out.push_str(&doc[..start_at]);
    out.push_str(&new);
    out.push_str(&doc[end_at..]);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str =
        "# pm\n\nIntro.\n\n## Goals\n\nShip it.\nFast.\n\n### Detail\n\nd\n\n## Risks\n\nNone.\n";

    fn replace(doc: &str, h: &str, file: &str) -> String {
        write(doc, h, file, Mode::Replace).unwrap()
    }

    #[test]
    fn replace_rewrites_only_the_section_text_and_keeps_separators() {
        let out = replace(DOC, "Risks", "\n\nMany.\n\n");
        assert_eq!(
            out,
            "# pm\n\nIntro.\n\n## Goals\n\nShip it.\nFast.\n\n### Detail\n\nd\n\n## Risks\n\nMany.\n"
        );
    }

    #[test]
    fn a_section_owns_its_deeper_headings() {
        let out = replace(DOC, "goals", "New goals.");
        assert_eq!(
            out,
            "# pm\n\nIntro.\n\n## Goals\n\nNew goals.\n\n## Risks\n\nNone.\n"
        );
    }

    #[test]
    fn append_goes_after_the_last_non_blank_line() {
        let out = write(DOC, "## Risks", "Some.\n\n", Mode::Append).unwrap();
        assert!(out.ends_with("## Risks\n\nNone.\nSome.\n"), "{out}");
        let out = write(DOC, "Detail", "\nmore\n", Mode::Append).unwrap();
        assert!(out.contains("### Detail\n\nd\n\nmore\n\n## Risks"), "{out}");
    }

    #[test]
    fn an_unterminated_last_section_stays_unterminated() {
        let out = replace("## A\n\nold", "A", "new\n");
        assert_eq!(out, "## A\n\nnew");
        let out = write("## A\n\nold", "A", "more\n", Mode::Append).unwrap();
        assert_eq!(out, "## A\n\nold\nmore");
    }

    #[test]
    fn an_empty_section_gets_its_text_after_the_separator() {
        assert_eq!(replace("## A\n\n## B\n", "A", "x"), "## A\n\nx\n\n## B\n");
        assert_eq!(replace("## A\n## B\n", "A", "x\n"), "## A\nx\n\n## B\n");
        assert_eq!(replace("## A", "A", "x\n"), "## A\nx\n");
        assert_eq!(replace("## A\n\n", "A", "x\n"), "## A\n\nx\n");
    }

    #[test]
    fn headings_in_fenced_code_do_not_count() {
        let doc = "## A\n\n```md\n## B\n```\n\n## C\n\nc\n";
        assert_eq!(replace(doc, "A", "a\n"), "## A\n\na\n\n## C\n\nc\n");
        assert_eq!(
            write(doc, "B", "x", Mode::Replace).unwrap_err().code,
            crate::exit::NOT_FOUND
        );
    }

    #[test]
    fn missing_and_ambiguous_headings_are_refused() {
        let err = write(DOC, "Nope", "x", Mode::Replace).unwrap_err();
        assert_eq!(err.code, crate::exit::NOT_FOUND);
        assert!(err.error.to_string().contains("## Goals"));
        let doc = "# Notes\n\na\n\n## Notes\n\nb\n";
        let err = write(doc, "Notes", "x", Mode::Replace).unwrap_err();
        assert_eq!(err.code, crate::exit::USAGE);
        assert_eq!(
            replace(doc, "## Notes", "c\n"),
            "# Notes\n\na\n\n## Notes\n\nc\n"
        );
        assert_eq!(
            write(doc, "  ", "x", Mode::Replace).unwrap_err().code,
            crate::exit::USAGE
        );
    }
}
