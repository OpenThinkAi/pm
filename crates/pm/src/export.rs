//! `pm export md <dir>` (AGT-1348 AC1): every ticket and project written
//! back as vault-format markdown — the inverse of `pm import vault`.
//!
//! A ticket becomes `tickets/<state>/<id>-<slug>.md`, or
//! `archive/<YYYY-MM>/<id>-<slug>.md` once archived (the month of its
//! `archived_at`), with the frontmatter in 00-meta/templates/ticket.md's
//! key order — `id, title, state, created, updated, project, repo,
//! blocked-by, linked-github, linked-pr, priority, labels, source` — then
//! the pm-only fields that are set (`assignee`, `linear`, `not-before`),
//! then every `ext` key, sorted. The body is the description followed by
//! a `## Comments` section in the vault's `### <date> — <author>` entry
//! form. Values are quoted only when YAML needs it.
//!
//! The structured markers (`waivers`, `hold`, `parked`) have two
//! renderings. By default they are frontmatter keys — `waived:`, `hold:`,
//! `parked:` — which `pm import vault` reads back as the same markers.
//! With `--legacy-markers` they are the vault's prose lines instead
//! (`waived: <rule> — <reason>`, `⚠ NEEDS-HUMAN: <reason>`,
//! `parked: <until>`), appended to the description, so a file exported
//! from an imported vault ticket parses to what its source did: that is
//! what the import parity report (`pm import vault --report`) compares.
//!
//! A project becomes `projects/<id>/README.md` (the design doc verbatim,
//! so a README imported from a vault comes back byte for byte), or
//! `archive/projects/<id>/` once retired (status complete or abandoned),
//! with each named document beside it (`<name>.md`, `ideation/IDEA-*.md`).

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use pm_core::markers::{PARKED_FOREVER, date_from_ms};
use pm_core::{Project, RelationKind, Ticket, Workspace};
use pm_store::Store;
use serde_json::{Value, json};

use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA, display_id, print_json};

/// `ext` key the importer fills from a prose `parked:` value that is
/// not a date (`crate::import::vault`): rendered back into the prose
/// line under `--legacy-markers`, so it is not also written as a key.
pub const PARKED_REASON: &str = "parked_reason";

/// The longest slug a filename carries, as the vault's own filer cuts it.
const SLUG_MAX: usize = 50;

/// One file the export writes: its path relative to the export root, and
/// its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub path: PathBuf,
    pub text: String,
}

/// `pm export md <dir> [--legacy-markers]`.
pub fn md(ctx: &Ctx<'_>, dir: &Path, legacy_markers: bool) -> Result<()> {
    let (store, ws) = ctx.open()?;
    create_private_dirs(dir)?;
    let mut tickets = 0usize;
    let mut archived = 0usize;
    let mut unnumbered = 0usize;
    let mut files = 0usize;
    for t in store.all_tickets()?.iter().filter(|t| !t.deleted) {
        if t.number.is_none() {
            // The vault format has no id for a ticket the authority has
            // not numbered yet (`AGT-?`); it is not a file.
            unnumbered += 1;
            continue;
        }
        let rendered = render_ticket(&ws, &store, t, legacy_markers)?;
        write(dir, &rendered)?;
        files += 1;
        tickets += 1;
        if t.archived_at.is_some() {
            archived += 1;
        }
    }
    let mut projects = 0usize;
    let mut documents = 0usize;
    for p in store.projects()? {
        let rendered = render_project(&p)?;
        documents += rendered.len() - 1;
        for r in &rendered {
            write(dir, r)?;
        }
        files += rendered.len();
        projects += 1;
    }
    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "dir": dir.display().to_string(),
            "legacy_markers": legacy_markers,
            "tickets": tickets,
            "archived": archived,
            "unnumbered": unnumbered,
            "projects": projects,
            "documents": documents,
            "files": files,
        }));
    } else {
        let markers = if legacy_markers {
            "prose markers"
        } else {
            "frontmatter markers"
        };
        println!(
            "export md {}: {tickets} tickets ({archived} archived), {projects} projects, {documents} documents; {files} files, {markers}",
            dir.display()
        );
        if unnumbered > 0 {
            eprintln!("pm: {unnumbered} unnumbered ticket(s) not exported (no vault id yet)");
        }
    }
    Ok(())
}

