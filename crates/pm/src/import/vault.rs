//! Reading a saltline-style vault from disk (AGT-1347 AC1, AC2, AC4, AC6):
//! `tickets/**` and `archive/20*/**` ticket files, `projects/*/README.md`
//! (and the retired ones under `archive/projects/`) with their sibling
//! and nested `.md` documents and `ideation/IDEA-*`.
//!
//! The vault's frontmatter is YAML in spirit but hand-written in
//! practice — 682 bare titles, some with `: ` or ` #` in them, which a
//! strict parser reads as a mapping or a comment. [`quote_bare_scalars`]
//! quotes every top-level bare value before the YAML parser sees it, so
//! the text of every value survives exactly (AC6: "non-template values
//! import without loss") and the same `FileFrontmatter` that `pm new
//! --from-file` uses (`crate::batch`) does the rest. Every failure names
//! the file and the line.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use pm_core::markers::parse_date;
use pm_core::{Priority, ProjectStatus, Source, Waiver, Workspace};
use serde_json::Value;

use super::prose::{self, CommentEntry, Markers, Migrated};
use crate::batch::{self, FileFrontmatter, SourceFm};
use crate::exit::{CliError, Result};

/// Vault files pm knows were clobbered, and the git object holding their
/// last good version (projects/pm/research/vault-anatomy.md "Anomalies"):
/// a file at this relative path with no frontmatter is read from
/// `git show <rev>:<path>` instead, and whatever the working-tree file
/// still holds (comment entries, typically) is appended. `--recover
/// PATH=REV` adds to this list.
pub const RECOVER_FROM_GIT: [(&str, &str); 1] = [(
    "tickets/triage/AGT-806-provisioned-subdomain-never-serves-the-site.md",
    "82a9982",
)];

/// The `source.type` values 00-meta/templates/ticket.md allows; anything
/// else is reported as non-template (AC6, `audit`).
const TEMPLATE_SOURCE_TYPES: [&str; 5] = ["manual", "github", "linear", "jira", "notion"];

/// One vault ticket file, parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultTicket {
    /// Relative to the vault root.
    pub path: PathBuf,
    pub number: u64,
    pub title: String,
    pub state: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub blocked_by: Vec<u64>,
    pub linked_github: Option<String>,
    pub linked_pr: Option<String>,
    pub priority: Priority,
    pub labels: BTreeSet<String>,
    pub source: Option<Source>,
    /// Every frontmatter key pm has no field for, verbatim.
    pub ext: BTreeMap<String, Value>,
    /// The body minus `## Comments` entries and migrated marker lines.
    pub description: String,
    pub comments: Vec<VaultComment>,
    pub waivers: Vec<Waiver>,
    pub hold: Option<String>,
    /// `Parked.until` and, for a prose value, the prose for
    /// `ext.parked_reason`.
    pub parked: Option<(String, Option<String>)>,
    /// `archive/YYYY-MM/` → the month's first day, UTC midnight.
    pub archived_month_ms: Option<u64>,
    /// What marker migration removed, for the report.
    pub migrated: Vec<Migrated>,
    /// Read from a git object because the working-tree file had lost its
    /// frontmatter.
    pub recovered: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultComment {
    pub date_ms: u64,
    pub author: String,
    pub body: String,
}

/// One project folder: its README (metadata + design doc) and named
/// documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VaultProject {
    pub id: String,
    pub title: String,
    pub status: ProjectStatus,
    pub parent: Option<String>,
    pub repos: BTreeSet<String>,
    /// The README verbatim — frontmatter included, so the snapshot
    /// re-imports byte for byte.
    pub doc: String,
    pub doc_mtime_ms: u64,
    /// `name` → (text, mtime).
    pub documents: BTreeMap<String, (String, u64)>,
    pub path: PathBuf,
}

/// A non-template value or shape (AC6), by category, naming `path:line`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Findings {
    pub non_template: BTreeMap<String, Vec<String>>,
    /// Every frontmatter key that landed in `ext`, with how many files
    /// carried it.
    pub ext_keys: BTreeMap<String, usize>,
    pub anomalies: Vec<String>,
}

