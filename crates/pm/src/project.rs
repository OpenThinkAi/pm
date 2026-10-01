//! `pm project new/show/list/edit/set/doc/delete` (AGT-1344,
//! projects/pm/README.md §CLI verbs: "`pm project new/show/list/edit` —
//! project README template, `view-projects --json`").
//!
//! Project *metadata* (title/status/parent/repos) is config ops since
//! AGT-1385 (`project.create` + `project.set`, committed by
//! `pm_store::Store::create_project` against the project's own Ulid); a
//! document's *body* — the design doc `pm project edit` opens, and any
//! named document `pm project doc add` creates — is a `body.edit` op
//! against that document's own `doc_id`, the same op kind and
//! [`pm_core::Body`] CRDT a ticket's description uses (AGT-1338): there is
//! exactly one body format in the op log, ever. `pm project delete` commits a
//! `project.delete` tombstone (AGT-1386) whose materialization removes the
//! row and its documents; the log keeps every op.
//!
//! `pm project edit` opens the design doc the same way `pm edit` opens a
//! ticket (`crate::edit::{run_editor, TempFile}`, AGT-1345): `$VISUAL`,
//! then `$EDITOR`, then `vi`, launched through `sh -c`. pm itself never
//! prompts (README §Constraints: "No command may prompt when stdin is not
//! a TTY") — a non-interactive `vi` just fails fast, which `run_editor`
//! reports as a non-zero exit, an abort. Tests exercise it by pointing
//! `$EDITOR` at a script that rewrites the file non-interactively.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::Context;
use clap::Subcommand;
use pm_core::op::BodyEdit;
use pm_core::{ActorId, Body, Payload, Project, ProjectStatus};
use pm_store::Store;
use serde_json::{Map, Value, json};
use ulid::Ulid;

use crate::edit;
use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, SCHEMA, Stamper, non_empty, print_json};

