//! The import parity report (AGT-1348 AC2, AC3): `pm import vault
//! --report <file>` proves the round trip. For every source ticket file,
//! the imported ticket is rendered back the way `pm export md
//! --legacy-markers` writes it, both texts are parsed with the importer's
//! own parser ([`super::vault::parse_ticket`]), and the two parses are
//! compared field by field. Parsing is the normaliser: key order,
//! quoting, whitespace and list spacing never reach the comparison.
//!
//! What remains is classified. A difference the round trip *explains* —
//! the source's `updated` carrying a time of day, or being stale against
//! its own last comment; a duplicate id renumbered; comment entries
//! re-sorted by date — is counted under its class, with the class's
//! rationale in the report header. Anything else is **unexplained**, and
//! listed with both values; the report is the P1 gate's evidence only when
//! that list is empty. Textual differences that the normaliser removes
//! (quoting, key order…) are counted too, so the header shows what the
//! normalisation actually absorbed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use pm_core::Ticket;
use pm_core::markers::date_from_ms;
use pm_store::Store;
use serde::Serialize;
use serde_json::Value;

use super::plan::DUPLICATE_OF;
use super::vault::{self, Findings, Snapshot, VaultTicket};
use crate::batch;
use crate::exit::Result;
use crate::export::{self, PARKED_REASON};

/// The `parity` block of the import report's `--json`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// Where the markdown report was written.
    pub report: String,
    /// Source ticket files compared against an exported rendering.
    pub compared: usize,
    /// Source files with no ticket in pm.
    pub missing: usize,
    pub unexplained: usize,
    /// Explained class → number of tickets it applies to.
    pub explained: BTreeMap<String, usize>,
    pub anomalies: usize,
}

pub struct Parity {
    pub summary: Summary,
    pub markdown: String,
}

/// One explained difference class: its name and why it is not a loss.
struct Class {
    name: &'static str,
    why: &'static str,
}

const CLASSES: &[Class] = &[
    Class {
        name: "key order",
        why: "The export writes frontmatter keys in the template's order; the vault has 27 orders. Parsed values are compared, so order is normalised.",
    },
    Class {
        name: "quoting",
        why: "A value quoted in one file and bare in the other (the vault quotes 646 titles and 7 `linked-pr` values; the export quotes only what YAML needs). Both parse to the same string.",
    },
    Class {
        name: "whitespace",
        why: "A frontmatter line differing only in whitespace (`project: ` vs `project:`, spacing inside a flow list).",
    },
    Class {
        name: "list order",
        why: "A flow list with the same members in another order: labels are a set, and `blocked-by` is written in number order.",
    },
    Class {
        name: "frontmatter marker → prose",
        why: "A `waived:` frontmatter key (AGT-1093) is the same marker as a prose `waived:` line; `--legacy-markers` writes the prose form, which parses to the same waiver.",
    },
    Class {
        name: "body text (parses equal)",
        why: "The body's text differs — blank-line runs collapsed, migrated marker lines re-placed at the end of the description, comment entries re-rendered — but the description and every comment parse identically.",
    },
    Class {
        name: "updated: ISO datetime",
        why: "The source `updated` carries a time of day; pm keeps the instant and the export writes the template's `YYYY-MM-DD`. The date is the same.",
    },
    Class {
        name: "updated: stale (predates last comment)",
        why: "The source `updated` is earlier than the ticket's own last comment. pm's `updated` is the ticket's last op, comments included — the vault's own convention (1,004 of its 1,346 commented files bump `updated` to the last comment's date). The comment's date is exported with the comment; nothing is lost.",
    },
    Class {
        name: "updated: re-imported change (stamped at import time)",
        why: "This run re-imported a change to a ticket the store already had. Such a write is stamped after the log's newest op so the file's value wins (README §Conflict semantics), and pm's `updated` is when the change landed in pm, not the file's `updated`. Never appears on an import into a fresh workspace.",
    },
    Class {
        name: "updated: before created",
        why: "The source `updated` precedes its `created`; a ticket's record ops cannot run backwards, so the update lands at `created`.",
    },
    Class {
        name: "renumbered duplicate",
        why: "A second file carrying an id already taken (AGT-846) was imported with a fresh number and `ext.duplicate_of_number` naming the id it duplicated; its `id`, and the date of the number op, differ by construction.",
    },
    Class {
        name: "comments: reordered by date",
        why: "Entries left the file out of date order; pm orders comments by date, and same-date entries keep file order. Every entry's date, author and text are identical.",
    },
    Class {
        name: "comments: empty entry dropped",
        why: "A `### <date> — <author>` heading with no text under it has nothing to import: a pm comment cannot be empty.",
    },
    Class {
        name: "filename: slugless",
        why: "The source file is `AGT-N.md` with no slug; the export names the file from the title. The id, not the path, keys the comparison.",
    },
    Class {
        name: "filename: slug differs",
        why: "The source filename's slug was cut from the title at filing time and the title changed since; pm keeps no filename. The id keys the comparison.",
    },
];

