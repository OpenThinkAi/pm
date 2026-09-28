//! The markdown half of a vault ticket (AGT-1347 AC2, AC3): splitting the
//! body into the description and its `## Comments` entries, and migrating
//! the prose markers (`waived:`, `⚠ NEEDS-HUMAN:` / `waiting-human:`,
//! `parked:`) into structured values, removing the migrated text.
//!
//! Everything here is pure string work over one file's text; the caller
//! (`super::vault`) supplies the text and records what was migrated.

use pm_core::Waiver;
use pm_core::markers::{PARKED_FOREVER, normalize_rule, parse_date};

/// One `### YYYY-MM-DD — Author` entry of a `## Comments` section, as it
/// appears in the file (before marker migration).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentEntry {
    pub date: String,
    pub author: String,
    pub body: String,
}

/// A body split into what stays in the description and the comment
/// entries that become `comment.add` ops.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SplitBody {
    /// Every section but `## Comments`, in file order — the standard
    /// template sections and any non-standard one alike (AC2: "every
    /// non-standard body section is kept in the description"). A
    /// `## Comments` section's text that precedes its first entry (an
    /// HTML comment from the template, a stray paragraph) is kept too,
    /// under its heading, so nothing is dropped.
    pub description: String,
    pub comments: Vec<CommentEntry>,
}

/// Splits `body` (the text after the frontmatter) at its `## Comments`
/// section(s). A `###` heading inside Comments that is not a dated entry
/// (`### Shipped 2026-08-09`) is kept as text of the entry before it —
/// or of the preamble, if it comes first.
pub fn split_body(body: &str) -> SplitBody {
    let mut description = String::new();
    let mut comments = Vec::new();
    let mut in_comments = false;
    // Inside a Comments section: the preamble (text before the first
    // entry) and the entry being collected.
    let mut preamble = String::new();
    let mut current: Option<CommentEntry> = None;

    let flush_comments = |preamble: &mut String,
                          current: &mut Option<CommentEntry>,
                          description: &mut String,
                          comments: &mut Vec<CommentEntry>| {
        if let Some(entry) = current.take() {
            comments.push(entry);
        }
        if !preamble.trim().is_empty() {
            description.push_str("## Comments\n\n");
            description.push_str(preamble.trim());
            description.push_str("\n\n");
        }
        preamble.clear();
    };

    for line in body.lines() {
        if let Some(heading) = line.strip_prefix("## ") {
            if in_comments {
                flush_comments(&mut preamble, &mut current, &mut description, &mut comments);
            }
            in_comments = heading.trim() == "Comments";
            if !in_comments {
                description.push_str(line);
                description.push('\n');
            }
            continue;
        }
        if !in_comments {
            description.push_str(line);
            description.push('\n');
            continue;
        }
        if let Some((date, author)) = comment_header(line) {
            if let Some(entry) = current.take() {
                comments.push(entry);
            }
            current = Some(CommentEntry {
                date: date.to_string(),
                author: author.to_string(),
                body: String::new(),
            });
            continue;
        }
        match &mut current {
            Some(entry) => {
                entry.body.push_str(line);
                entry.body.push('\n');
            }
            None => {
                preamble.push_str(line);
                preamble.push('\n');
            }
        }
    }
    if in_comments {
        flush_comments(&mut preamble, &mut current, &mut description, &mut comments);
    }
    for c in &mut comments {
        c.body = tidy(&c.body);
    }
    SplitBody {
        description: tidy(&description),
        comments,
    }
}

/// `### 2026-08-01 — Author` → `("2026-08-01", "Author")`. The vault's
/// entries all use the em dash (projects/pm/research/vault-anatomy.md).
fn comment_header(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix("### ")?;
    let (date, author) = rest.split_once(" — ")?;
    let date = date.trim();
    let author = author.trim();
    if parse_date(date).is_err() || author.is_empty() {
        return None;
    }
    Some((date, author))
}

/// Trims, and collapses runs of blank lines (left behind by removed
/// marker lines) to one — so two imports of the same text agree.
pub fn tidy(text: &str) -> String {
    let mut out = String::new();
    let mut blank_run = 0;
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_run > 0 {
                out.push('\n');
            }
        }
        blank_run = 0;
        out.push_str(line);
    }
    out
}

// ---------------------------------------------------------------- markers

/// Structured values migrated out of one ticket's prose.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Markers {
    pub waivers: Vec<Waiver>,
    /// Every `NEEDS-HUMAN` / `waiting-human` reason, in file order; the
    /// ticket's one `hold` joins them.
    pub holds: Vec<String>,
    /// Every `parked:` value, in file order (the last one wins).
    pub parked: Vec<String>,
}