impl Findings {
    fn note(&mut self, category: &str, at: String) {
        self.non_template
            .entry(category.to_string())
            .or_default()
            .push(at);
    }
}

/// Everything read from the vault.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    /// The vault's canonical root.
    pub root: PathBuf,
    pub tickets: Vec<VaultTicket>,
    pub projects: Vec<VaultProject>,
    pub findings: Findings,
}

/// Reads the whole vault at `root`. Any file that does not parse fails
/// the import before anything is written, naming the file and line.
/// `recover` is `(relative path, git rev)` pairs beyond
/// [`RECOVER_FROM_GIT`].
pub fn read(root: &Path, ws: &Workspace, recover: &[(String, String)]) -> Result<Snapshot> {
    let root = fs::canonicalize(root).with_context(|| format!("resolving {}", root.display()))?;
    if !root.join("tickets").is_dir() {
        return Err(CliError::usage(format!(
            "{} is not a vault: no tickets/ directory",
            root.display()
        )));
    }
    let recover: Vec<(&str, &str)> = RECOVER_FROM_GIT
        .iter()
        .copied()
        .chain(recover.iter().map(|(p, r)| (p.as_str(), r.as_str())))
        .collect();
    let mut snapshot = Snapshot {
        root: root.clone(),
        ..Snapshot::default()
    };
    for path in ticket_files(&root)? {
        let rel = path.strip_prefix(&root).unwrap_or(&path).to_path_buf();
        let ticket = read_ticket(&root, &rel, ws, &recover, &mut snapshot.findings)?;
        snapshot.tickets.push(ticket);
    }
    merge_recovered(&mut snapshot, &ws.prefix);
    for dir in project_dirs(&root)? {
        let rel = dir.strip_prefix(&root).unwrap_or(&dir).to_path_buf();
        snapshot
            .projects
            .push(read_project(&root, &rel, &mut snapshot.findings)?);
    }
    Ok(snapshot)
}

// ------------------------------------------------------------ discovery

/// `tickets/**/*.md` and `archive/20*/**/*.md`, sorted; `assets/`
/// folders (screenshots) are skipped.
fn ticket_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    walk_md(&root.join("tickets"), &mut files)?;
    if let Ok(entries) = fs::read_dir(root.join("archive")) {
        for entry in entries {
            let path = entry.context("listing archive/")?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() && name.starts_with("20") {
                walk_md(&path, &mut files)?;
            }
        }
    }
    files.sort();
    Ok(files)
}

fn walk_md(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))?;
    for entry in entries {
        let path = entry
            .with_context(|| format!("listing {}", dir.display()))?
            .path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if name != "assets" && !name.starts_with('.') {
                walk_md(&path, out)?;
            }
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
    Ok(())
}

/// `projects/*/` then `archive/projects/*/`, each with a README.md —
/// retired projects are imported too, since archived tickets still name
/// them (R2: a ticket's project must exist). A live project shadows a
/// retired one of the same id.
fn project_dirs(root: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    let mut seen = BTreeSet::new();
    for base in [root.join("projects"), root.join("archive").join("projects")] {
        let Ok(entries) = fs::read_dir(&base) else {
            continue;
        };
        let mut found: Vec<PathBuf> = entries
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("listing {}", base.display()))?;
        found.sort();
        for dir in found {
            let id = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if dir.is_dir()
                && !id.starts_with('.')
                && dir.join("README.md").is_file()
                && seen.insert(id.to_string())
            {
                dirs.push(dir);
            }
        }
    }
    Ok(dirs)
}

// -------------------------------------------------------------- tickets

