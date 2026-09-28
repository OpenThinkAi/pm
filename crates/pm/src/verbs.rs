//! The op-backed verbs: `pm init`, `pm new`, `pm show`, `pm set`
//! (projects/pm/README.md §CLI verbs). Every mutation is a `pm_core::Op`
//! committed through `pm_store::Store::commit`; nothing here writes a
//! ticket row directly.
//!
//! No verb reads stdin or prompts, so a command run with stdin closed or
//! redirected behaves exactly as it does at a terminal (README §Constraints:
//! "No command may prompt when stdin is not a TTY").

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use pm_core::op::{FieldSet, LabelAdd, TicketCreate};
use pm_core::{ActorId, Clock, Op, Payload, Priority, State, StateCategory, Ticket, Workspace};
use pm_store::Store;
use serde_json::{Map, Value, json};
use ulid::Ulid;

use crate::exit::{CliError, Result};
use crate::workspace::{self, Config, DB_FILE, Env};

/// Version of every `--json` payload (README §Constraints: "`--json`
/// output is a versioned contract").
pub const SCHEMA: u32 = 1;

/// What every verb needs besides its own arguments: the global flags and
/// the environment.
pub struct Ctx<'a> {
    pub env: &'a Env,
    pub workspace: Option<&'a Path>,
    pub as_flag: Option<&'a str>,
    pub json: bool,
}

impl Ctx<'_> {
    /// The actor every op of this command records: `PM_ACTOR`, then
    /// `--as`, then `$USER` (README §Data model "actor").
    fn actor(&self) -> Result<ActorId> {
        ActorId::resolve(
            self.env.pm_actor.as_deref(),
            self.as_flag,
            self.env.user.as_deref(),
        )
        .ok_or_else(|| CliError::usage("no actor: set PM_ACTOR, pass --as <actor>, or set USER"))
    }

    fn open(&self) -> Result<(Store, Workspace)> {
        workspace::open(&workspace::resolve(self.workspace, self.env)?)
    }
}

/// Stamps this command's ops. The clock is seeded from the log's newest
/// HLC so a stamp is never re-issued, and fed the wall clock here — pm-core
/// never reads it.
struct Stamper {
    clock: Clock,
    actor: ActorId,
}

impl Stamper {
    fn new(store: &Store, actor: ActorId) -> Result<Self> {
        Ok(Stamper {
            clock: Clock::from_latest(store.latest_hlc()?),
            actor,
        })
    }

    fn op(&mut self, entity: Ulid, payload: Payload) -> Op {
        let hlc = self.clock.send(now_ms());
        Op::new(Ulid::new(), hlc, self.actor.clone(), entity, payload)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Clap value parser for `--priority` and `priority=`.
pub fn parse_priority(s: &str) -> std::result::Result<Priority, String> {
    serde_json::from_value(Value::String(s.to_string()))
        .map_err(|_| format!("unknown priority '{s}': expected one of low, medium, high, critical"))
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("a JSON value serializes")
    );
}

// ---------------------------------------------------------------- pm init

/// Saltline's workflow (README §Data model "state").
fn saltline_states() -> Vec<State> {
    [
        ("triage", StateCategory::Unstarted),
        ("in-progress", StateCategory::Started),
        ("done", StateCategory::Completed),
    ]
    .into_iter()
    .enumerate()
    .map(|(position, (name, category))| State {
        name: name.to_string(),
        category,
        position: position as u32,
    })
    .collect()
}

fn validate_prefix(prefix: &str) -> Result<()> {
    let mut chars = prefix.chars();
    let ok = prefix.len() <= 16
        && chars.next().is_some_and(|c| c.is_ascii_uppercase())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err(CliError::usage(format!(
            "invalid prefix '{prefix}': use 1-16 uppercase letters or digits, starting with a letter (e.g. AGT)"
        )))
    }
}