/// One ticket's outcome.
#[derive(Default)]
struct Outcome {
    id: String,
    source_path: PathBuf,
    export_path: PathBuf,
    classes: BTreeSet<&'static str>,
    /// Explained differences with their values, for the per-ticket list.
    explained: Vec<String>,
    unexplained: Vec<String>,
}

impl Outcome {
    fn class(&mut self, name: &'static str) {
        self.classes.insert(name);
    }

    fn explain(&mut self, name: &'static str, detail: String) {
        self.classes.insert(name);
        self.explained.push(format!("{detail} [{name}]"));
    }

    fn lose(&mut self, field: &str, source: &str, export: &str) {
        self.unexplained
            .push(format!("{field}: {} → {}", short(source), short(export)));
    }
}

/// Runs the comparison over every ticket in `snapshot` (the vault as the
/// import read it) against the store, and writes nothing: the caller
/// writes [`Parity::markdown`] where `--report` says.
pub fn run(
    store: &Store,
    ws: &pm_core::Workspace,
    snapshot: &Snapshot,
    import_anomalies: &[String],
    changed: &[String],
    non_template: &BTreeMap<String, Vec<String>>,
    report_path: &Path,
) -> Result<Parity> {
    // `AGT-N: title, state` → the ids this run changed.
    let changed: BTreeSet<&str> = changed
        .iter()
        .filter_map(|c| c.split_once(':').map(|(id, _)| id))
        .collect();
    let all: Vec<Ticket> = export::numbered(store)?;
    let duplicates: Vec<&Ticket> = all
        .iter()
        .filter(|t| t.ext.contains_key(DUPLICATE_OF))
        .collect();

    // Which file holds each number (the earliest-created keeps it; later
    // ones were renumbered — `plan::build`).
    let mut groups: BTreeMap<u64, Vec<&VaultTicket>> = BTreeMap::new();
    for vt in &snapshot.tickets {
        groups.entry(vt.number).or_default().push(vt);
    }
    for files in groups.values_mut() {
        files.sort_by(|a, b| (a.created_ms, &a.path).cmp(&(b.created_ms, &b.path)));
    }

    let mut outcomes: Vec<Outcome> = Vec::with_capacity(snapshot.tickets.len());
    let mut missing = 0usize;
    for (number, files) in &groups {
        let id = format!("{}-{number}", ws.prefix);
        for (i, vt) in files.iter().enumerate() {
            let is_dup = i > 0;
            let ticket = if is_dup {
                duplicates
                    .iter()
                    .find(|t| {
                        t.ext.get(DUPLICATE_OF) == Some(&Value::String(id.clone()))
                            && t.title == vt.title
                    })
                    .map(|t| (*t).clone())
            } else {
                all.iter().find(|t| t.number == Some(*number)).cloned()
            };
            let mut o = Outcome {
                id: id.clone(),
                source_path: vt.path.clone(),
                ..Outcome::default()
            };
            let Some(t) = ticket else {
                missing += 1;
                o.unexplained.push("missing: no ticket in pm".into());
                outcomes.push(o);
                continue;
            };
            let rendered = export::render_ticket(ws, store, &t, true)?;
            let mut findings = Findings::default();
            let ex = vault::parse_ticket(&rendered.text, None, &rendered.path, ws, &mut findings)?;
            o.export_path = rendered.path.clone();
            let source_text = fs::read_to_string(snapshot.root.join(&vt.path))
                .with_context(|| format!("reading {}", vt.path.display()))?;
            compare(
                &mut o,
                vt,
                &ex,
                &source_text,
                &rendered.text,
                is_dup,
                changed.contains(id.as_str()),
            );
            outcomes.push(o);
        }
    }

    let mut explained: BTreeMap<String, usize> = BTreeMap::new();
    for o in &outcomes {
        for c in &o.classes {
            *explained.entry((*c).to_string()).or_default() += 1;
        }
    }
    let unexplained: usize = outcomes.iter().map(|o| o.unexplained.len()).sum();

    // Anomalies: the import's own, then the outcome of every ticket they
    // name, then the slugless files.
    let mut anomalies: Vec<String> = import_anomalies.to_vec();
    let named: BTreeSet<String> = import_anomalies
        .iter()
        .flat_map(|a| ids_in(a, &ws.prefix))
        .collect();
    for o in &outcomes {
        if !named.contains(&o.id) && !o.classes.contains("renumbered duplicate") {
            continue;
        }
        anomalies.push(format!(
            "{} ({}): exported as {}; {} unexplained diff(s){}",
            o.id,
            o.source_path.display(),
            o.export_path.display(),
            o.unexplained.len(),
            if o.explained.is_empty() {
                String::new()
            } else {
                format!("; explained: {}", o.explained.join("; "))
            }
        ));
    }
    for o in &outcomes {
        if o.classes.contains("filename: slugless") {
            anomalies.push(format!(
                "{} ({}): slugless filename; exported as {}; {} unexplained diff(s)",
                o.id,
                o.source_path.display(),
                o.export_path.display(),
                o.unexplained.len()
            ));
        }
    }

    let summary = Summary {
        report: report_path.display().to_string(),
        compared: outcomes.len(),
        missing,
        unexplained,
        explained,
        anomalies: anomalies.len(),
    };
    let markdown = markdown(&summary, snapshot, &outcomes, &anomalies, non_template);
    Ok(Parity { summary, markdown })
}