/// A recovered file whose id another, intact file also carries (AGT-806:
/// the clobbered triage file beside its archived, done copy) is not a
/// second ticket: its comment entries the intact file lacks are appended
/// to that ticket, and the file is dropped from the list.
fn merge_recovered(snapshot: &mut Snapshot, prefix: &str) {
    let (recovered, mut kept): (Vec<VaultTicket>, Vec<VaultTicket>) =
        std::mem::take(&mut snapshot.tickets)
            .into_iter()
            .partition(|t| t.recovered);
    for fragment in recovered {
        let Some(canonical) = kept.iter_mut().find(|t| t.number == fragment.number) else {
            kept.push(fragment);
            continue;
        };
        let mut added = 0;
        for c in fragment.comments {
            if !canonical
                .comments
                .iter()
                .any(|k| k.author == c.author && k.body == c.body)
            {
                canonical.comments.push(c);
                added += 1;
            }
        }
        snapshot.findings.anomalies.push(format!(
            "{}: {prefix}-{} also has an intact file at {}; {added} comment(s) the intact file lacked were appended to it, nothing else taken from the recovered version",
            fragment.path.display(),
            fragment.number,
            canonical.path.display()
        ));
    }
    kept.sort_by(|a, b| a.path.cmp(&b.path));
    snapshot.tickets = kept;
}

fn read_ticket(
    root: &Path,
    rel: &Path,
    ws: &Workspace,
    recover: &[(&str, &str)],
    findings: &mut Findings,
) -> Result<VaultTicket> {
    let path = root.join(rel);
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let shown = rel.display().to_string();

    let (text, appended) = if text.starts_with("---\n") {
        (text, None)
    } else {
        let rel_str = rel.to_string_lossy();
        let Some((_, rev)) = recover.iter().find(|(p, _)| *p == rel_str) else {
            return Err(CliError::usage(format!(
                "{shown}:1: file has no frontmatter block (expected a leading '---' line)"
            )));
        };
        let recovered = git_show(root, rev, &rel_str)?;
        findings.anomalies.push(format!(
            "{shown}: no frontmatter; imported from git object {rev} with the working-tree text appended as comments"
        ));
        (recovered, Some(text))
    };
    parse_ticket(&text, appended.as_deref(), rel, ws, findings)
}