pub fn init(ctx: &Ctx<'_>, prefix: &str) -> Result<()> {
    validate_prefix(prefix)?;
    let dir = match ctx.workspace.or(ctx.env.pm_workspace.as_deref()) {
        Some(dir) => dir.to_path_buf(),
        None => ctx.env.default_workspace_dir(prefix)?,
    };
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dir = fs::canonicalize(&dir).with_context(|| format!("resolving {}", dir.display()))?;
    let db = dir.join(DB_FILE);

    let mut store = Store::open(&db)?;
    if let Some(existing) = store.workspace()? {
        return Err(CliError::error(format!(
            "{} is already a pm workspace (prefix {})",
            dir.display(),
            existing.prefix
        )));
    }
    let ws = Workspace {
        id: Ulid::new(),
        prefix: prefix.to_string(),
        states: saltline_states(),
        gate_labels: ["manual".to_string()].into(),
        model_labels: Default::default(),
        template_sections: vec!["Problem Statement".into(), "Acceptance Criteria".into()],
        stale_days: 30,
    };
    store.init_workspace(&ws)?;

    // config.toml records the default workspace the first time; an
    // existing file is never rewritten, so initializing a second (e.g.
    // scratch) workspace cannot silently repoint every later command.
    let config_path = ctx.env.config_path()?;
    let existing = Config::load(&config_path)?;
    let config_written = existing.is_none();
    if config_written {
        Config {
            workspace: Some(dir.clone()),
        }
        .write(&config_path)?;
    }

    if ctx.json {
        print_json(&json!({
            "schema": SCHEMA,
            "prefix": ws.prefix,
            "workspace": dir,
            "db": db,
            "states": ws.states,
            "config": config_path,
            "config_written": config_written,
        }));
    } else {
        println!("initialized {} workspace at {}", ws.prefix, dir.display());
        let states: Vec<String> = ws
            .states
            .iter()
            .map(|s| {
                let category = serde_json::to_value(s.category).expect("a category serializes");
                format!("{} ({})", s.name, category.as_str().unwrap_or_default())
            })
            .collect();
        println!("states: {}", states.join(", "));
        if config_written {
            println!("config: wrote {}", config_path.display());
        } else {
            println!(
                "config: {} already exists; left unchanged",
                config_path.display()
            );
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- pm new

pub struct NewArgs {
    pub title: String,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub priority: Option<Priority>,
    pub labels: Vec<String>,
}

fn non_empty(flag: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        Err(CliError::usage(format!("{flag} must not be empty")))
    } else {
        Ok(value.to_string())
    }
}

fn require_project(store: &Store, project: &str) -> Result<()> {
    if store.project(project)?.is_none() {
        return Err(CliError::not_found(format!(
            "project '{project}' does not exist; create it before filing tickets against it"
        )));
    }
    Ok(())
}

pub fn new(ctx: &Ctx<'_>, args: NewArgs) -> Result<()> {
    let title = non_empty("--title", &args.title)?;
    let project = args
        .project
        .as_deref()
        .map(|p| non_empty("--project", p))
        .transpose()?;
    let repo = args
        .repo
        .as_deref()
        .map(|r| non_empty("--repo", r))
        .transpose()?;
    let labels: BTreeSet<String> = args
        .labels
        .iter()
        .map(|l| non_empty("--label", l))
        .collect::<Result<_>>()?;
    let actor = ctx.actor()?;

    let (mut store, ws) = ctx.open()?;
    let state = ws
        .states
        .iter()
        .filter(|s| s.category == StateCategory::Unstarted)
        .min_by_key(|s| s.position)
        .ok_or_else(|| CliError::error("this workspace has no unstarted state to file into"))?
        .name
        .clone();
    // Checked up front so a missing project fails before any op lands
    // (the store would reject the create too, as R2).
    if let Some(project) = &project {
        require_project(&store, project)?;
    }

    let id = Ulid::new();
    let mut stamper = Stamper::new(&store, actor.clone())?;
    store.commit(&stamper.op(
        id,
        Payload::TicketCreate(TicketCreate {
            title,
            state,
            priority: args.priority.unwrap_or_default(),
            project,
            repo,
            source: None,
            ext: Default::default(),
        }),
    ))?;
    for label in labels {
        store.commit(&stamper.op(id, Payload::LabelAdd(LabelAdd { label })))?;
    }
    // Phase 1: this database is the numbering authority (README §Conflict
    // semantics).
    store.allocate_number(id, &actor)?;

    let ticket = store
        .ticket(id)?
        .ok_or_else(|| CliError::error(format!("ticket {id} vanished after create")))?;
    if ctx.json {
        print_json(&ticket_json(&ws, &ticket));
    } else {
        println!("{}", display_id(&ws, &ticket));
    }
    Ok(())
}

// ---------------------------------------------------------------- pm show

/// `AGT-12`, or `AGT-?` before the authority numbers it.
fn display_id(ws: &Workspace, t: &Ticket) -> String {
    match t.number {
        Some(n) => format!("{}-{n}", ws.prefix),
        None => format!("{}-?", ws.prefix),
    }
}

/// A ticket named on the command line: `<PREFIX>-<n>` or its ULID.
fn find(store: &Store, ws: &Workspace, reference: &str) -> Result<Ticket> {
    let r = reference.trim();
    if let Some((prefix, digits)) = r.rsplit_once('-')
        && !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
    {
        if !prefix.eq_ignore_ascii_case(&ws.prefix) {
            return Err(CliError::not_found(format!(
                "no ticket {r}: this workspace's prefix is {}",
                ws.prefix
            )));
        }
        let number: u64 = digits
            .parse()
            .map_err(|_| CliError::usage(format!("ticket number in '{r}' is out of range")))?;
        return store
            .ticket_by_number(number)?
            .ok_or_else(|| CliError::not_found(format!("no ticket {r}")));
    }
    if let Ok(id) = r.parse::<Ulid>() {
        return store
            .ticket(id)?
            .ok_or_else(|| CliError::not_found(format!("no ticket {r}")));
    }
    Err(CliError::usage(format!(
        "'{r}' is not a ticket id: expected {}-<number> or a ULID",
        ws.prefix
    )))
}

/// The `--json` shape of a ticket: every `pm_core::Ticket` field, with
/// `id` the human id (`AGT-12`) and the ULID under `ulid`, plus `schema`.
fn ticket_json(ws: &Workspace, t: &Ticket) -> Value {
    let Value::Object(fields) = serde_json::to_value(t).expect("a ticket serializes") else {
        unreachable!("a ticket serializes to an object");
    };
    let mut out = Map::new();
    out.insert("schema".into(), json!(SCHEMA));
    for (key, value) in fields {
        if key == "id" {
            out.insert("ulid".into(), value);
        } else {
            out.insert(key, value);
        }
    }
    out.insert("id".into(), json!(display_id(ws, t)));
    Value::Object(out)
}

fn print_human(ws: &Workspace, t: &Ticket) {
    let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
    println!("{}  {}", display_id(ws, t), t.title);
    println!("state:     {}", t.state);
    println!(
        "priority:  {}",
        serde_json::to_value(t.priority)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    );
    println!("project:   {}", dash(&t.project));
    println!("repo:      {}", dash(&t.repo));
    println!(
        "assignee:  {}",
        t.assignee
            .as_ref()
            .map_or_else(|| "-".into(), ActorId::to_string)
    );
    let labels: Vec<&str> = t.labels.iter().map(String::as_str).collect();
    println!(
        "labels:    {}",
        if labels.is_empty() {
            "-".into()
        } else {
            labels.join(", ")
        }
    );
    for (name, value) in [
        ("linked-github", &t.linked_github),
        ("linked-pr", &t.linked_pr),
        ("linear", &t.linear),
    ] {
        if let Some(value) = value {
            println!("{name}: {value}");
        }
    }
    if let Some(hold) = &t.hold {
        println!("hold:      {} (by {})", hold.reason, hold.by);
    }
    if t.deleted {
        println!("deleted:   true");
    }
    println!("ulid:      {}", t.id);
    if !t.description.is_empty() {
        println!();
        print!("{}", t.description);
        if !t.description.ends_with('\n') {
            println!();
        }
    }
}

/// `--field k`: one value, raw. Strings print as-is, null as an empty
/// line, a list of strings one per line, anything else as compact JSON.
/// With `--json`, `{schema, k: value}`.
fn print_field(ctx: &Ctx<'_>, ticket: &Value, field: &str) -> Result<()> {
    let key = field.replace('-', "_");
    let value = match ticket.get(&key) {
        Some(value) if key != "schema" => value,
        _ => {
            let known: Vec<&str> = ticket
                .as_object()
                .into_iter()
                .flat_map(|o| o.keys())
                .map(String::as_str)
                .filter(|k| *k != "schema")
                .collect();
            return Err(CliError::usage(format!(
                "unknown field '{field}': expected one of {}",
                known.join(", ")
            )));
        }
    };
    if ctx.json {
        print_json(&json!({ "schema": SCHEMA, key: value }));
        return Ok(());
    }
    match value {
        Value::String(s) => println!("{s}"),
        Value::Null => println!(),
        Value::Array(items) if items.iter().all(Value::is_string) => {
            for item in items {
                println!("{}", item.as_str().unwrap_or_default());
            }
        }
        other => println!("{other}"),
    }
    Ok(())
}

pub fn show(ctx: &Ctx<'_>, reference: &str, field: Option<&str>) -> Result<()> {
    let (store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    match field {
        Some(field) => print_field(ctx, &ticket_json(&ws, &ticket), field)?,
        None if ctx.json => print_json(&ticket_json(&ws, &ticket)),
        None => print_human(&ws, &ticket),
    }
    Ok(())
}

// ----------------------------------------------------------------- pm set

/// Parses one `key=value`. Empty values clear optional fields.
fn parse_assignment(assignment: &str) -> Result<FieldSet> {
    let Some((key, value)) = assignment.split_once('=') else {
        return Err(CliError::usage(format!(
            "'{assignment}' is not key=value (e.g. title=\"New title\")"
        )));
    };
    let optional = |v: &str| {
        let v = v.trim();
        (!v.is_empty()).then(|| v.to_string())
    };
    match key.trim() {
        "title" => Ok(FieldSet::Title(non_empty("title", value)?)),
        "priority" => Ok(FieldSet::Priority(
            parse_priority(value.trim()).map_err(CliError::usage)?,
        )),
        "project" => Ok(FieldSet::Project(optional(value))),
        "repo" => Ok(FieldSet::Repo(optional(value))),
        other => Err(CliError::usage(format!(
            "unsupported field '{other}': pm set accepts title, priority, project, repo"
        ))),
    }
}

pub fn set(ctx: &Ctx<'_>, reference: &str, assignments: &[String]) -> Result<()> {
    let fields: Vec<FieldSet> = assignments
        .iter()
        .map(|a| parse_assignment(a))
        .collect::<Result<_>>()?;
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    // Validate every assignment before the first op lands, so a bad one
    // never leaves the ticket half-updated.
    for field in &fields {
        if let FieldSet::Project(Some(project)) = field {
            require_project(&store, project)?;
        }
    }
    let mut stamper = Stamper::new(&store, actor)?;
    for field in fields {
        store.commit(&stamper.op(ticket.id, Payload::FieldSet(field)))?;
    }
    let ticket = store
        .ticket(ticket.id)?
        .ok_or_else(|| CliError::error(format!("ticket {} vanished after set", ticket.id)))?;
    if ctx.json {
        print_json(&ticket_json(&ws, &ticket));
    } else {
        println!("{}", display_id(&ws, &ticket));
    }
    Ok(())
}