/// `AGT-806`-shaped ids mentioned in an anomaly line.
fn ids_in(text: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = format!("{prefix}-");
    let mut rest = text;
    while let Some(pos) = rest.find(&needle) {
        let after = &rest[pos + needle.len()..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() {
            out.push(format!("{prefix}-{digits}"));
        }
        rest = &after[digits.len()..];
    }
    out
}

// -------------------------------------------------------------- compare

fn compare(
    o: &mut Outcome,
    src: &VaultTicket,
    ex: &VaultTicket,
    source_text: &str,
    export_text: &str,
    is_dup: bool,
    changed_this_run: bool,
) {
    // ---- textual: what the normaliser absorbs ----
    let src_fm = fm_lines(source_text);
    let ex_fm = fm_lines(export_text);
    let src_keys: Vec<&str> = src_fm.iter().map(|(k, _)| k.as_str()).collect();
    let ex_keys: Vec<&str> = ex_fm.iter().map(|(k, _)| k.as_str()).collect();
    let common: Vec<&str> = src_keys
        .iter()
        .copied()
        .filter(|k| ex_keys.contains(k))
        .collect();
    let ex_common: Vec<&str> = ex_keys
        .iter()
        .copied()
        .filter(|k| src_keys.contains(k))
        .collect();
    if common != ex_common {
        o.class("key order");
    }
    for key in src_keys.iter().filter(|k| !ex_keys.contains(k)) {
        if *key == "waived" {
            o.class("frontmatter marker → prose");
        }
        // Any other key the export lacks shows up as an `ext` difference.
    }
    let src_values: BTreeMap<&str, &str> = src_fm
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let ex_values: BTreeMap<&str, &str> = ex_fm
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    for key in &common {
        let (a, b) = (src_values[key], ex_values[key]);
        if a == b {
            continue;
        }
        if unquote(a) == unquote(b) {
            o.class("quoting");
        } else if squash(a) == squash(b) {
            o.class("whitespace");
        } else if let (Some(mut la), Some(mut lb)) = (flow_items(a), flow_items(b)) {
            la.sort();
            lb.sort();
            if la == lb {
                o.class("list order");
            }
        }
        // Anything else is decided by the parsed comparison below.
    }
    let src_body = batch::split_frontmatter(source_text)
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    let ex_body = batch::split_frontmatter(export_text)
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    let body_text_differs = src_body != ex_body;

    // ---- parsed: what matters ----
    if src.number != ex.number {
        if is_dup && ex.ext.get(DUPLICATE_OF) == Some(&Value::String(o.id.clone())) {
            o.explain(
                "renumbered duplicate",
                format!(
                    "id: {} → {}-{}",
                    o.id,
                    o.id.rsplit_once('-').map_or("", |p| p.0),
                    ex.number
                ),
            );
        } else {
            o.lose("id", &src.number.to_string(), &ex.number.to_string());
        }
    }
    let scalar_fields: [(&str, String, String); 9] = [
        ("title", src.title.clone(), ex.title.clone()),
        ("state", src.state.clone(), ex.state.clone()),
        ("project", fmt_opt(&src.project), fmt_opt(&ex.project)),
        ("repo", fmt_opt(&src.repo), fmt_opt(&ex.repo)),
        (
            "linked-github",
            fmt_opt(&src.linked_github),
            fmt_opt(&ex.linked_github),
        ),
        ("linked-pr", fmt_opt(&src.linked_pr), fmt_opt(&ex.linked_pr)),
        (
            "priority",
            format!("{:?}", src.priority),
            format!("{:?}", ex.priority),
        ),
        (
            "source",
            format!("{:?}", src.source),
            format!("{:?}", ex.source),
        ),
        (
            "created",
            date_from_ms(src.created_ms),
            date_from_ms(ex.created_ms),
        ),
    ];
    for (field, a, b) in &scalar_fields {
        if a != b {
            o.lose(field, a, b);
        }
    }
    if src.labels != ex.labels {
        o.lose(
            "labels",
            &format!("{:?}", src.labels),
            &format!("{:?}", ex.labels),
        );
    }
    let mut sb = src.blocked_by.clone();
    let mut eb = ex.blocked_by.clone();
    sb.sort();
    eb.sort();
    if sb != eb {
        o.lose("blocked-by", &format!("{sb:?}"), &format!("{eb:?}"));
    }
    let strip = |ext: &BTreeMap<String, Value>| -> BTreeMap<String, Value> {
        ext.iter()
            .filter(|(k, _)| k.as_str() != DUPLICATE_OF && k.as_str() != PARKED_REASON)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    let (se, ee) = (strip(&src.ext), strip(&ex.ext));
    if se != ee {
        for key in se.keys().chain(ee.keys()).collect::<BTreeSet<_>>() {
            if se.get(key) != ee.get(key) {
                o.lose(
                    &format!("ext.{key}"),
                    &se.get(key).map_or("(absent)".to_string(), Value::to_string),
                    &ee.get(key).map_or("(absent)".to_string(), Value::to_string),
                );
            }
        }
    }
    if src.waivers != ex.waivers {
        o.lose(
            "waivers",
            &format!("{:?}", src.waivers),
            &format!("{:?}", ex.waivers),
        );
    }
    if src.hold != ex.hold {
        o.lose("hold", &fmt_opt(&src.hold), &fmt_opt(&ex.hold));
    }
    if src.parked != ex.parked {
        o.lose(
            "parked",
            &format!("{:?}", src.parked),
            &format!("{:?}", ex.parked),
        );
    }
    if src.archived_month_ms != ex.archived_month_ms {
        o.lose(
            "archive month",
            &src.archived_month_ms.map_or("none".into(), date_from_ms),
            &ex.archived_month_ms.map_or("none".into(), date_from_ms),
        );
    }
    if src.description != ex.description {
        o.lose("description", &src.description, &ex.description);
    }

    // Comments: entries with text, compared in order, then as a
    // date-sorted sequence.
    let dropped = src.comments.iter().filter(|c| c.body.is_empty()).count();
    if dropped > 0 {
        o.explain(
            "comments: empty entry dropped",
            format!(
                "comments: {dropped} empty entr{}",
                if dropped == 1 { "y" } else { "ies" }
            ),
        );
    }
    let entry =
        |c: &vault::VaultComment| (date_from_ms(c.date_ms), c.author.clone(), c.body.clone());
    let src_entries: Vec<_> = src
        .comments
        .iter()
        .filter(|c| !c.body.is_empty())
        .map(entry)
        .collect();
    let ex_entries: Vec<_> = ex.comments.iter().map(entry).collect();
    if src_entries != ex_entries {
        let mut sorted = src_entries.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        if sorted == ex_entries {
            o.explain(
                "comments: reordered by date",
                format!("comments: {} entries re-sorted by date", src_entries.len()),
            );
        } else {
            let at = src_entries
                .iter()
                .zip(&ex_entries)
                .position(|(a, b)| a != b)
                .unwrap_or(src_entries.len().min(ex_entries.len()));
            let show = |e: Option<&(String, String, String)>| match e {
                Some((d, a, b)) => format!("### {d} — {a}: {}", short(b)),
                None => "(none)".into(),
            };
            o.lose(
                &format!(
                    "comments[{at}] ({} source entries, {} exported)",
                    src_entries.len(),
                    ex_entries.len()
                ),
                &show(src_entries.get(at)),
                &show(ex_entries.get(at)),
            );
        }
    }

    // `updated`: the one field pm derives rather than stores.
    if src.updated_ms != ex.updated_ms {
        let (a, b) = (date_from_ms(src.updated_ms), date_from_ms(ex.updated_ms));
        let raw = src_values.get("updated").copied().unwrap_or_default();
        let last_comment = src.comments.iter().map(|c| c.date_ms).max().unwrap_or(0);
        if is_dup {
            o.explain(
                "renumbered duplicate",
                format!("updated: {a} → {b} (the fresh number's op is dated at import time)"),
            );
        } else if a == b && raw.contains('T') {
            o.explain("updated: ISO datetime", format!("updated: {raw} → {b}"));
        } else if changed_this_run && ex.updated_ms > src.updated_ms {
            o.explain(
                "updated: re-imported change (stamped at import time)",
                format!("updated: {a} → {b}"),
            );
        } else if src.updated_ms < src.created_ms {
            o.explain(
                "updated: before created",
                format!(
                    "updated: {a} → {b} (created {})",
                    date_from_ms(src.created_ms)
                ),
            );
        } else if ex.updated_ms > src.updated_ms && last_comment > src.updated_ms {
            o.explain(
                "updated: stale (predates last comment)",
                format!(
                    "updated: {a} → {b} (last comment {})",
                    date_from_ms(last_comment)
                ),
            );
        } else {
            o.lose("updated", &a, &b);
        }
    }

    if body_text_differs && o.unexplained.is_empty() {
        o.class("body text (parses equal)");
    }

    // Filenames: not a field, but the vault's readers key on them.
    let stem = |p: &Path| {
        p.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string()
    };
    let (ss, es) = (stem(&o.source_path), stem(&o.export_path));
    if ss == o.id {
        o.class("filename: slugless");
    } else if ss != es {
        o.class("filename: slug differs");
    }
}

/// Top-level `key: value` lines of a file's frontmatter, in order,
/// values trimmed.
fn fm_lines(text: &str) -> Vec<(String, String)> {
    let Ok((fm, _)) = batch::split_frontmatter(text) else {
        return Vec::new();
    };
    fm.lines()
        .filter_map(vault::top_level_key)
        .map(|(k, v)| (k.to_string(), v.trim().to_string()))
        .collect()
}

/// A double- or single-quoted scalar's text, escapes resolved; anything
/// else unchanged.
fn unquote(v: &str) -> String {
    let v = v.trim();
    for q in ['"', '\''] {
        if v.len() >= 2 && v.starts_with(q) && v.ends_with(q) {
            let inner = &v[1..v.len() - 1];
            return if q == '"' {
                inner.replace("\\\"", "\"").replace("\\\\", "\\")
            } else {
                inner.replace("''", "'")
            };
        }
    }
    v.to_string()
}

/// The value with every whitespace character removed — enough to tell
/// a spacing difference from a real one, since the parsed comparison
/// decides what is real.
fn squash(v: &str) -> String {
    v.chars().filter(|c| !c.is_whitespace()).collect()
}

/// `[a, b]` → `["a", "b"]`; `None` for anything that is not a flow list.
fn flow_items(v: &str) -> Option<Vec<String>> {
    let inner = v.trim().strip_prefix('[')?.strip_suffix(']')?;
    Some(
        inner
            .split(',')
            .map(|s| unquote(s.trim()))
            .filter(|s| !s.is_empty())
            .collect(),
    )
}

fn fmt_opt(v: &Option<String>) -> String {
    v.clone().unwrap_or_else(|| "(none)".into())
}

/// The first line of `s`, cut to a readable width, control characters
/// dropped (the report is markdown; a comment body is untrusted text).
fn short(s: &str) -> String {
    let line = s.lines().next().unwrap_or_default();
    let chars: Vec<char> = line.chars().filter(|c| !c.is_control()).collect();
    let cut = chars.len() > 100 || s.lines().count() > 1;
    let mut out: String = chars.into_iter().take(100).collect();
    if cut {
        out.push('…');
    }
    format!("`{out}`")
}

// ------------------------------------------------------------- markdown

fn markdown(
    summary: &Summary,
    snapshot: &Snapshot,
    outcomes: &[Outcome],
    anomalies: &[String],
    non_template: &BTreeMap<String, Vec<String>>,
) -> String {
    let today = date_from_ms(crate::verbs::now_ms());
    let sha = git_head(&snapshot.root).unwrap_or_else(|| "unknown".into());
    let archived = snapshot
        .tickets
        .iter()
        .filter(|t| t.archived_month_ms.is_some())
        .count();
    let mut md = String::new();
    md.push_str(&format!("# Import parity report — {today}\n\n"));
    md.push_str(&format!(
        "- vault: `{}` at `{sha}` — {} ticket files ({archived} under `archive/`), {} projects\n",
        snapshot.root.display(),
        snapshot.tickets.len(),
        snapshot.projects.len()
    ));
    md.push_str(&format!(
        "- pm {}: `pm import vault --report`, comparing against `pm export md --legacy-markers`\n",
        env!("CARGO_PKG_VERSION")
    ));
    md.push_str(&format!(
        "- tickets compared: {} ({} missing in pm)\n",
        summary.compared, summary.missing
    ));
    md.push_str(&format!(
        "- **unexplained diffs: {}**\n\n",
        summary.unexplained
    ));

    md.push_str("## Method\n\n");
    md.push_str(
        "Every source ticket file is parsed with the importer's parser. The imported ticket is rendered back as `pm export md --legacy-markers` writes it (template key order; `waived:`/`⚠ NEEDS-HUMAN:`/`parked:` markers as their original prose lines) and parsed the same way. The two parses are compared field by field: title, state, created, updated, project, repo, blocked-by, linked-github, linked-pr, priority, labels, source, every `ext` key, the description, every comment entry (date, author, text), waivers, hold, parked, and the archive month. Parsing is the normaliser — key order, quoting, whitespace and list spacing never reach the comparison — and the textual differences it absorbed are counted below so the normalisation is visible. Tickets are keyed by id, never by path.\n\n",
    );

    md.push_str("## Explained diff classes\n\n");
    md.push_str("| Class | Tickets | Why it is not a loss |\n|---|---|---|\n");
    for c in CLASSES {
        let n = summary.explained.get(c.name).copied().unwrap_or(0);
        md.push_str(&format!("| {} | {n} | {} |\n", c.name, c.why));
    }
    md.push('\n');
    md.push_str("Non-template values the import reported (`non_template`), all of which round-trip verbatim and produce no diff:\n\n");
    for (category, at) in non_template {
        md.push_str(&format!("- {category}: {} file(s)\n", at.len()));
    }
    md.push('\n');

    md.push_str(&format!("## Anomalies ({})\n\n", anomalies.len()));
    for a in anomalies {
        md.push_str(&format!("- {a}\n"));
    }
    md.push('\n');

    md.push_str(&format!(
        "## Unexplained diffs ({})\n\n",
        summary.unexplained
    ));
    let mut any = false;
    for o in outcomes.iter().filter(|o| !o.unexplained.is_empty()) {
        any = true;
        md.push_str(&format!(
            "- **{}** (`{}` → `{}`)\n",
            o.id,
            o.source_path.display(),
            o.export_path.display()
        ));
        for line in &o.unexplained {
            md.push_str(&format!("  - {line}\n"));
        }
    }
    if !any {
        md.push_str("None.\n");
    }
    md.push('\n');

    let with_values: Vec<&Outcome> = outcomes
        .iter()
        .filter(|o| !o.explained.is_empty())
        .collect();
    md.push_str(&format!(
        "## Tickets with explained value diffs ({})\n\n",
        with_values.len()
    ));
    for o in with_values {
        md.push_str(&format!(
            "- {} (`{}`): {}\n",
            o.id,
            o.source_path.display(),
            o.explained.join("; ")
        ));
    }
    md
}

fn git_head(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textual_normalisers() {
        assert_eq!(unquote("\"deps: pass \\\"x\\\"\""), "deps: pass \"x\"");
        assert_eq!(unquote("'it''s'"), "it's");
        assert_eq!(unquote("bare"), "bare");
        assert_eq!(squash("[ a ,  b ]"), squash("[a, b]"));
        assert_ne!(squash("[a, b]"), squash("[a, c]"));
        assert_eq!(
            flow_items("[AGT-2, \"AGT-1\"]"),
            Some(vec!["AGT-2".into(), "AGT-1".into()])
        );
        assert_eq!(flow_items("[]"), Some(vec![]));
        assert_eq!(flow_items("plain"), None);
        assert_eq!(
            fm_lines("---\nid: AGT-1\ntitle: \"T\"\nsource: { type: manual }\n---\nbody"),
            [
                ("id".to_string(), "AGT-1".to_string()),
                ("title".into(), "\"T\"".into()),
                ("source".into(), "{ type: manual }".into())
            ]
        );
        assert_eq!(
            ids_in("x AGT-806 also AGT-12; AGT-", "AGT"),
            ["AGT-806", "AGT-12"]
        );
        assert_eq!(short("one\ntwo"), "`one…`");
        assert_eq!(short("a\u{7}b\tc"), "`abc`");
    }
}