/// Writes one rendered file under `root`, owner-only (AGT-1482): the export
/// is every ticket, comment and author name in the workspace, so it gets
/// the same 0700/0600 treatment as the editor's temp files (AGT-1468)
/// rather than the umask's usual world-readable 0644.
fn write(root: &Path, r: &Rendered) -> Result<()> {
    let path = root.join(&r.path);
    if let Some(parent) = path.parent() {
        create_private_dirs(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    // `mode` applies only to a file this call creates; a file an earlier
    // export (or anyone) left behind is narrowed too.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .with_context(|| format!("restricting {}", path.display()))?;
    }
    std::io::Write::write_all(&mut file, r.text.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// `create_dir_all`, with every directory it creates 0700 (AGT-1482). A
/// directory that already exists — the export root the operator named,
/// say, or their home — is left as it is.
fn create_private_dirs(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    Ok(())
}

// -------------------------------------------------------------- tickets

/// Where a ticket's file goes, relative to the export root. The state
/// folder is a workspace state name, checked as a path segment too.
pub(crate) fn ticket_path(ws: &Workspace, t: &Ticket) -> Result<PathBuf> {
    crate::ids::safe_component(&ws.prefix, "workspace prefix")?;
    let id = display_id(ws, t);
    let slug = slug(&t.title);
    let name = if slug.is_empty() {
        format!("{id}.md")
    } else {
        format!("{id}-{slug}.md")
    };
    Ok(match t.archived_at {
        Some(at) => {
            let month = date_from_ms(at.wall_ms);
            Path::new("archive").join(&month[..7]).join(name)
        }
        None => {
            if ws.state(&t.state).is_none() {
                return Err(CliError::error(format!(
                    "{id}: state '{}' is not a workflow state",
                    t.state
                )));
            }
            Path::new("tickets")
                .join(segment(&t.state, "state")?)
                .join(name)
        }
    })
}

/// A path built from ticket or project data (`state`, a project id, a
/// document name such as `ideation/IDEA-1`) must stay under the export
/// root: only plain components, so no `..`, no `.`, no leading `/`, no
/// empty segment, and nothing a filesystem cannot take.
pub(crate) fn segment<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    let ok = !value.is_empty()
        && !value.contains('\0')
        && !value.ends_with('/')
        && Path::new(value)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    if ok {
        Ok(value)
    } else {
        Err(CliError::error(format!(
            "{what} '{value}' is not a safe path segment (must be plain names without `..`)"
        )))
    }
}

/// The vault filer's slug: lowercase, every run of non-alphanumerics a
/// single `-`, cut at [`SLUG_MAX`] bytes, no leading/trailing `-`.
pub(crate) fn slug(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c.to_ascii_lowercase());
        } else {
            dash = true;
        }
    }
    if out.len() > SLUG_MAX {
        out.truncate(SLUG_MAX);
    }
    out.trim_end_matches('-').to_string()
}

