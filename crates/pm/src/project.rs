//! `pm project new/show/list/edit/doc/delete` (AGT-1344,
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
use pm_core::{Body, Payload, Project, ProjectStatus};
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
    /// Open the design doc in $EDITOR and record the result as body.edit ops
    Edit { id: String },
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
        ProjectCmd::Edit { id } => edit(ctx, &id),
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

/// A project id (README §Data model: "project — id (kebab)"): lowercase
/// letters, digits and single hyphens, never leading, trailing or doubled.
fn validate_project_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
        && !id.ends_with('-')
        && !id.contains("--");
    if ok {
        Ok(())
    } else {
        Err(CliError::usage(format!(
            "invalid project id '{id}': use lowercase letters, digits and single hyphens \
             (e.g. pm-project-verbs)"
        )))
    }
}

fn not_found(id: &str) -> CliError {
    CliError::not_found(format!("no project '{id}'"))
}

// -------------------------------------------------------------------- new

fn new(ctx: &Ctx<'_>, id: &str, title: &str, repos: &[String], parent: Option<&str>) -> Result<()> {
    validate_project_id(id)?;
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
                print!("{body}");
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
            println!("{:<24} {:<12} {}", p.id, status_str(p.status), p.title);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------- edit

fn edit(ctx: &Ctx<'_>, id: &str) -> Result<()> {
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
    let file = edit::TempFile::create(id, &project.doc)?;
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
        .diff_from_text(&new_text)
        .map_err(|e| CliError::error(format!("diffing project doc: {e}")))?;

    let mut stamper = Stamper::new(&store, actor)?;
    let op = stamper.op(
        doc_id,
        Payload::BodyEdit(BodyEdit {
            update: update.into_bytes(),
        }),
    );
    store.commit_doc_edit(doc_id, &op)?;
    let project = store
        .project(id)?
        .ok_or_else(|| CliError::error(format!("project '{id}' vanished after edit")))?;
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
        println!("deleted {id}");
    }
    Ok(())
}

// --------------------------------------------------------------- doc add

fn doc_add(ctx: &Ctx<'_>, id: &str, name: &str, from_file: &std::path::Path) -> Result<()> {
    let name = non_empty("doc name", name)?;
    let text = fs::read_to_string(from_file)
        .with_context(|| format!("reading {}", from_file.display()))?;
    let actor = ctx.actor()?;

    let (mut store, _ws) = ctx.open()?;
    store.project(id)?.ok_or_else(|| not_found(id))?;
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
        println!("{id} doc/{name}");
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

fn project_json(p: &Project) -> Value {
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
    println!("{}  {}", project.id, project.title);
    println!("status:  {}", status_str(project.status));
    println!("parent:  {}", project.parent.as_deref().unwrap_or("-"));
    let repos: Vec<&str> = project.repos.iter().map(String::as_str).collect();
    println!(
        "repos:   {}",
        if repos.is_empty() {
            "-".into()
        } else {
            repos.join(", ")
        }
    );
    if !project.documents.is_empty() {
        let names: Vec<&str> = project.documents.keys().map(String::as_str).collect();
        println!("docs:    {}", names.join(", "));
    }
    if !project.doc.is_empty() {
        println!();
        print!("{}", project.doc);
        if !project.doc.ends_with('\n') {
            println!();
        }
    }
    Ok(())
}