/// One migrated marker, for the import report ("migrated lines are …
/// logged").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Migrated {
    pub kind: MarkerKind,
    /// The marker text as it stood in the file, `<token> <value>`.
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkerKind {
    Waiver,
    Hold,
    Parked,
}

impl MarkerKind {
    pub fn name(self) -> &'static str {
        match self {
            MarkerKind::Waiver => "waiver",
            MarkerKind::Hold => "hold",
            MarkerKind::Parked => "parked",
        }
    }
}

/// The marker tokens, with the kind each migrates to. `NEEDS-CLARIFICATION`
/// is the vault's other "waiting on a human" spelling (vault-anatomy.md),
/// so it holds the ticket too.
const TOKENS: [(&str, MarkerKind); 5] = [
    ("waived:", MarkerKind::Waiver),
    ("NEEDS-HUMAN:", MarkerKind::Hold),
    ("waiting-human:", MarkerKind::Hold),
    ("NEEDS-CLARIFICATION:", MarkerKind::Hold),
    ("parked:", MarkerKind::Parked),
];

/// Where a marker sits in a line: the byte range to remove (the token,
/// its `⚠ ` prefix if any, and the value to end of line) and the value.
struct Found {
    kind: MarkerKind,
    token: &'static str,
    start: usize,
    value: String,
    /// The value opened with a backtick that this line does not close, so
    /// the marker continues on following lines.
    open_backtick: bool,
}

/// The first marker in `line`, if any. A token counts only when it starts
/// a word (start of line, or after whitespace, a backtick, `*`/`_`
/// emphasis or an opening parenthesis) and is followed by whitespace and
/// a value that is not itself a backtick — so `` `waived:` lines `` and
/// `NEEDS-HUMAN, waivers` in ordinary prose are left alone.
fn find_marker(line: &str) -> Option<Found> {
    let mut best: Option<Found> = None;
    for (token, kind) in TOKENS {
        for (idx, _) in line.match_indices(token) {
            let before = line[..idx].chars().next_back();
            let word_start = match before {
                None => true,
                Some(c) => c.is_whitespace() || matches!(c, '`' | '*' | '_' | '('),
            };
            if !word_start {
                continue;
            }
            let after = &line[idx + token.len()..];
            let value = after.trim_start();
            if value.is_empty() || value.len() == after.len() || value.starts_with('`') {
                continue;
            }
            // Fold the warning sign into the removed range.
            let mut start = idx;
            let prefix = line[..idx].trim_end();
            if let Some(stripped) = prefix.strip_suffix('⚠') {
                start = stripped.len();
            }
            // Inside an opening backtick: `waived: … (closed on this line or later).
            let opened =
                line[..start].trim_end().ends_with('`') || line[..idx].trim_end().ends_with('`');
            let closes_here = value.contains('`');
            let open_backtick = opened && !closes_here;
            let value = value.trim_end().trim_end_matches('`').trim_end();
            let candidate = Found {
                kind,
                token,
                start,
                value: value.to_string(),
                open_backtick,
            };
            if best.as_ref().is_none_or(|b| candidate.start < b.start) {
                best = Some(candidate);
            }
            break;
        }
    }
    best
}

/// Whether `line` continues a marker's prose from the previous line: not
/// blank, not a heading, list item, table row or quote, and not a marker
/// of its own.
fn continues(line: &str) -> bool {
    let t = line.trim_start();
    if t.is_empty() || find_marker(line).is_some() {
        return false;
    }
    let list_item = t.split_once('.').is_some_and(|(n, rest)| {
        !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) && rest.starts_with(' ')
    });
    !(t.starts_with('#')
        || t.starts_with("- ")
        || t.starts_with("* ")
        || t.starts_with('|')
        || t.starts_with('>')
        || list_item)
}

