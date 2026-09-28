//! `pm --help` / `pm <verb> --help` vs. `docs/cli-contract.md` (AGT-1352
//! AC3): every `--flag` clap prints for a command must appear somewhere in
//! the doc, and — so the doc never documents a flag that does not exist —
//! every `--flag`-shaped token the doc mentions must be one clap actually
//! prints for *some* command. No regex dependency: flags are `--` tokens,
//! found the same simple way on both sides.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Output};

/// Every command and subcommand path `main.rs`'s `Cmd`/`ImportCmd`/
/// `ProjectCmd`/`BackupCmd`/`TicketCmd` define. Kept as a flat list (rather than
/// re-deriving it from clap) so this test fails loudly — a path missing
/// here, or a path here clap no longer has — the moment either one drifts,
/// rather than silently checking less than it claims to.
const PATHS: &[&[&str]] = &[
    &[],
    &["init"],
    &["new"],
    &["show"],
    &["set"],
    &["label"],
    &["comment"],
    &["move"],
    &["done"],
    &["unclaim"],
    &["edit"],
    &["claim"],
    &["list"],
    &["log"],
    &["status"],
    &["graph"],
    &["ready"],
    &["hold"],
    &["holds"],
    &["waive"],
    &["check"],
    &["doctor"],
    &["archive"],
    &["unarchive"],
    &["import"],
    &["import", "vault"],
    &["project"],
    &["project", "new"],
    &["project", "show"],
    &["project", "list"],
    &["project", "edit"],
    &["project", "delete"],
    &["project", "doc"],
    &["project", "doc", "add"],
    &["ticket"],
    &["ticket", "list"],
    &["ticket", "show"],
    &["backup"],
    &["backup", "install-timer"],
    &["backup", "status"],
];

fn help_text(path: &[&str]) -> String {
    let mut args: Vec<&str> = path.to_vec();
    args.push("--help");
    let out: Output = Command::new(env!("CARGO_BIN_EXE_pm"))
        .args(&args)
        .env_clear()
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`pm {} --help` exited {:?}\nstderr: {}",
        path.join(" "),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Every `--flag`-shaped token in `text`: `--`, then a letter (every real
/// clap flag starts with one — this is what tells a flag apart from a
/// markdown table rule like `|---|---|`), then more letters/digits/`-`.
/// Works identically on `--help` output and on the markdown doc, so a flag
/// found one way is the same string found the other.
fn flag_tokens(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'-'
            && bytes[i + 1] == b'-'
            && bytes.get(i + 2).is_some_and(u8::is_ascii_alphabetic)
        {
            let start = i;
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'-') {
                j += 1;
            }
            out.insert(text[start..j].to_string());
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

/// Strips fenced (triple-backtick) code blocks: this doc's JSON-shape and
/// shell-command examples legitimately contain `--`-prefixed tokens that
/// are not `pm` flags (e.g. `cargo test --test json_contract`), and every
/// real `pm` flag is also named in this doc's plain prose (its "Flags:"
/// line), so excluding fenced blocks loses no real flag mention while
/// dropping that noise from the reverse (doc -> real flag) check.
fn strip_fenced_code_blocks(text: &str) -> String {
    text.split("```")
        .enumerate()
        .filter_map(|(i, part)| (i % 2 == 0).then_some(part))
        .collect::<Vec<_>>()
        .join("")
}

fn contract_doc() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("docs")
        .join("cli-contract.md");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    strip_fenced_code_blocks(&text)
}

#[test]
fn every_help_flag_is_documented_and_every_documented_flag_is_real() {
    let doc = contract_doc();
    let doc_flags = flag_tokens(&doc);

    let mut all_real_flags: BTreeSet<String> = BTreeSet::new();
    let mut missing_from_doc: Vec<String> = Vec::new();

    for path in PATHS {
        let text = help_text(path);
        let flags = flag_tokens(&text);
        for flag in &flags {
            all_real_flags.insert(flag.clone());
            if !doc_flags.contains(flag) {
                missing_from_doc.push(format!(
                    "`pm {}` has `{flag}` (from --help), but docs/cli-contract.md never mentions it",
                    path.join(" ")
                ));
            }
        }
    }

    let phantom_in_doc: Vec<&String> = doc_flags.difference(&all_real_flags).collect();

    let mut problems = missing_from_doc;
    problems.extend(phantom_in_doc.into_iter().map(|flag| {
        format!("docs/cli-contract.md mentions `{flag}`, which no `pm ... --help` output has")
    }));

    assert!(
        problems.is_empty(),
        "{} problem(s) between --help and docs/cli-contract.md:\n{}",
        problems.len(),
        problems.join("\n")
    );
}

/// Every path in [`PATHS`] is one clap actually recognizes (catches a typo
/// in the list itself, and a subcommand this test forgot to add).
#[test]
fn every_listed_path_is_a_real_command() {
    for path in PATHS {
        let text = help_text(path);
        assert!(
            text.starts_with(char::is_alphabetic) || text.contains("Usage:"),
            "`pm {} --help` did not look like help text:\n{text}",
            path.join(" ")
        );
    }
}