/// A ticket as one vault-format file (see the module docs).
pub(crate) fn render_ticket(
    ws: &Workspace,
    store: &Store,
    t: &Ticket,
    legacy_markers: bool,
) -> Result<Rendered> {
    let id = display_id(ws, t);
    let mut blockers: Vec<(u64, String)> = Vec::new();
    for r in store.relations(t.id)? {
        if r.kind == RelationKind::Blocks && r.to == t.id {
            let (n, shown) = match store.ticket(r.from)? {
                Some(other) => (other.number.unwrap_or(u64::MAX), display_id(ws, &other)),
                None => (u64::MAX, r.from.to_string()),
            };
            blockers.push((n, shown));
        }
    }
    blockers.sort();
    let comments = store.comments(t.id)?;

    let mut fm = String::new();
    let mut put = |key: &str, value: String| {
        fm.push_str(key);
        fm.push(':');
        if !value.is_empty() {
            fm.push(' ');
            fm.push_str(&value);
        }
        fm.push('\n');
    };
    put("id", id.clone());
    put("title", scalar(&t.title));
    put("state", scalar(&t.state));
    put("created", date_from_ms(t.created.wall_ms));
    put("updated", date_from_ms(t.updated.wall_ms));
    put("project", opt(&t.project));
    put("repo", opt(&t.repo));
    put(
        "blocked-by",
        flow_list(blockers.iter().map(|(_, shown)| shown.as_str())),
    );
    put("linked-github", opt(&t.linked_github));
    put("linked-pr", opt(&t.linked_pr));
    put("priority", priority_name(t));
    put("labels", flow_list(t.labels.iter().map(String::as_str)));
    put(
        "source",
        match &t.source {
            Some(s) => format!(
                "{{ type: {}, url: {}, id: {}, fetched-at: {} }}",
                flow_scalar(&s.kind),
                quoted(&s.url),
                quoted(&s.id),
                quoted(&s.fetched_at)
            ),
            None => String::new(),
        },
    );
    if let Some(a) = &t.assignee {
        put("assignee", scalar(a.as_str()));
    }
    if let Some(l) = &t.linear {
        put("linear", scalar(l));
    }
    if let Some(nb) = &t.not_before {
        put("not-before", nb.date.clone());
    }

    let parked_reason = t
        .ext
        .get(PARKED_REASON)
        .and_then(Value::as_str)
        .map(str::to_string);
    let markers = marker_lines(t, parked_reason.as_deref());
    if !legacy_markers {
        let waivers: Vec<String> = t.waivers.iter().map(waiver_text).collect();
        match waivers.len() {
            0 => {}
            1 => put("waived", scalar(&waivers[0])),
            _ => put("waived", flow_list(waivers.iter().map(String::as_str))),
        }
        if let Some(h) = &t.hold {
            put("hold", scalar(&h.reason));
        }
        if let Some(p) = &t.parked {
            put("parked", scalar(&p.until));
        }
    }
    for (key, value) in &t.ext {
        if legacy_markers && key == PARKED_REASON {
            continue;
        }
        put(key, yaml_value(value));
    }

    let mut body = t.description.trim().to_string();
    if legacy_markers && !markers.is_empty() {
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(&markers.join("\n"));
    }
    if !comments.is_empty() {
        if !ends_with_comments_section(&body) {
            if !body.is_empty() {
                body.push_str("\n\n");
            }
            body.push_str("## Comments");
        }
        for c in &comments {
            body.push_str(&format!(
                "\n\n### {} — {}\n{}",
                date_from_ms(c.hlc.wall_ms),
                c.author,
                c.body.trim()
            ));
        }
    }
    let text = if body.is_empty() {
        format!("---\n{fm}---\n")
    } else {
        format!("---\n{fm}---\n\n{body}\n")
    };
    Ok(Rendered {
        path: ticket_path(ws, t)?,
        text,
    })
}

/// The prose form of every structured marker on `t`, one line each, in
/// the vault's idioms (`crate::import::prose` reads them back).
fn marker_lines(t: &Ticket, parked_reason: Option<&str>) -> Vec<String> {
    let mut lines: Vec<String> = t
        .waivers
        .iter()
        .map(|w| format!("waived: {}", waiver_text(w)))
        .collect();
    if let Some(h) = &t.hold {
        lines.push(format!("⚠ NEEDS-HUMAN: {}", h.reason));
    }
    if let Some(p) = &t.parked {
        let value = match parked_reason {
            Some(reason) if p.until == PARKED_FOREVER => reason.to_string(),
            _ => p.until.clone(),
        };
        lines.push(format!("parked: {value}"));
    }
    lines
}

/// `<rule> — <reason>`, or just the rule when the reason is the rule
/// (`parse_waiver` reads a dash-less value as both).
fn waiver_text(w: &pm_core::Waiver) -> String {
    if w.reason == w.rule {
        w.rule.clone()
    } else {
        format!("{} — {}", w.rule, w.reason)
    }
}

/// Whether the last `## ` heading of `body` is `## Comments` — a
/// template's comment preamble the import kept — so entries belong under
/// it rather than under a second heading.
fn ends_with_comments_section(body: &str) -> bool {
    body.lines()
        .rfind(|l| l.starts_with("## "))
        .is_some_and(|l| l.trim() == "## Comments")
}