/// After the marker fragment is cut from a line, what is left of it —
/// or `None` when only list/emphasis/backtick scaffolding remains and the
/// whole line should go.
fn remainder(prefix: &str) -> Option<String> {
    let mut rest = prefix.trim_end();
    // Unbalanced opening backtick left behind by the cut.
    if rest.matches('`').count() % 2 == 1
        && let Some(s) = rest.strip_suffix('`')
    {
        rest = s.trim_end();
    }
    let core = rest
        .trim_start()
        .trim_start_matches(['-', '*', '#', '>'])
        .trim_start();
    let core = match core.split_once('.') {
        Some((n, r)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => r.trim_start(),
        _ => core,
    };
    let core = core.trim_matches(|c: char| matches!(c, '*' | '_' | '`') || c.is_whitespace());
    if core.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

/// Migrates every marker in `text` into `markers`, logging each in
/// `log`, and returns the text with the migrated lines (or, for a marker
/// mid-line, the fragment from the marker to the end of the line)
/// removed. A marker's value continues onto following lines while they
/// read as continuation prose (or, inside backticks, until the closing
/// one).
pub fn migrate(text: &str, markers: &mut Markers, log: &mut Vec<Migrated>) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let Some(found) = find_marker(line) else {
            out.push(line.to_string());
            i += 1;
            continue;
        };
        let mut value = found.value.clone();
        let mut j = i + 1;
        if found.open_backtick {
            while j < lines.len() {
                let next = lines[j].trim();
                j += 1;
                if let Some(closed) = next.split_once('`') {
                    push_word(&mut value, closed.0.trim_end());
                    break;
                }
                push_word(&mut value, next);
            }
        } else {
            while j < lines.len() && continues(lines[j]) {
                push_word(&mut value, lines[j].trim());
                j += 1;
            }
        }
        let value = value.trim().to_string();
        log.push(Migrated {
            kind: found.kind,
            text: format!("{} {value}", found.token),
        });
        match found.kind {
            MarkerKind::Waiver => markers.waivers.push(parse_waiver(&value)),
            MarkerKind::Hold => markers.holds.push(value),
            MarkerKind::Parked => markers.parked.push(value),
        }
        if let Some(rest) = remainder(&line[..found.start]) {
            out.push(rest);
        }
        i = j;
    }
    tidy(&out.join("\n"))
}

fn push_word(value: &mut String, more: &str) {
    if more.is_empty() {
        return;
    }
    if !value.is_empty() {
        value.push(' ');
    }
    value.push_str(more);
}

/// `standalone — why` → rule `standalone`, reason `why` (the vault's
/// spelling, practices/ticket-hygiene.md); `T3 — why` → rule `T3`; a
/// value without a dash is both rule and reason, except that a leading
/// `standalone` is still the rule.
pub fn parse_waiver(value: &str) -> Waiver {
    let (rule, reason) = match value.split_once(" — ") {
        Some((rule, reason)) => (rule.trim(), reason.trim()),
        None => match value.strip_prefix("standalone") {
            Some(rest) => (
                "standalone",
                rest.trim_start_matches(|c: char| {
                    matches!(c, ':' | ',' | '-') || c.is_whitespace()
                }),
            ),
            None => (value, value),
        },
    };
    let reason = if reason.is_empty() { rule } else { reason };
    Waiver {
        rule: normalize_rule(rule).unwrap_or_else(|_| "standalone".to_string()),
        reason: reason.to_string(),
    }
}