/// Parses one ticket file's text (`pub(super)`: the parity report parses
/// `pm export md`'s output with exactly this). `appended` is the text of
/// a clobbered working-tree file whose comment entries are added to the
/// recovered `text`.
pub(super) fn parse_ticket(
    text: &str,
    appended: Option<&str>,
    rel: &Path,
    ws: &Workspace,
    findings: &mut Findings,
) -> Result<VaultTicket> {
    let shown = rel.display().to_string();
    let (fm_text, body) = batch::split_frontmatter(text)
        .map_err(|e| CliError::usage(format!("{shown}:1: {:#}", e.error)))?;
    let normalized = quote_bare_scalars(fm_text, &shown, findings);
    let fm: FileFrontmatter = crate::yaml::from_str(&normalized).map_err(|e| {
        let at = crate::yaml::line_of(&e)
            .map(|l| format!("{shown}:{}", l + 1))
            .unwrap_or_else(|| shown.clone());
        CliError::usage(format!("{at}: parsing frontmatter: {e}"))
    })?;

    let at = |key: &str| format!("{shown}:{}", fm_line(fm_text, key));
    let scalar = |key: &str, v: &Option<serde_json::Value>| -> Result<String> {
        match v {
            Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            _ => Err(CliError::usage(format!("{}: missing {key}", at(key)))),
        }
    };
    let id = scalar("id", &fm.id)?;
    let number = parse_id(&id, &ws.prefix).ok_or_else(|| {
        CliError::usage(format!(
            "{}: id '{id}' is not {}-<number>",
            at("id"),
            ws.prefix
        ))
    })?;
    let state = scalar("state", &fm.state)?;
    if ws.state(&state).is_none() {
        let known: Vec<&str> = ws.states.iter().map(|s| s.name.as_str()).collect();
        return Err(CliError::usage(format!(
            "{}: state '{state}' is not a workflow state (expected one of {})",
            at("state"),
            known.join(", ")
        )));
    }
    let created = scalar("created", &fm.created)?;
    let created_ms = parse_timestamp(&created).ok_or_else(|| {
        CliError::usage(format!(
            "{}: created '{created}' is not a date",
            at("created")
        ))
    })?;
    let updated = scalar("updated", &fm.updated)?;
    let updated_ms = parse_timestamp(&updated).ok_or_else(|| {
        CliError::usage(format!(
            "{}: updated '{updated}' is not a date",
            at("updated")
        ))
    })?;
    if updated.contains('T') {
        findings.note("updated: ISO datetime", at("updated"));
    }
    let title = fm
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| CliError::usage(format!("{}: missing title", at("title"))))?
        .to_string();
    if fm.priority == Some(Priority::Critical) {
        findings.note("priority: critical", at("priority"));
    }
    if fm_text
        .lines()
        .any(|l| l.starts_with("linked-pr: \"") || l.starts_with("linked-pr: '"))
    {
        findings.note("linked-pr: quoted", at("linked-pr"));
    }
    if let Some(kind) = fm.source.as_ref().and_then(|s| s.kind.as_deref())
        && !TEMPLATE_SOURCE_TYPES.contains(&kind)
    {
        findings.note(&format!("source.type: {kind}"), at("source"));
    }
    let blocked_by = fm
        .blocked_by
        .iter()
        .map(|b| {
            parse_id(b.trim(), &ws.prefix).ok_or_else(|| {
                CliError::usage(format!(
                    "{}: blocked-by '{b}' is not {}-<number>",
                    at("blocked-by"),
                    ws.prefix
                ))
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut ext_yaml = fm.ext;
    let mut markers = Markers::default();
    let mut migrated = Vec::new();
    // A `waived:` frontmatter key (AGT-1093) is the same marker in a
    // different place; `hold:` and `parked:` keys are how `pm export md`
    // writes the structured markers back without `--legacy-markers`.
    // Each is read exactly like its prose form.
    for (key, kind) in [
        ("waived", prose::MarkerKind::Waiver),
        ("hold", prose::MarkerKind::Hold),
        ("parked", prose::MarkerKind::Parked),
    ] {
        let values: Vec<String> = match ext_yaml.remove(key) {
            Some(serde_json::Value::String(s)) => vec![s],
            Some(serde_json::Value::Array(items)) => items
                .into_iter()
                .filter_map(|v| match v {
                    serde_json::Value::String(s) => Some(s),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        for value in values.iter().map(|v| v.trim()).filter(|v| !v.is_empty()) {
            migrated.push(Migrated {
                kind,
                text: format!("{key}: {value}"),
            });
            match kind {
                prose::MarkerKind::Waiver => markers.waivers.push(prose::parse_waiver(value)),
                prose::MarkerKind::Hold => markers.holds.push(value.to_string()),
                prose::MarkerKind::Parked => markers.parked.push(value.to_string()),
            }
        }
    }
    for key in ext_yaml.keys() {
        *findings.ext_keys.entry(key.clone()).or_default() += 1;
    }
    let mut ext = batch::ext_to_json(ext_yaml)?;

    let recovered = appended.is_some();
    let mut split = prose::split_body(body);
    if let Some(fragment) = appended {
        // What survives in a clobbered file is comment entries with no
        // heading above them; read them as a Comments section.
        let extra = prose::split_body(&format!("## Comments\n{fragment}"));
        if !extra.description.is_empty() {
            split.description.push_str("\n\n");
            split.description.push_str(&extra.description);
        }
        split.comments.extend(extra.comments);
    }
    let description = prose::migrate(&split.description, &mut markers, &mut migrated);
    let mut comments = Vec::with_capacity(split.comments.len());
    for c in split.comments {
        let mut body = prose::migrate(&c.body, &mut markers, &mut migrated);
        if body.is_empty() {
            // A comment that was nothing but a marker keeps its text: the
            // marker is structured now, and the entry still happened.
            body = prose::tidy(&c.body);
        }
        let CommentEntry { date, author, .. } = c;
        let date_ms = parse_timestamp(&date).expect("comment_header validated the date");
        comments.push(VaultComment {
            date_ms,
            author,
            body,
        });
    }
    // The same marker text twice (a comment that was nothing but a
    // `waived:` line keeps its text *and* becomes a waiver, so an
    // exported file carries it in both places) is one marker.
    dedup(&mut markers.waivers);
    dedup(&mut markers.holds);
    let hold = (!markers.holds.is_empty()).then(|| markers.holds.join("; "));
    let parked = markers.parked.last().map(|p| prose::parked_until(p));
    if let Some((_, Some(reason))) = &parked {
        ext.insert("parked_reason".to_string(), Value::String(reason.clone()));
    }
    let archived_month_ms = archive_month(rel).map(|(y, m)| date_ms(y, m, 1));

    Ok(VaultTicket {
        path: rel.to_path_buf(),
        number,
        title,
        state,
        created_ms,
        updated_ms,
        project: fm
            .project
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty()),
        repo: fm
            .repo
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty()),
        blocked_by,
        linked_github: fm
            .linked_github
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        linked_pr: fm
            .linked_pr
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        priority: fm.priority.unwrap_or_default(),
        labels: fm
            .labels
            .iter()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
        source: fm.source.map(SourceFm::into_source),
        ext,
        description,
        comments,
        waivers: markers.waivers,
        hold,
        parked,
        archived_month_ms,
        migrated,
        recovered,
    })
}

/// Drops later repeats, keeping first occurrences in order.
fn dedup<T: PartialEq>(items: &mut Vec<T>) {
    let mut kept: Vec<T> = Vec::with_capacity(items.len());
    for item in items.drain(..) {
        if !kept.contains(&item) {
            kept.push(item);
        }
    }
    *items = kept;
}

/// `archive/2026-08/…` → `(2026, 8)`.
fn archive_month(rel: &Path) -> Option<(i64, u32)> {
    let mut parts = rel.components();
    if parts.next()?.as_os_str() != "archive" {
        return None;
    }
    let month = parts.next()?.as_os_str().to_str()?;
    let (y, m) = month.split_once('-')?;
    let (y, m) = (y.parse().ok()?, m.parse().ok()?);
    (1..=12).contains(&m).then_some((y, m))
}

/// `AGT-834` → `834` when the prefix matches (case-insensitively).
pub fn parse_id(id: &str, prefix: &str) -> Option<u64> {
    let (p, digits) = id.rsplit_once('-')?;
    if !p.eq_ignore_ascii_case(prefix)
        || digits.is_empty()
        || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    digits.parse().ok()
}

/// The 1-based file line of the top-level `key:` line in a frontmatter
/// block (line 1 is the opening fence), for error messages.
fn fm_line(fm_text: &str, key: &str) -> usize {
    fm_text
        .lines()
        .position(|l| l.starts_with(key) && l[key.len()..].starts_with(':'))
        .map_or(1, |i| i + 2)
}

/// `git -C <root> show <rev>:<path>` — read-only; the vault is never
/// written.
fn git_show(root: &Path, rev: &str, rel: &str) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("show")
        .arg(format!("{rev}:{rel}"))
        .output()
        .with_context(|| format!("running git show {rev}:{rel}"))?;
    if !out.status.success() {
        return Err(CliError::error(format!(
            "{rel}: recovering from git object {rev} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    String::from_utf8(out.stdout)
        .map_err(|_| CliError::error(format!("{rel}: git object {rev} is not UTF-8")))
}

/// Quotes every top-level bare scalar (`title: deps: pass #62` →
/// `title: "deps: pass #62"`) so YAML reads it as the text it is. Flow
/// lists/maps, already-quoted values, empty values and indented lines
/// are left as they are. A bare value that YAML would have misread (a
/// `: ` inside it, or an opening quote never closed) is noted as a
/// non-template shape.
pub fn quote_bare_scalars(fm_text: &str, shown: &str, findings: &mut Findings) -> String {
    let mut out = String::with_capacity(fm_text.len() + 64);
    for (i, line) in fm_text.lines().enumerate() {
        let quoted = top_level_key(line).and_then(|(key, value)| {
            let v = value.trim();
            let quoted_ok = |q: char| v.starts_with(q) && v.len() > 1 && v.ends_with(q);
            if v.is_empty()
                || v.starts_with('[')
                || v.starts_with('{')
                || quoted_ok('"')
                || quoted_ok('\'')
            {
                return None;
            }
            if v.contains(": ") {
                findings.note(
                    "frontmatter: bare value containing ': '",
                    format!("{shown}:{}", i + 2),
                );
            } else if v.starts_with('"') || v.starts_with('\'') {
                findings.note(
                    "frontmatter: unterminated quote",
                    format!("{shown}:{}", i + 2),
                );
            }
            let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
            Some(format!("{key}: \"{escaped}\""))
        });
        out.push_str(quoted.as_deref().unwrap_or(line));
        out.push('\n');
    }
    out
}

/// `key: value` at column 0, when `key` looks like a frontmatter key.
pub(super) fn top_level_key(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    let ok = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && (value.is_empty() || value.starts_with(' '));
    ok.then_some((key, value))
}

// ------------------------------------------------------------- projects

fn read_project(root: &Path, rel: &Path, findings: &mut Findings) -> Result<VaultProject> {
    let dir = root.join(rel);
    let id = rel
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    let readme = dir.join("README.md");
    let doc =
        fs::read_to_string(&readme).with_context(|| format!("reading {}", readme.display()))?;
    let shown = rel.join("README.md").display().to_string();

    let mut title = None;
    let mut status = None;
    let mut parent = None;
    let mut repos = BTreeSet::new();
    if doc.starts_with("---\n") {
        let (fm_text, _) = batch::split_frontmatter(&doc)
            .map_err(|e| CliError::usage(format!("{shown}:1: {:#}", e.error)))?;
        let normalized = quote_bare_scalars(fm_text, &shown, findings);
        let fm: ProjectFm = crate::yaml::from_str(&normalized).map_err(|e| {
            let at = crate::yaml::line_of(&e)
                .map(|l| format!("{shown}:{}", l + 1))
                .unwrap_or_else(|| shown.clone());
            CliError::usage(format!("{at}: parsing frontmatter: {e}"))
        })?;
        title = fm
            .title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        if let Some(s) = fm
            .status
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
        {
            let mapped = project_status(&s);
            if mapped.is_none() {
                findings.note(
                    &format!("project status: {s} (unknown, kept as in-progress)"),
                    shown.clone(),
                );
            } else if !matches!(s.as_str(), "in-progress" | "complete" | "abandoned") {
                findings.note(&format!("project status: {s}"), shown.clone());
            }
            status = Some(mapped.unwrap_or(ProjectStatus::InProgress));
        }
        parent = fm
            .parent
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty());
        repos = fm
            .repos
            .into_iter()
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect();
    } else {
        findings.anomalies.push(format!(
            "{shown}: no frontmatter; title taken from the first heading"
        ));
    }
    let title = title
        .or_else(|| {
            doc.lines()
                .find_map(|l| l.strip_prefix("# "))
                .map(|h| h.trim().to_string())
                .filter(|h| !h.is_empty())
        })
        .unwrap_or_else(|| id.clone());

    let documents = project_documents(&dir, rel, findings)?;

    Ok(VaultProject {
        id,
        title,
        status: status.unwrap_or(ProjectStatus::InProgress),
        parent,
        repos,
        doc_mtime_ms: mtime_ms(&readme),
        doc,
        documents,
        path: rel.to_path_buf(),
    })
}

/// A project's top-level subfolders that hold vault resources, not
/// documents (vault-anatomy.md: research notes, committed screenshots,
/// gitignored design-QA captures). Nothing under them is imported or
/// reported. `ideation/` is not here: it has its own rule, see
/// [`project_documents`].
pub const RESOURCE_SUBTREES: [&str; 3] = ["research", "assets", "design-qa"];

/// A project folder's named documents, `name` → (text, mtime): every
/// `.md` under it except its own README.md, named by its path relative to
/// the folder minus `.md` (`EXECUTION`, `cutover/README`,
/// `blog-drafts/AGT-679-preview`; AGT-1485). `ideation/` keeps its rule —
/// only the `IDEA-*.md` directly in it, as `ideation/IDEA-…` — and the
/// [`RESOURCE_SUBTREES`] are skipped whole. Hidden entries are skipped.
/// A non-markdown file is never imported; each project's are listed in
/// one anomaly so the report shows what stayed behind, as is a markdown
/// file whose path is not a safe document name.
fn project_documents(
    dir: &Path,
    rel: &Path,
    findings: &mut Findings,
) -> Result<BTreeMap<String, (String, u64)>> {
    let mut files = Vec::new();
    walk_project(dir, dir, &mut files)?;
    files.sort();
    let mut documents = BTreeMap::new();
    let mut not_markdown = Vec::new();
    for path in files {
        let inner = path.strip_prefix(dir).unwrap_or(&path);
        let segments: Vec<&str> = inner
            .iter()
            .map(|s| s.to_str().unwrap_or_default())
            .collect();
        let shown = rel.join(inner).display().to_string();
        if !path.extension().is_some_and(|e| e == "md") {
            not_markdown.push(shown);
            continue;
        }
        let doc_name = segments.join("/");
        let doc_name = doc_name.trim_end_matches(".md");
        if !pm_core::ids::is_safe_doc_name(doc_name) {
            findings
                .anomalies
                .push(format!("{shown}: not a safe document name; not imported"));
            continue;
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        documents.insert(doc_name.to_string(), (text, mtime_ms(&path)));
    }
    if !not_markdown.is_empty() {
        findings.anomalies.push(format!(
            "{}: {} non-markdown file(s) left in the vault, not imported: {}",
            rel.display(),
            not_markdown.len(),
            not_markdown.join(", ")
        ));
    }
    Ok(documents)
}

/// The candidate files under project folder `root`, from `dir` down:
/// see [`project_documents`] for what is skipped.
fn walk_project(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))?;
    let top = dir == root;
    for entry in entries {
        let path = entry
            .with_context(|| format!("listing {}", dir.display()))?
            .path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if top && RESOURCE_SUBTREES.contains(&name) {
                continue;
            }
            if top && name == "ideation" {
                for idea in
                    fs::read_dir(&path).with_context(|| format!("listing {}", path.display()))?
                {
                    let idea = idea
                        .with_context(|| format!("listing {}", path.display()))?
                        .path();
                    let n = idea.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if idea.is_file()
                        && n.starts_with("IDEA-")
                        && idea.extension().is_some_and(|e| e == "md")
                    {
                        out.push(idea);
                    }
                }
                continue;
            }
            walk_project(root, &path, out)?;
        } else if path.is_file() && !(top && name == "README.md") {
            out.push(path);
        }
    }
    Ok(())
}

/// A project README's frontmatter (00-meta/templates/project.md), plus
/// the keys some READMEs add (`created`, `type`, …), which are ignored:
/// the README is stored verbatim as the design doc, so they are not lost.
#[derive(Debug, Default, serde::Deserialize)]
struct ProjectFm {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(rename = "parent-project", default)]
    parent: Option<String>,
    #[serde(default)]
    repos: Vec<String>,
}

/// The vault's `status:` spellings onto pm's three (README §Data model).
fn project_status(s: &str) -> Option<ProjectStatus> {
    Some(match s {
        "in-progress" | "active" | "planning" => ProjectStatus::InProgress,
        "complete" | "done" | "shipped" | "phase-1-complete" => ProjectStatus::Complete,
        "abandoned" | "obsolete" => ProjectStatus::Abandoned,
        _ => return None,
    })
}

fn mtime_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

// ---------------------------------------------------------------- dates

/// `YYYY-MM-DD` → UTC midnight; `YYYY-MM-DDTHH:MM[:SS[.fff]][Z|±HH:MM]`
/// → that instant. Milliseconds since the Unix epoch.
pub fn parse_timestamp(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    parse_date(date).ok()?;
    let year: i64 = date[0..4].parse().ok()?;
    let month: u32 = date[5..7].parse().ok()?;
    let day: u32 = date[8..10].parse().ok()?;
    let mut ms = date_ms(year, month, day);
    let Some(time) = time else {
        return Some(ms);
    };
    // Split off the zone: `Z`, `+HH:MM` or `-HH:MM`.
    let (clock, offset_ms) = if let Some(t) = time.strip_suffix('Z') {
        (t, 0i64)
    } else if let Some(pos) = time.rfind(['+', '-']) {
        let (t, zone) = time.split_at(pos);
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let (h, m) = zone[1..].split_once(':').unwrap_or((&zone[1..], "0"));
        let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
        (t, sign * (h * 3_600_000 + m * 60_000))
    } else {
        (time, 0)
    };
    let mut parts = clock.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let (second, millis): (i64, i64) = match parts.next() {
        None => (0, 0),
        Some(sec) => {
            let (whole, frac) = sec.split_once('.').unwrap_or((sec, ""));
            let frac: String = frac.chars().chain("000".chars()).take(3).collect();
            (whole.parse().ok()?, frac.parse().ok()?)
        }
    };
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let of_day = hour * 3_600_000 + minute * 60_000 + second * 1000 + millis;
    ms = (ms as i64 + of_day - offset_ms).max(0) as u64;
    Some(ms)
}

/// UTC midnight of a civil date, in epoch milliseconds (Howard Hinnant's
/// `days_from_civil`, the inverse of `pm_core::markers::date_from_ms`).
pub fn date_ms(year: i64, month: u32, day: u32) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    (days.max(0) as u64) * 86_400_000
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::markers::date_from_ms;

    #[test]
    fn timestamps_parse_dates_and_iso_datetimes() {
        assert_eq!(parse_timestamp("1970-01-01"), Some(0));
        assert_eq!(parse_timestamp("2000-02-29"), Some(951_782_400_000));
        assert_eq!(
            date_from_ms(parse_timestamp("2026-09-28").unwrap()),
            "2026-09-28"
        );
        assert_eq!(
            parse_timestamp("2026-05-12T21:17:00.000Z"),
            Some(parse_timestamp("2026-05-12").unwrap() + 21 * 3_600_000 + 17 * 60_000)
        );
        assert_eq!(
            parse_timestamp("2026-06-09T04:00:00Z"),
            Some(parse_timestamp("2026-06-09").unwrap() + 4 * 3_600_000)
        );
        assert_eq!(
            parse_timestamp("2026-06-09T04:00:00+02:00"),
            Some(parse_timestamp("2026-06-09").unwrap() + 2 * 3_600_000)
        );
        assert_eq!(parse_timestamp("2026-13-01"), None);
        assert_eq!(parse_timestamp("2026-06-09T25:00"), None);
        assert_eq!(parse_timestamp("yesterday"), None);
    }

    #[test]
    fn bare_scalars_are_quoted_and_the_rest_left_alone() {
        let mut f = Findings::default();
        let fm = "id: AGT-7\ntitle: deps: pass (#62, #92)\nstate: done\nrepo: \nlabels: [a, b]\nsource: { type: manual, url: \"\" }\nlinked-pr: \"https://x\"\nnote: say \"hi\" \\ there\nbad: \"open\n";
        let q = quote_bare_scalars(fm, "f.md", &mut f);
        assert_eq!(
            q,
            "id: \"AGT-7\"\ntitle: \"deps: pass (#62, #92)\"\nstate: \"done\"\nrepo: \nlabels: [a, b]\nsource: { type: manual, url: \"\" }\nlinked-pr: \"https://x\"\nnote: \"say \\\"hi\\\" \\\\ there\"\nbad: \"\\\"open\"\n"
        );
        let fm: FileFrontmatter = crate::yaml::from_str(&q).unwrap();
        assert_eq!(fm.title.as_deref(), Some("deps: pass (#62, #92)"));
        assert_eq!(
            fm.ext["note"],
            serde_json::Value::String("say \"hi\" \\ there".into())
        );
        assert_eq!(
            f.non_template["frontmatter: bare value containing ': '"],
            ["f.md:3"]
        );
        assert_eq!(
            f.non_template["frontmatter: unterminated quote"],
            ["f.md:10"]
        );
    }

    #[test]
    fn ids_and_archive_months() {
        assert_eq!(parse_id("AGT-834", "AGT"), Some(834));
        assert_eq!(parse_id("agt-1", "AGT"), Some(1));
        assert_eq!(parse_id("BUG-1", "AGT"), None);
        assert_eq!(parse_id("AGT-", "AGT"), None);
        assert_eq!(
            archive_month(Path::new("archive/2026-08/AGT-846-x.md")),
            Some((2026, 8))
        );
        assert_eq!(archive_month(Path::new("tickets/done/AGT-1220.md")), None);
        assert_eq!(fm_line("id: x\ntitle: t\nstate: s", "state"), 4);
        assert_eq!(project_status("shipped"), Some(ProjectStatus::Complete));
        assert_eq!(project_status("weird"), None);
    }
}
