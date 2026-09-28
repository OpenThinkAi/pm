//! `pm project new/show/list/edit/doc/delete` (AGT-1344,
//! projects/pm/README.md §CLI verbs: "`pm project new/show/list/edit` —
//! project README template, `view-projects --json`").
//!
//! Project *metadata* (title/status/parent/repos) is a direct write
//! (`pm_store::Store::create_project`, mirroring AGT-1335's `put_project`);
//! only a document's *body* — the design doc `pm project edit` opens, and
//! any named document `pm project doc add` creates — is a `body.edit` op
//! against that document's own `doc_id`, the same op kind and
//! [`pm_core::Body`] CRDT a ticket's description uses (AGT-1338): there is
//! exactly one body format in the op log, ever.
//!
//! `pm project edit` never prompts on its own (README §Constraints: "No
//! command may prompt when stdin is not a TTY") — it requires `$EDITOR` to
//! be set and fails with a usage error otherwise, rather than falling back
//! to some default terminal editor that would hang or fail without one.
//! Tests exercise it by pointing `$EDITOR` at a script that rewrites the
//! file non-interactively; that needs no TTY either, since pm itself never
//! prompts and the script doesn't read one.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::Context;
use clap::Subcommand;
use pm_core::op::BodyEdit;
use pm_core::{Body, Payload, Project, ProjectStatus};
use pm_store::Store;
use serde_json::{Map, Value, json};
use ulid::Ulid;

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

    let (mut store, _ws) = ctx.open()?;
    store.create_project(id, &title, &repos, parent)?;
    let project = store
        .project(id)?
        .ok_or_else(|| CliError::error(format!("project '{id}' vanished after create")))?;
    print_project(ctx, &store, &project)
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
        None => print_project(ctx, &store, &project),
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

/// `$EDITOR`, required: pm never falls back to a default terminal editor
/// (README §Constraints, module docs above).
fn require_editor() -> Result<String> {
    std::env::var("EDITOR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            CliError::usage(
                "pm project edit requires $EDITOR to be set (pm never prompts on its own); \
                 use `pm project doc add --from-file` for a non-interactive document instead",
            )
        })
}

fn edit(ctx: &Ctx<'_>, id: &str) -> Result<()> {
    let editor = require_editor()?;
    let actor = ctx.actor()?;
    let (mut store, _ws) = ctx.open()?;
    let project = store.project(id)?.ok_or_else(|| not_found(id))?;
    let doc_id = store.design_doc_id(id)?.ok_or_else(|| {
        CliError::error(format!(
            "project '{id}' has no design-doc id; it was created before AGT-1344 or written \
             directly (`put_project`) — recreate it with `pm project new` to get one"
        ))
    })?;

    let path = std::env::temp_dir().join(format!("pm-project-doc-{}.md", Ulid::new()));
    fs::write(&path, &project.doc)
        .with_context(|| format!("writing scratch file {}", path.display()))?;
    let status = Command::new(&editor)
        .arg(&path)
        .status()
        .with_context(|| format!("running $EDITOR ({editor})"));
    let status = match status {
        Ok(status) => status,
        Err(e) => {
            let _ = fs::remove_file(&path);
            return Err(e.into());
        }
    };
    if !status.success() {
        let _ = fs::remove_file(&path);
        return Err(CliError::error(format!("$EDITOR exited with {status}")));
    }
    let new_text =
        fs::read_to_string(&path).with_context(|| format!("reading back {}", path.display()))?;
    let _ = fs::remove_file(&path);

    if new_text == project.doc {
        return print_project(ctx, &store, &project);
    }

    // Continue this document's causal history rather than diffing from an
    // empty replica: import whatever this replica already knows (the
    // cached snapshot, if any body.edit has ever landed) before diffing to
    // the editor's text, so the update is a minimal, correctly-merging
    // edit rather than a from-scratch replacement (pm-core::Body docs).
    let mut body = Body::new();
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
    print_project(ctx, &store, &project)
}

// ----------------------------------------------------------------- delete

fn delete(ctx: &Ctx<'_>, id: &str) -> Result<()> {
    let (mut store, _ws) = ctx.open()?;
    store.project(id)?.ok_or_else(|| not_found(id))?;
    store.delete_project(id)?;
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
    let doc_id = store.add_named_doc(id, &name)?;

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

fn print_project(ctx: &Ctx<'_>, _store: &Store, project: &Project) -> Result<()> {
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