/// A `parked:` value as `Parked.until`: a `YYYY-MM-DD` date or `forever`
/// verbatim; anything else (prose like `until AGT-1236 is done`) is
/// parked `forever`, with the prose kept by the caller under
/// `ext.parked_reason`.
pub fn parked_until(value: &str) -> (String, Option<String>) {
    let v = value.trim().trim_end_matches('.');
    if v == PARKED_FOREVER {
        return (PARKED_FOREVER.to_string(), None);
    }
    match parse_date(v) {
        Ok(date) => (date, None),
        Err(_) => (PARKED_FOREVER.to_string(), Some(value.trim().to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_splits_comments_from_the_other_sections() {
        let body = "## Problem Statement\n\nP.\n\n## Spike\n\nlegacy\n\n## Comments\n\n<!-- keep -->\n\n### 2026-08-01 — Filed\n\nfirst\n\n### Shipped 2026-08-09\n\nextra\n\n### 2026-08-02 — QA agent\nsecond\n\n## Outcome\n\ndone\n";
        let split = split_body(body);
        assert_eq!(
            split.description,
            "## Problem Statement\n\nP.\n\n## Spike\n\nlegacy\n\n## Comments\n\n<!-- keep -->\n\n## Outcome\n\ndone"
        );
        assert_eq!(
            split.comments,
            [
                CommentEntry {
                    date: "2026-08-01".into(),
                    author: "Filed".into(),
                    body: "first\n\n### Shipped 2026-08-09\n\nextra".into(),
                },
                CommentEntry {
                    date: "2026-08-02".into(),
                    author: "QA agent".into(),
                    body: "second".into(),
                },
            ]
        );
        assert_eq!(comment_header("### 2026-13-01 — x"), None);
        assert_eq!(comment_header("### 2026-08-01 — "), None);
    }

    fn migrated(text: &str) -> (String, Markers, Vec<Migrated>) {
        let mut m = Markers::default();
        let mut log = Vec::new();
        let out = migrate(text, &mut m, &mut log);
        (out, m, log)
    }

    #[test]
    fn waived_lines_migrate_in_every_idiom() {
        // Bare, inside backticks over three lines (AGT-834), in a bullet
        // with backticks (AGT-1282), after a label, and continuing onto
        // the next line without backticks (AGT-1018).
        let text = "waived: standalone — single-repo fix from GH issue #96\n\n`waived: standalone — no live project; archived\nand no replacement exists. Self-contained\none ticket.`\n\nkept\n\n- `waived: T3 — no single repo; touches `~/.config/pablo`.`\n- Vault-only ticket: `waived: standalone repo — vault bin/ script; no code repo.`\n\nwaived: standalone — repo hygiene, not part of the\nunified-renderer collapse.\n\nafter\n";
        let (out, m, log) = migrated(text);
        let rules: Vec<(&str, &str)> = m
            .waivers
            .iter()
            .map(|w| (w.rule.as_str(), w.reason.as_str()))
            .collect();
        assert_eq!(
            rules,
            [
                ("standalone", "single-repo fix from GH issue #96"),
                (
                    "standalone",
                    "no live project; archived and no replacement exists. Self-contained one ticket."
                ),
                ("T3", "no single repo; touches `~/.config/pablo`."),
                ("standalone repo", "vault bin/ script; no code repo."),
                (
                    "standalone",
                    "repo hygiene, not part of the unified-renderer collapse."
                ),
            ]
        );
        assert_eq!(out, "kept\n\n- Vault-only ticket:\n\nafter");
        assert_eq!(log.len(), 5);
        assert!(log[0].text.starts_with("waived: standalone — single-repo"));
        assert!(m.holds.is_empty() && m.parked.is_empty());
    }

    #[test]
    fn prose_mentions_of_markers_are_left_alone() {
        let text = "3. Marker migration: `waived:` lines (including inside backticks) → waiver; `⚠ NEEDS-HUMAN`/`waiting-human:` → hold.\nNEEDS-HUMAN, waivers, parked and date gates are free-text grep today.\nor explicitly kept with a `waived:` note in their comments\n";
        let (out, m, log) = migrated(text);
        assert_eq!(out, tidy(text));
        assert_eq!(m, Markers::default());
        assert!(log.is_empty());
    }

    #[test]
    fn holds_and_parked_migrate_including_mid_line() {
        let text = "5. ⚠ NEEDS-HUMAN: Matt reviews the PR text before it is opened.\n6. Something else.\n\n⚠ NEEDS-HUMAN: AC5 — install the plist, then after the\nfirst Sunday run record it here\nand move the ticket to done.\n\nwaiting-human: sign-off\n\nDeliberately blocked: Matt does not want bugs. parked: until AGT-1236 is done.\n`parked: do not start this until the backlog moves.`\nparked: 2026-11-01\n### ⚠ NEEDS-HUMAN: gate CLOSED on findings\n";
        let (out, m, _) = migrated(text);
        assert_eq!(
            m.holds,
            [
                "Matt reviews the PR text before it is opened.",
                "AC5 — install the plist, then after the first Sunday run record it here and move the ticket to done.",
                "sign-off",
                "gate CLOSED on findings",
            ]
        );
        assert_eq!(
            m.parked,
            [
                "until AGT-1236 is done.",
                "do not start this until the backlog moves.",
                "2026-11-01"
            ]
        );
        assert_eq!(
            out,
            "6. Something else.\n\nDeliberately blocked: Matt does not want bugs."
        );
        assert_eq!(parked_until("2026-11-01"), ("2026-11-01".into(), None));
        assert_eq!(parked_until("forever"), ("forever".into(), None));
        assert_eq!(
            parked_until("until AGT-1236 is done."),
            ("forever".into(), Some("until AGT-1236 is done.".into()))
        );
    }

    #[test]
    fn waiver_values_split_into_rule_and_reason() {
        let w = parse_waiver("r1 — no project");
        assert_eq!((w.rule.as_str(), w.reason.as_str()), ("R1", "no project"));
        let w = parse_waiver("standalone");
        assert_eq!(
            (w.rule.as_str(), w.reason.as_str()),
            ("standalone", "standalone")
        );
        let w = parse_waiver("standalone: one-off");
        assert_eq!(
            (w.rule.as_str(), w.reason.as_str()),
            ("standalone", "one-off")
        );
        let w = parse_waiver("operator decision");
        assert_eq!(w.rule, "operator decision");
        assert_eq!(w.reason, "operator decision");
    }

    #[test]
    fn tidy_is_idempotent_and_collapses_blank_runs() {
        let t = tidy("\n\na  \n\n\n\nb\n  \nc\n\n");
        assert_eq!(t, "a\n\nb\n\nc");
        assert_eq!(tidy(&t), t);
    }
}