fn priority_name(t: &Ticket) -> String {
    serde_json::to_value(t.priority)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn opt(value: &Option<String>) -> String {
    value.as_deref().map(scalar).unwrap_or_default()
}

// ----------------------------------------------------------------- yaml

/// A block scalar as the vault writes it: bare unless YAML would read it
/// as something else (a mapping, a comment, a number, a boolean, an
/// anchor…), then double-quoted.
pub(crate) fn scalar(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".into();
    }
    let first = s.chars().next().unwrap_or(' ');
    let needs_quotes = s != s.trim()
        || s.contains(": ")
        || s.contains(" #")
        || s.contains('\n')
        || s.contains('\t')
        || s.ends_with(':')
        || matches!(
            first,
            '"' | '\''
                | '['
                | ']'
                | '{'
                | '}'
                | '&'
                | '*'
                | '!'
                | '|'
                | '>'
                | '%'
                | '@'
                | '`'
                | '#'
                | ','
        )
        || s == "-"
        || s.starts_with("- ")
        || s.starts_with("? ")
        || s.starts_with("---")
        || looks_like_non_string(s);
    if needs_quotes {
        quoted(s)
    } else {
        s.to_string()
    }
}

/// A scalar inside a flow list or map, where `,`, `[`, `]`, `{` and `}`
/// also delimit.
fn flow_scalar(s: &str) -> String {
    if s.contains([',', '[', ']', '{', '}']) {
        quoted(s)
    } else {
        scalar(s)
    }
}

/// `true`, `null`, `1.5`, `2026-09-28`… — plain text a YAML reader would
/// type; quoted so it stays the string it is.
fn looks_like_non_string(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "true" | "false" | "null" | "~" | "yes" | "no" | "on" | "off" | ".inf" | "-.inf" | ".nan"
    ) || s.parse::<f64>().is_ok()
        || s.starts_with("0x")
        || s.starts_with("0o")
}

fn quoted(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn flow_list<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let items: Vec<String> = items.map(flow_scalar).collect();
    format!("[{}]", items.join(", "))
}

/// An `ext` value back to YAML: scalars bare where possible, lists and
/// maps in flow style (the only style the vault's frontmatter uses).
fn yaml_value(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => scalar(s),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(yaml_flow_value).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(map) => {
            let items: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", flow_scalar(k), yaml_flow_value(v)))
                .collect();
            format!("{{ {} }}", items.join(", "))
        }
    }
}

fn yaml_flow_value(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::String(s) => flow_scalar(s),
        other => yaml_value(other),
    }
}

// ------------------------------------------------------------- projects

/// A project's folder: the README (its design doc), then each named
/// document.
pub(crate) fn render_project(p: &Project) -> Result<Vec<Rendered>> {
    let id = segment(
        crate::ids::safe_component(&p.id, "project id")?,
        "project id",
    )?;
    // A parked project may come back (AGT-1635): not retired, so not
    // archived.
    let base = if p.status.is_retired() {
        Path::new("archive").join("projects").join(id)
    } else {
        Path::new("projects").join(id)
    };
    let readme = if p.doc.trim().is_empty() {
        // A project pm created without a doc (an import stub): a README
        // in the vault's own template shape, so the folder reads back.
        let status = serde_json::to_value(p.status)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        format!(
            "---\nid: {}\ntitle: {}\nstatus: {status}\nparent-project: {}\nrepos: {}\n---\n\n# {}\n",
            p.id,
            scalar(&p.title),
            opt(&p.parent),
            flow_list(p.repos.iter().map(String::as_str)),
            p.title
        )
    } else {
        p.doc.clone()
    };
    let mut out = vec![Rendered {
        path: base.join("README.md"),
        text: readme,
    }];
    for (name, text) in &p.documents {
        let name = segment(name, "document name")?;
        out.push(Rendered {
            path: base.join(format!("{name}.md")),
            text: text.clone(),
        });
    }
    Ok(out)
}