#[derive(Subcommand, Debug)]
pub enum ProjectCmd {
    /// Create a project
    New {
        /// Kebab-case id, e.g. pm-project-verbs
        id: String,
        #[arg(long)]
        title: String,
        /// Repo this project ships in; repeat or comma-separate for several
        #[arg(long = "repo", value_name = "OWNER/NAME", value_delimiter = ',')]
        repos: Vec<String>,
        /// An existing project this one is a sub-project of
        #[arg(long)]
        parent: Option<String>,
    },
    /// Print a project, or one of its documents with --doc
    Show {
        id: String,
        /// Print this named document's body instead of the project itself
        #[arg(long, value_name = "NAME")]
        doc: Option<String>,
    },
    /// List every project
    List {
        /// in-progress, complete or abandoned
        #[arg(long, value_parser = parse_status)]
        status: Option<ProjectStatus>,
    },
    /// Open the project view in ui-leaf (design doc + named docs in the CRDT editor, its tickets), or the design doc in $EDITOR; every change becomes body.edit ops
    Edit {
        id: String,
        /// ui-leaf or editor ($EDITOR); default: config `edit.view`, else ui-leaf at a terminal (falling back to $EDITOR)
        #[arg(long, value_parser = edit::parse_view)]
        view: Option<edit::View>,
        /// Non-interactive: replace the document body with this file's text (`-` = stdin) instead of opening any editor; the BODY only, since a design doc has no frontmatter. Agents use this, never a scripted $EDITOR
        #[arg(long = "from-file", value_name = "PATH|-", conflicts_with = "view")]
        from_file: Option<String>,
        /// With --from-file: write this named document instead of the design doc
        #[arg(long, value_name = "NAME", requires = "from_file")]
        doc: Option<String>,
    },
    /// Change a project's title, status or parent: title=… status=in-progress|complete|abandoned parent=<id|->
    Set {
        id: String,
        /// key=value pairs (keys: title, status, parent; parent=- clears it)
        #[arg(required = true, value_name = "KEY=VALUE")]
        assignments: Vec<String>,
    },
    /// Refuses while the project still has tickets or child projects (FK)
    Delete { id: String },
    /// Named documents beyond the design doc
    Doc {
        #[command(subcommand)]
        cmd: ProjectDocCmd,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProjectDocCmd {
    /// Create a named document from a file's contents
    Add {
        id: String,
        name: String,
        #[arg(long = "from-file", value_name = "PATH")]
        from_file: PathBuf,
    },
}

pub fn run(ctx: &Ctx<'_>, cmd: ProjectCmd) -> Result<()> {
    match cmd {
        ProjectCmd::New {
            id,
            title,
            repos,
            parent,
        } => new(ctx, &id, &title, &repos, parent.as_deref()),
        ProjectCmd::Show { id, doc } => show(ctx, &id, doc.as_deref()),
        ProjectCmd::List { status } => list(ctx, status),
        ProjectCmd::Edit {
            id,
            from_file: Some(src),
            doc,
            ..
        } => edit_from_file(ctx, &id, doc.as_deref(), &src),
        ProjectCmd::Edit { id, view, .. } => edit(ctx, &id, view),
        ProjectCmd::Set { id, assignments } => set(ctx, &id, &assignments),
        ProjectCmd::Delete { id } => delete(ctx, &id),
        ProjectCmd::Doc { cmd } => match cmd {
            ProjectDocCmd::Add {
                id,
                name,
                from_file,
            } => doc_add(ctx, &id, &name, &from_file),
        },
    }
}

/// Clap value parser for `--status`.
fn parse_status(s: &str) -> std::result::Result<ProjectStatus, String> {
    serde_json::from_value(Value::String(s.to_string())).map_err(|_| {
        format!("unknown status '{s}': expected one of in-progress, complete, abandoned")
    })
}

fn not_found(id: &str) -> CliError {
    CliError::not_found(format!("no project '{id}'"))
}

// -------------------------------------------------------------------- new

fn new(ctx: &Ctx<'_>, id: &str, title: &str, repos: &[String], parent: Option<&str>) -> Result<()> {
    crate::ids::validate_project_id(id)?;
    let title = non_empty("--title", title)?;
    let repos: BTreeSet<String> = repos
        .iter()
        .map(|r| non_empty("--repo", r))
        .collect::<Result<_>>()?;

    let actor = ctx.actor()?;
    let (mut store, _ws) = ctx.open()?;
    store.create_project(id, &title, &repos, parent, &actor)?;
    let project = store
        .project(id)?
        .ok_or_else(|| CliError::error(format!("project '{id}' vanished after create")))?;
    print_project(ctx, &project)
}

// ------------------------------------------------------------------- show

fn show(ctx: &Ctx<'_>, id: &str, doc: Option<&str>) -> Result<()> {
    let (store, _ws) = ctx.open()?;
    let project = store.project(id)?.ok_or_else(|| not_found(id))?;
    match doc {
        Some(name) => {
            let body = project.documents.get(name).ok_or_else(|| {
                CliError::not_found(format!("project '{id}' has no document '{name}'"))
            })?;
            if ctx.json {
                print_json(&json!({"schema": SCHEMA, "project": id, "doc": name, "body": body}));
            } else {
                // stderr, so the body on stdout stays pipeable.
                eprintln!("{id}: named doc '{}'", crate::text::inline(name));
                print!("{}", crate::text::printable(body));
                if !body.ends_with('\n') {
                    println!();
                }
            }
            Ok(())
        }
        None => print_project(ctx, &project),
    }
}

// ------------------------------------------------------------------- list

fn list(ctx: &Ctx<'_>, status: Option<ProjectStatus>) -> Result<()> {
    let (store, _ws) = ctx.open()?;
    let mut projects = store.projects()?;
    if let Some(status) = status {
        projects.retain(|p| p.status == status);
    }
    if ctx.json {
        let out: Vec<Value> = projects.iter().map(project_json).collect();
        print_json(&json!({"schema": SCHEMA, "projects": out}));
    } else if projects.is_empty() {
        eprintln!("no projects found");
    } else {
        for p in &projects {
            println!(
                "{:<24} {:<12} {}",
                crate::text::inline(&p.id),
                status_str(p.status),
                crate::text::inline(&p.title)
            );
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- edit

/// `pm project edit <id> [--view=editor|ui-leaf]` (AGT-1405): the view is
/// chosen exactly as `pm edit` chooses (`crate::edit::resolve_view`, and
/// the default ui-leaf only at a terminal); ui-leaf opens the project view,
/// anything else — or a ui-leaf that cannot open — is the `$EDITOR` flow
/// on the design doc below.
fn edit(ctx: &Ctx<'_>, id: &str, view_flag: Option<edit::View>) -> Result<()> {
    let (view, explicit) = edit::resolve_view(view_flag, ctx.env)?;
    if view == edit::View::UiLeaf && edit_in_ui_leaf(ctx, id, explicit)? {
        return Ok(());
    }
    edit_in_editor(ctx, id)
}

/// The ui-leaf path of `pm project edit`: `Ok(true)` when the project view
/// opened and has closed, `Ok(false)` to continue with `$EDITOR`.
fn edit_in_ui_leaf(ctx: &Ctx<'_>, id: &str, explicit: bool) -> Result<bool> {
    let Some(runtime) = edit::ui_leaf_runtime(ctx, explicit, "pm project edit")? else {
        return Ok(false);
    };
    // Resolve the project first: an unknown id is exit 3 before any window.
    {
        let (store, _ws) = ctx.open()?;
        store.project(id)?.ok_or_else(|| not_found(id))?;
    }
    match crate::app::edit_project(ctx, runtime, id)? {
        crate::app::launch::Ended::Closed => {}
        crate::app::launch::Ended::Failed(why) => {
            eprintln!(
                "pm project edit: ui-leaf could not open {id} ({}); using $EDITOR",
                crate::text::inline(&why.to_string())
            );
            return Ok(false);
        }
    }
    let (store, _ws) = ctx.open()?;
    let project = store.project(id)?.ok_or_else(|| not_found(id))?;
    print_project(ctx, &project)?;
    Ok(true)
}

fn edit_in_editor(ctx: &Ctx<'_>, id: &str) -> Result<()> {
    let actor = ctx.actor()?;
    let (mut store, _ws) = ctx.open()?;
    let project = store.project(id)?.ok_or_else(|| not_found(id))?;
    let doc_id = store.design_doc_id(id)?.ok_or_else(|| {
        CliError::error(format!(
            "project '{id}' has no design doc bound yet (its binding has not synced here)"
        ))
    })?;

    // Same editor launch as `pm edit` (crate::edit, AGT-1345): $VISUAL,
    // then $EDITOR, then `vi`, run through `sh -c` and never a fallback pm
    // prompts for itself — a non-interactive `vi` just fails fast rather
    // than hang, which is how README's "no command may prompt when stdin
    // is not a TTY" is satisfied here too.
    let file = edit::TempFile::create(crate::ids::safe_component(id, "project id")?, &project.doc)?;
    if !edit::run_editor(file.path())? {
        return Err(CliError::error("edit aborted (the editor exited non-zero)"));
    }
    let new_text = file.read()?;
    // The whole file is captured in `new_text` now, so the temp file (Drop
    // removes it) has nothing left to hold onto, unlike `pm edit`'s
    // reopen-on-parse-error loop, where the file stays the live source of
    // truth across editor invocations.
    if new_text == project.doc {
        return print_project(ctx, &project);
    }

    commit_doc_text(&mut store, actor, doc_id, &new_text)?;
    let project = store
        .project(id)?
        .ok_or_else(|| CliError::error(format!("project '{id}' vanished after edit")))?;
    print_project(ctx, &project)
}

/// Diffs `new_text` against a document's replica history and commits the
/// `body.edit` (shared by the `$EDITOR` and `--from-file` paths).
fn commit_doc_text(store: &mut Store, actor: ActorId, doc_id: Ulid, new_text: &str) -> Result<()> {
    // Continue this document's causal history rather than diffing from an
    // empty replica: import whatever this replica already knows (the
    // cached snapshot, if any body.edit has ever landed) before diffing to
    // the editor's text, so the update is a minimal, correctly-merging
    // edit rather than a from-scratch replacement (pm-core::Body docs).
    // The session gets its own Loro peer (crate::edit::session_peer) so two
    // concurrent `pm project edit`s by the same actor never collide the way
    // a shared peer would (crate::edit module docs, AGT-1345).
    let mut body = Body::with_peer(edit::session_peer(Ulid::new()))
        .map_err(|e| CliError::error(format!("starting project doc session: {e}")))?;
    if let Some(view) = store.doc_view(doc_id)? {
        let snapshot = view
            .body
            .snapshot()
            .map_err(|e| CliError::error(format!("reading project doc history: {e}")))?;
        body.apply(&snapshot)
            .map_err(|e| CliError::error(format!("restoring project doc history: {e}")))?;
    }
    let update = body
        .diff_from_text(new_text)
        .map_err(|e| CliError::error(format!("diffing project doc: {e}")))?;

    let mut stamper = Stamper::new(store, actor)?;
    let op = stamper.op(
        doc_id,
        Payload::BodyEdit(BodyEdit {
            update: update.into_bytes(),
        }),
    );
    store.commit_doc_edit(doc_id, &op)?;
    Ok(())
}

/// `pm project edit <id> [--doc <name>] --from-file <path|->` (AGT-1480):
/// the non-interactive write. The file is the document BODY (a design doc
/// has no frontmatter), diffed line-faithfully through `Body::diff_from_text`
/// like the editor flow, so a concurrent edit merges; unchanged text
/// commits nothing.
fn edit_from_file(ctx: &Ctx<'_>, id: &str, doc: Option<&str>, src: &str) -> Result<()> {
    let actor = ctx.actor()?;
    let (mut store, _ws) = ctx.open()?;
    let project = store.project(id)?.ok_or_else(|| not_found(id))?;
    let (doc_id, current) = match doc {
        None => {
            let doc_id = store.design_doc_id(id)?.ok_or_else(|| {
                CliError::error(format!(
                    "project '{id}' has no design doc bound yet (its binding has not synced here)"
                ))
            })?;
            (doc_id, &project.doc)
        }
        Some(name) => {
            let not_found = || {
                CliError::not_found(format!(
                    "project '{id}' has no document '{name}' (create it with `pm project doc add`)"
                ))
            };
            let current = project.documents.get(name).ok_or_else(not_found)?;
            let doc_id = store.named_doc_id(id, name)?.ok_or_else(not_found)?;
            (doc_id, current)
        }
    };
    let text = crate::fromfile::read_source(src)?;
    let label = match doc {
        None => "design doc".to_string(),
        Some(name) => format!("named doc '{}'", crate::text::inline(name)),
    };
    if text == *current {
        if ctx.json {
            return print_project(ctx, &project);
        }
        println!("{}: {label} unchanged", crate::text::inline(id));
        return Ok(());
    }
    commit_doc_text(&mut store, actor, doc_id, &text)?;
    if ctx.json {
        let project = store
            .project(id)?
            .ok_or_else(|| CliError::error(format!("project '{id}' vanished after edit")))?;
        return print_project(ctx, &project);
    }
    println!("{}: {label} updated", crate::text::inline(id));
    Ok(())
}

// -------------------------------------------------------------------- set

/// The fields one `pm project set` changes; `None` leaves a field alone,
/// `parent: Some(None)` clears the parent.
#[derive(Debug, Default, PartialEq)]
struct ProjectAssignments {
    title: Option<String>,
    status: Option<ProjectStatus>,
    parent: Option<Option<String>>,
}

/// Parses `pm project set`'s `key=value` list (AGT-1489). Unlike `pm set`,
/// a project has no `ext`, so an unknown key is a usage error, as is a
/// malformed assignment or a key given twice. Every assignment is parsed
/// before anything is committed.
fn parse_project_assignments(assignments: &[String]) -> Result<ProjectAssignments> {
    let mut out = ProjectAssignments::default();
    for assignment in assignments {
        let Some((key, value)) = assignment.split_once('=') else {
            return Err(CliError::usage(format!(
                "'{}' is not key=value (e.g. title=\"New title\", status=complete, parent=-)",
                crate::text::inline(assignment)
            )));
        };
        let key = key.trim();
        let twice = || CliError::usage(format!("'{key}' given more than once"));
        match key {
            "title" => {
                if out.title.is_some() {
                    return Err(twice());
                }
                out.title = Some(non_empty("title", value)?);
            }
            "status" => {
                if out.status.is_some() {
                    return Err(twice());
                }
                out.status = Some(parse_status(value.trim()).map_err(CliError::usage)?);
            }
            "parent" => {
                if out.parent.is_some() {
                    return Err(twice());
                }
                out.parent = Some(match value.trim() {
                    "-" => None,
                    "" => {
                        return Err(CliError::usage(
                            "parent= needs a project id, or - to clear the parent",
                        ));
                    }
                    parent => Some(parent.to_string()),
                });
            }
            other => {
                return Err(CliError::usage(format!(
                    "unknown key '{}': expected title, status or parent",
                    crate::text::inline(other)
                )));
            }
        }
    }
    Ok(out)
}

/// `pm project set <id> key=value…` (AGT-1489): one `project.set` op per
/// field that changes, over the existing `ProjectSet::{Title,Status,Parent}`
/// (no new op kind). The store refuses an unknown parent (exit 3) and a
/// parent that is the project itself or a descendant (exit 2).
fn set(ctx: &Ctx<'_>, id: &str, assignments: &[String]) -> Result<()> {
    let change = parse_project_assignments(assignments)?;
    let actor = ctx.actor()?;
    let (mut store, _ws) = ctx.open()?;
    store.project(id)?.ok_or_else(|| not_found(id))?;
    store.set_project(
        id,
        change.title.as_deref(),
        change.status,
        change.parent.as_ref().map(Option::as_deref),
        &actor,
    )?;
    let project = store
        .project(id)?
        .ok_or_else(|| CliError::error(format!("project '{id}' vanished after set")))?;
    print_project(ctx, &project)
}

// ----------------------------------------------------------------- delete

fn delete(ctx: &Ctx<'_>, id: &str) -> Result<()> {
    let (mut store, _ws) = ctx.open()?;
    store.project(id)?.ok_or_else(|| not_found(id))?;
    let actor = ctx.actor()?;
    store.delete_project(id, &actor)?;
    if ctx.json {
        print_json(&json!({"schema": SCHEMA, "id": id, "deleted": true}));
    } else {
        println!("deleted {}", crate::text::inline(id));
    }
    Ok(())
}

// --------------------------------------------------------------- doc add

/// Names a NEW named document may not take (AGT-1481): they read as the
/// design doc, and a named doc called `design` once shadowed it in
/// everyone's head. CLI-only on purpose: pm-core accepts any safe name,
/// because existing replicas already hold such docs and a core rule would
/// wedge their sync.
fn is_reserved_doc_name(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "design" | "readme")
}

fn doc_add(ctx: &Ctx<'_>, id: &str, name: &str, from_file: &std::path::Path) -> Result<()> {
    let name = non_empty("doc name", name)?;
    if is_reserved_doc_name(&name) {
        return Err(CliError::usage(format!(
            "'{name}' is reserved: it reads as the project's design doc, which every \
             project already has. Write the design doc with \
             `pm project edit {id} --from-file <path|->`, or pick a different name for \
             a named doc"
        )));
    }
    let text = fs::read_to_string(from_file)
        .with_context(|| format!("reading {}", from_file.display()))?;
    let actor = ctx.actor()?;

    let (mut store, _ws) = ctx.open()?;
    let project = store.project(id)?.ok_or_else(|| not_found(id))?;
    if project.documents.contains_key(&name) {
        return Err(CliError::error(format!(
            "project '{id}' already has a document '{name}'; replace it with \
             `pm project edit {id} --doc {name} --from-file <path|->`"
        )));
    }
    let doc_id = store.add_named_doc(id, &name, &actor)?;

    let mut body = Body::new();
    let update = body
        .diff_from_text(&text)
        .map_err(|e| CliError::error(format!("building document: {e}")))?;
    let mut stamper = Stamper::new(&store, actor)?;
    let op = stamper.op(
        doc_id,
        Payload::BodyEdit(BodyEdit {
            update: update.into_bytes(),
        }),
    );
    store.commit_doc_edit(doc_id, &op)?;

    if ctx.json {
        print_json(&json!({"schema": SCHEMA, "project": id, "doc": name}));
    } else {
        println!(
            "{} doc/{}",
            crate::text::inline(id),
            crate::text::inline(&name)
        );
    }
    Ok(())
}

// ---------------------------------------------------------------- output

fn status_str(status: ProjectStatus) -> &'static str {
    match status {
        ProjectStatus::InProgress => "in-progress",
        ProjectStatus::Complete => "complete",
        ProjectStatus::Abandoned => "abandoned",
    }
}

/// The **Project** `--json` shape. `pub(crate)`: `pm app` (AGT-1401)
/// serves it unchanged.
pub(crate) fn project_json(p: &Project) -> Value {
    let Value::Object(fields) = serde_json::to_value(p).expect("a project serializes") else {
        unreachable!("a project serializes to an object");
    };
    let mut out = Map::new();
    out.insert("schema".into(), json!(SCHEMA));
    out.extend(fields);
    Value::Object(out)
}

fn print_project(ctx: &Ctx<'_>, project: &Project) -> Result<()> {
    if ctx.json {
        print_json(&project_json(project));
        return Ok(());
    }
    println!(
        "{}  {}",
        crate::text::inline(&project.id),
        crate::text::inline(&project.title)
    );
    println!("status:  {}", status_str(project.status));
    println!(
        "parent:  {}",
        crate::text::inline(project.parent.as_deref().unwrap_or("-"))
    );
    let repos: Vec<&str> = project.repos.iter().map(String::as_str).collect();
    println!(
        "repos:   {}",
        if repos.is_empty() {
            "-".into()
        } else {
            crate::text::inline(&repos.join(", "))
        }
    );
    if !project.documents.is_empty() {
        let names: Vec<&str> = project.documents.keys().map(String::as_str).collect();
        println!(
            "named docs: {}  (pm project show {} --doc <name>)",
            crate::text::inline(&names.join(", ")),
            crate::text::inline(&project.id)
        );
    }
    if project.doc.is_empty() {
        println!("design doc: (empty)");
    } else {
        println!("design doc:");
        println!();
        print!("{}", crate::text::printable(&project.doc));
        if !project.doc.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}