/// The tickets a `pm export md` run writes, for callers that want the
/// list without the files (`pm import vault --report` diffs each against
/// its source).
pub(crate) fn numbered(store: &Store) -> Result<Vec<Ticket>> {
    Ok(store
        .all_tickets()?
        .into_iter()
        .filter(|t| !t.deleted && t.number.is_some())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_follow_the_vault_filer() {
        assert_eq!(
            slug("pm import vault: lossless, incremental import (AGT-1347)"),
            "pm-import-vault-lossless-incremental-import-agt-13"
        );
        assert_eq!(slug("Approve the E2 copy"), "approve-the-e2-copy");
        assert_eq!(slug("— ⚠ —"), "");
        assert_eq!(slug("a".repeat(60).as_str()).len(), SLUG_MAX);
        // The cut never leaves a trailing dash.
        let s = slug(&format!("{} b", "a".repeat(49)));
        assert_eq!(s, "a".repeat(49));
    }

    #[test]
    fn data_derived_path_segments_stay_under_the_root() {
        for ok in [
            "triage",
            "in-progress",
            "pm",
            "ideation/IDEA-001-first-idea",
            "a.b",
        ] {
            assert_eq!(segment(ok, "x").unwrap(), ok);
        }
        for bad in [
            "",
            "..",
            "../../.ssh",
            "a/../b",
            "/etc",
            "./a",
            "a/",
            "a\0b",
        ] {
            let err = segment(bad, "state").unwrap_err();
            assert_eq!(err.code, crate::exit::ERROR, "{bad}");
            assert!(
                err.error.to_string().contains("not a safe path segment"),
                "{bad}"
            );
        }
    }

    #[test]
    fn scalars_are_bare_unless_yaml_would_misread_them() {
        assert_eq!(scalar("Approve the E2 copy"), "Approve the E2 copy");
        assert_eq!(scalar("deps: pass #62"), "\"deps: pass #62\"");
        assert_eq!(scalar("a #b"), "\"a #b\"");
        assert_eq!(scalar("model:sonnet-5"), "model:sonnet-5");
        assert_eq!(scalar("1234567"), "\"1234567\"");
        assert_eq!(scalar("true"), "\"true\"");
        assert_eq!(scalar("[x]"), "\"[x]\"");
        // A quote or backslash mid-value is plain YAML; a leading one is not.
        assert_eq!(scalar("say \"hi\" \\ there"), "say \"hi\" \\ there");
        assert_eq!(scalar("\"open"), "\"\\\"open\"");
        assert_eq!(scalar(""), "\"\"");
        assert_eq!(scalar("`pm` verb"), "\"`pm` verb\"");
        assert_eq!(flow_scalar("a, b"), "\"a, b\"");
        assert_eq!(
            yaml_value(&json!({"k": ["a", 1, null], "n": 2})),
            "{ k: [a, 1, null], n: 2 }"
        );
        assert_eq!(yaml_value(&Value::Null), "");
    }

    #[test]
    fn comment_entries_join_a_kept_comments_preamble() {
        assert!(ends_with_comments_section(
            "## Problem\n\nP.\n\n## Comments\n\n<!-- keep -->"
        ));
        assert!(!ends_with_comments_section(
            "## Comments\n\n<!-- keep -->\n\n## Outcome\n\ndone"
        ));
        assert!(!ends_with_comments_section(""));
    }

    #[test]
    fn marker_lines_use_the_vaults_idioms() {
        let mut t = pm_core::Ticket {
            id: ulid::Ulid::new(),
            number: Some(1),
            title: "t".into(),
            state: "triage".into(),
            priority: pm_core::Priority::Medium,
            project: None,
            repo: None,
            assignee: None,
            description: String::new(),
            labels: Default::default(),
            created: pm_core::Hlc::ZERO,
            updated: pm_core::Hlc::ZERO,
            archived_at: None,
            deleted: false,
            linked_github: None,
            linked_pr: None,
            linear: None,
            source: None,
            hold: None,
            waivers: vec![
                pm_core::Waiver {
                    rule: "standalone".into(),
                    reason: "one ticket".into(),
                },
                pm_core::Waiver {
                    rule: "operator decision".into(),
                    reason: "operator decision".into(),
                },
            ],
            not_before: None,
            parked: Some(pm_core::Parked {
                until: PARKED_FOREVER.into(),
            }),
            ext: Default::default(),
        };
        assert_eq!(
            marker_lines(&t, Some("until AGT-1236 is done.")),
            [
                "waived: standalone — one ticket",
                "waived: operator decision",
                "parked: until AGT-1236 is done."
            ]
        );
        t.parked = Some(pm_core::Parked {
            until: "2026-11-01".into(),
        });
        t.hold = Some(pm_core::Hold {
            reason: "Matt signs off".into(),
            by: pm_core::ActorId::new("import"),
            at: pm_core::Hlc::ZERO,
        });
        assert_eq!(
            marker_lines(&t, None)[2..],
            ["⚠ NEEDS-HUMAN: Matt signs off", "parked: 2026-11-01"]
        );
    }
}
