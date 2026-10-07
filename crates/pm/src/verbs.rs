//! The op-backed verbs: `pm init`, `pm new`, `pm show`, `pm set`
//! (projects/pm/README.md §CLI verbs). Every mutation is a `pm_core::Op`
//! committed through `pm_store::Store::commit`; nothing here writes a
//! ticket row directly.
//!
//! No verb reads stdin or prompts, so a command run with stdin closed or
//! redirected behaves exactly as it does at a terminal (README §Constraints:
//! "No command may prompt when stdin is not a TTY").

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context;
use pm_core::op::{BodyEdit, FieldSet, LabelAdd, RelationAdd, TicketCreate};
use pm_core::{
    ActorId, Body, Clock, Op, Payload, Priority, Relation, RelationKind, Source, State,
    StateCategory, Ticket, Workspace,
};
use pm_store::Store;
use serde_json::{Map, Value, json};
use ulid::Ulid;

use crate::batch::{self, BatchEntry, SourceFm};
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
    pub(crate) fn actor(&self) -> Result<ActorId> {
        ActorId::resolve(
            self.env.pm_actor.as_deref(),
            self.as_flag,
            self.env.user.as_deref(),
        )
        .ok_or_else(|| CliError::usage("no actor: set PM_ACTOR, pass --as <actor>, or set USER"))
    }

    /// `pub(crate)`: the read verbs (`crate::read`) open a workspace too,
    /// but never resolve an actor — reads never write an op.
    pub(crate) fn open(&self) -> Result<(Store, Workspace)> {
        workspace::open(&workspace::resolve(self.workspace, self.env)?)
    }
}

/// Stamps this command's ops. The clock is seeded from the log's newest
/// HLC so a stamp is never re-issued, and fed the wall clock here — pm-core
/// never reads it. `pub(crate)`: `crate::mutate` and `crate::project`
/// (AGT-1344) stamp their own ops the same way.
pub(crate) struct Stamper {
    clock: Clock,
    actor: ActorId,
}

impl Stamper {
    pub(crate) fn new(store: &Store, actor: ActorId) -> Result<Self> {
        Ok(Stamper {
            clock: Clock::from_latest(store.latest_hlc()?),
            actor,
        })
    }

    pub(crate) fn op(&mut self, entity: Ulid, payload: Payload) -> Op {
        let hlc = self.clock.send(now_ms());
        Op::new(Ulid::new(), hlc, self.actor.clone(), entity, payload)
    }

    /// Like [`Stamper::op`], but for the one payload shape that needs to
    /// carry its own stamp as data: `FieldSet::ArchivedAt(Some(hlc))`
    /// (`crate::archive`) records the HLC an archive op landed at, so the
    /// value must be exactly the op's own `hlc` rather than a second,
    /// slightly later one from calling [`Stamper::op`] a second time.
    pub(crate) fn op_with_hlc(
        &mut self,
        entity: Ulid,
        payload: impl FnOnce(pm_core::Hlc) -> Payload,
    ) -> Op {
        let hlc = self.clock.send(now_ms());
        Op::new(Ulid::new(), hlc, self.actor.clone(), entity, payload(hlc))
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A moment for human-readable (text) output: the UTC date-time
/// `YYYY-MM-DD HH:MM UTC` of an HLC's wall clock (AGT-1449). UTC, to match
/// every date pm already prints (`pm show` comment dates, hold dates, `pm
/// export md`) — pm never reads the local timezone. The counter is dropped;
/// `--json` carries the exact `{wall_ms, counter}`.
pub(crate) fn when(hlc: &pm_core::Hlc) -> String {
    let (date, hh, mm, _) = utc_parts(hlc.wall_ms);
    format!("{date} {hh:02}:{mm:02} UTC")
}

/// Like [`when`] with seconds (`YYYY-MM-DD HH:MM:SS UTC`), for `pm log`
/// rows, where neighbouring ops are seconds apart.
pub(crate) fn when_secs(hlc: &pm_core::Hlc) -> String {
    let (date, hh, mm, ss) = utc_parts(hlc.wall_ms);
    format!("{date} {hh:02}:{mm:02}:{ss:02} UTC")
}

fn utc_parts(ms: u64) -> (String, u64, u64, u64) {
    let secs = ms / 1000 % 86_400;
    (
        pm_core::markers::date_from_ms(ms),
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
    )
}

/// Clap value parser for `--priority` and `priority=`.
pub fn parse_priority(s: &str) -> std::result::Result<Priority, String> {
    serde_json::from_value(Value::String(s.to_string()))
        .map_err(|_| format!("unknown priority '{s}': expected one of low, medium, high, critical"))
}

/// `pub(crate)`: `crate::read` prints JSON for `pm list`/`pm log`/
/// `pm status`/`pm graph` too.
pub(crate) fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("a JSON value serializes")
    );
}

// ---------------------------------------------------------------- pm init

/// Which seed `pm init` writes. Presets are data, not branching logic
/// beyond this module: each names a default prefix, a workflow, and a
/// gate-label set; everything else (template sections, stale_days) is
/// shared (AGT-1373 design: "presets as data").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preset {
    /// The neutral, no-flags default for outside users: prefix `PM`,
    /// states `backlog`/`todo`/`in-progress`/`done`, no gate or model
    /// labels.
    Default,
    /// Reproduces the workspace this repo's own build loops have always
    /// used: prefix `AGT`, states `triage`/`in-progress`/`done`, and the
    /// `manual` gate label.
    Saltline,
}

impl Preset {
    /// The prefix this preset seeds when `--prefix` is not given.
    fn default_prefix(self) -> &'static str {
        match self {
            Preset::Default => "PM",
            Preset::Saltline => "AGT",
        }
    }

    /// This preset's workflow, in position order. `Default`'s `backlog`
    /// state uses [`StateCategory::Backlog`], which `pm ready`/`pm claim`
    /// (`pm_core::ready`) already treat as not-yet-claimable — only
    /// `Unstarted` is a ready candidate, so filing into `backlog` doesn't
    /// make a ticket claimable until it's moved to `todo`.
    fn states(self) -> Vec<State> {
        let named = match self {
            Preset::Default => [
                ("backlog", StateCategory::Backlog),
                ("todo", StateCategory::Unstarted),
                ("in-progress", StateCategory::Started),
                ("done", StateCategory::Completed),
            ]
            .as_slice(),
            Preset::Saltline => [
                ("triage", StateCategory::Unstarted),
                ("in-progress", StateCategory::Started),
                ("done", StateCategory::Completed),
            ]
            .as_slice(),
        };
        named
            .iter()
            .enumerate()
            .map(|(position, (name, category))| State {
                name: name.to_string(),
                category: *category,
                position: position as u32,
            })
            .collect()
    }

    /// Labels that gate a ticket out of the ready frontier
    /// ([`pm_core::ready::Rules::gate_labels`]). The default preset seeds
    /// none — a public user hasn't adopted saltline's "manual" human-review
    /// convention.
    fn gate_labels(self) -> BTreeSet<String> {
        match self {
            Preset::Default => BTreeSet::new(),
            Preset::Saltline => ["manual".to_string()].into(),
        }
    }
}

/// Clap value parser for `--preset`.
pub fn parse_preset(s: &str) -> std::result::Result<Preset, String> {
    match s {
        "default" => Ok(Preset::Default),
        "saltline" => Ok(Preset::Saltline),
        other => Err(format!(
            "unknown preset '{other}': expected one of default, saltline"
        )),
    }
}

/// Clap value parser for a state category (`--category`, `pm init
/// --state NAME:CATEGORY`): the five Linear categories, lowercase.
pub fn parse_category(s: &str) -> std::result::Result<StateCategory, String> {
    match s {
        "backlog" => Ok(StateCategory::Backlog),
        "unstarted" => Ok(StateCategory::Unstarted),
        "started" => Ok(StateCategory::Started),
        "completed" => Ok(StateCategory::Completed),
        "canceled" => Ok(StateCategory::Canceled),
        other => Err(format!(
            "unknown category '{other}': expected one of backlog, unstarted, started, completed, canceled"
        )),
    }
}

/// A category as `--json` and `--category` spell it.
pub fn category_name(c: StateCategory) -> &'static str {
    match c {
        StateCategory::Backlog => "backlog",
        StateCategory::Unstarted => "unstarted",
        StateCategory::Started => "started",
        StateCategory::Completed => "completed",
        StateCategory::Canceled => "canceled",
    }
}

/// Clap value parser for `pm init --state NAME:CATEGORY` (AGT-1518 AC3).
pub fn parse_state_spec(s: &str) -> std::result::Result<(String, StateCategory), String> {
    let (name, category) = s
        .split_once(':')
        .ok_or_else(|| format!("--state '{s}' is not NAME:CATEGORY (e.g. qa:started)"))?;
    let name = name.trim();
    if !pm_core::ids::is_safe_component(name) {
        return Err(format!(
            "state name '{}' is not safe to use in a file path (letters, digits, `-`, `_`, `.`)",
            name.escape_default()
        ));
    }
    Ok((name.to_string(), parse_category(category.trim())?))
}

/// The workflow `pm init --state …` seeds instead of the preset's, in the
/// order given (positions 0, 1, …). It needs an `unstarted` state (where
/// `pm new` files) and a `completed` one (where `pm done` moves), and no
/// name twice; exit `2` otherwise.
fn custom_states(specs: &[(String, StateCategory)]) -> Result<Vec<State>> {
    let mut seen = BTreeSet::new();
    for (name, _) in specs {
        if !seen.insert(name.as_str()) {
            return Err(CliError::usage(format!("--state '{name}' is given twice")));
        }
    }
    for required in [StateCategory::Unstarted, StateCategory::Completed] {
        if !specs.iter().any(|(_, c)| *c == required) {
            return Err(CliError::usage(format!(
                "--state: the workflow needs at least one {} state",
                category_name(required)
            )));
        }
    }
    Ok(specs
        .iter()
        .enumerate()
        .map(|(position, (name, category))| State {
            name: name.clone(),
            category: *category,
            position: position as u32,
        })
        .collect())
}

/// `pm init`. With `join` (AGT-1396, `--join <WORKSPACE-ULID>`) the new
/// database is an empty replica of that workspace — its id, no ops —
/// for a second machine: `pm hub login` and `pm sync` then pull the whole
/// log, config included, from the hub. The prefix and states the row
/// starts with are placeholders the first pull overwrites.
/// `states` (AGT-1518 AC3, `--state NAME:CATEGORY`…), when non-empty,
/// replaces the preset's workflow; the preset still supplies the prefix
/// default and gate labels.
pub fn init(
    ctx: &Ctx<'_>,
    preset: Preset,
    prefix: Option<&str>,
    join: Option<Ulid>,
    states: &[(String, StateCategory)],
) -> Result<()> {
    let prefix = prefix.unwrap_or_else(|| preset.default_prefix());
    crate::ids::validate_prefix(prefix)?;
    let custom = if states.is_empty() {
        None
    } else {
        Some(custom_states(states)?)
    };
    let dir = match ctx.workspace.or(ctx.env.pm_workspace.as_deref()) {
        Some(dir) => dir.to_path_buf(),
        None => ctx.env.default_workspace_dir(prefix)?,
    };
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let dir = fs::canonicalize(&dir).with_context(|| format!("resolving {}", dir.display()))?;
    let db = dir.join(DB_FILE);

    let actor = ctx.actor()?;
    let mut store = Store::open(&db)?;
    if let Some(existing) = store.workspace()? {
        return Err(CliError::error(format!(
            "{} is already a pm workspace (prefix {})",
            dir.display(),
            existing.prefix
        )));
    }
    let ws = match join {
        Some(id) => {
            // No ops: the workspace's config arrives from the hub. A
            // config op of our own here would be a *newer* write of the
            // prefix and states and win the merge over the seeded ones.
            store.join_workspace(id, prefix)?;
            Workspace {
                id,
                prefix: prefix.to_string(),
                states: Vec::new(),
                gate_labels: Default::default(),
                model_labels: Default::default(),
                template_sections: Vec::new(),
                stale_days: 30,
                docs_owned_by: Default::default(),
            }
        }
        None => {
            let ws = Workspace {
                id: Ulid::new(),
                prefix: prefix.to_string(),
                states: custom.unwrap_or_else(|| preset.states()),
                gate_labels: preset.gate_labels(),
                model_labels: Default::default(),
                template_sections: vec!["Problem Statement".into(), "Acceptance Criteria".into()],
                stale_days: 30,
                docs_owned_by: Default::default(),
            };
            // The workspace's first ops (AGT-1385): one `workspace.set` per
            // field and gate label, one `state.upsert` per state, under this
            // command's actor.
            store.init_workspace(&ws, &actor)?;
            ws
        }
    };

    // config.toml records the default workspace the first time; an
    // existing file is never rewritten, so initializing a second (e.g.
    // scratch) workspace cannot silently repoint every later command.
    let config_path = ctx.env.config_path()?;
    let existing = Config::load(&config_path)?;
    let config_written = existing.is_none();
    if config_written {
        Config {
            workspace: Some(dir.clone()),
            backup: None,
            edit: None,
            hub: None,
            ui_leaf: None,
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
            "joined": join.map(|id| id.to_string()),
        }));
    } else if join.is_some() {
        println!(
            "joined workspace {} at {} (empty until `pm hub login` and `pm sync` pull its log)",
            ws.id,
            dir.display()
        );
        if config_written {
            println!("config: wrote {}", config_path.display());
        } else {
            println!(
                "config: {} already exists; left unchanged",
                config_path.display()
            );
        }
    } else {
        println!(
            "initialized {} workspace at {}",
            crate::text::inline(&ws.prefix),
            dir.display()
        );
        let states: Vec<String> = ws
            .states
            .iter()
            .map(|s| {
                format!(
                    "{} ({})",
                    crate::text::inline(&s.name),
                    category_name(s.category)
                )
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
    pub title: Option<String>,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub priority: Option<Priority>,
    pub labels: Vec<String>,
    pub description: Option<String>,
    pub description_file: Option<String>,
    pub blocked_by: Vec<String>,
    pub linked_github: Option<String>,
    pub source: Option<String>,
    pub from_file: Option<PathBuf>,
    pub batch: Option<PathBuf>,
}

pub(crate) fn non_empty(flag: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        Err(CliError::usage(format!("{flag} must not be empty")))
    } else {
        Ok(value.to_string())
    }
}

pub(crate) fn require_project(store: &Store, project: &str) -> Result<()> {
    if store.project(project)?.is_none() {
        return Err(CliError::not_found(format!(
            "project '{project}' does not exist; create it before filing tickets against it"
        )));
    }
    Ok(())
}

/// The workspace's initial (unstarted) state — where every new ticket
/// starts.
pub(crate) fn initial_state(ws: &Workspace) -> Result<String> {
    Ok(ws
        .states
        .iter()
        .filter(|s| s.category == StateCategory::Unstarted)
        .min_by_key(|s| s.position)
        .ok_or_else(|| CliError::error("this workspace has no unstarted state to file into"))?
        .name
        .clone())
}

/// `--description` / `--description-file <path|->` (AC5): at most one,
/// `-` reads stdin.
fn read_description(
    description: Option<String>,
    description_file: Option<String>,
) -> Result<Option<String>> {
    match (description, description_file) {
        (Some(_), Some(_)) => Err(CliError::usage(
            "--description and --description-file are mutually exclusive",
        )),
        (Some(text), None) => Ok(Some(text)),
        (None, Some(path)) if path == "-" => {
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("reading --description-file - from stdin")?;
            Ok(Some(buf))
        }
        (None, Some(path)) => {
            Ok(Some(fs::read_to_string(&path).with_context(|| {
                format!("reading --description-file {path}")
            })?))
        }
        (None, None) => Ok(None),
    }
}

/// `--source type=…,url=…,id=…[,fetched-at=…]` (AC5).
fn parse_source_flag(spec: &str) -> Result<Source> {
    let mut kind = String::new();
    let mut url = String::new();
    let mut id = String::new();
    let mut fetched_at = String::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let Some((key, value)) = part.split_once('=') else {
            return Err(CliError::usage(format!(
                "--source: '{part}' is not key=value (expected type=…,url=…,id=…)"
            )));
        };
        match key.trim() {
            "type" => kind = value.trim().to_string(),
            "url" => url = value.trim().to_string(),
            "id" => id = value.trim().to_string(),
            "fetched-at" | "fetched_at" => fetched_at = value.trim().to_string(),
            other => {
                return Err(CliError::usage(format!(
                    "--source: unknown key '{other}': expected type, url, id or fetched-at"
                )));
            }
        }
    }
    if kind.is_empty() {
        return Err(CliError::usage(
            "--source requires type=… (e.g. manual, github, linear, jira, notion)",
        ));
    }
    Ok(Source {
        kind,
        url,
        id,
        fetched_at,
    })
}

/// Builds the op set for one new ticket: create, labels, blocked-by
/// relations, linked-github/pr, and a body.edit for the description —
/// every op AC1/AC2/AC5 need, shared by the single-ticket, `--from-file`
/// and `--batch` paths.
#[allow(clippy::too_many_arguments)]
fn build_create_ops(
    stamper: &mut Stamper,
    id: Ulid,
    state: &str,
    title: String,
    priority: Priority,
    project: Option<String>,
    repo: Option<String>,
    source: Option<Source>,
    ext: BTreeMap<String, Value>,
    labels: BTreeSet<String>,
    blocked_by: Vec<Ulid>,
    description: Option<String>,
    linked_github: Option<String>,
    linked_pr: Option<String>,
) -> Result<Vec<Op>> {
    let mut ops = vec![stamper.op(
        id,
        Payload::TicketCreate(TicketCreate {
            title,
            state: state.to_string(),
            priority,
            project,
            repo,
            source,
            ext,
        }),
    )];
    for label in labels {
        ops.push(stamper.op(id, Payload::LabelAdd(LabelAdd { label })));
    }
    // `blocked-by` reads "this ticket is blocked by <blocker>": the
    // relation's `from` is the blocker, `to` is the ticket being created
    // (RelationKind::Blocks: "from blocks to").
    for blocker in blocked_by {
        ops.push(stamper.op(
            id,
            Payload::RelationAdd(RelationAdd {
                relation: Relation {
                    kind: RelationKind::Blocks,
                    from: blocker,
                    to: id,
                },
            }),
        ));
    }
    if let Some(github) = linked_github {
        ops.push(stamper.op(id, Payload::FieldSet(FieldSet::LinkedGithub(Some(github)))));
    }
    if let Some(pr) = linked_pr {
        ops.push(stamper.op(id, Payload::FieldSet(FieldSet::LinkedPr(Some(pr)))));
    }
    if let Some(text) = description
        .map(|d| d.trim().to_string())
        .filter(|d| !d.is_empty())
    {
        let mut body = Body::new();
        let update = body
            .diff_from_text(&text)
            .map_err(|e| CliError::error(format!("building description: {e}")))?;
        ops.push(stamper.op(
            id,
            Payload::BodyEdit(BodyEdit {
                update: update.into_bytes(),
            }),
        ));
    }
    Ok(ops)
}

/// Commits one `pm new` invocation's ops — every ticket's create set in
/// `ops`, the tickets themselves in `ids` — and numbers them the way this
/// machine numbers tickets ([`crate::hub::numbers_are_hub_assigned`]):
/// locally, as the numbering authority (README §Conflict semantics, phase
/// 1), or not at all, flagged pending until the hub's `field.set number`
/// arrives on `pm sync` (AGT-1398). One transaction either way. Returns
/// the tickets in `ids` order.
fn commit_new(
    store: &mut Store,
    env: &Env,
    ops: &[Op],
    ids: &[Ulid],
    actor: &ActorId,
) -> Result<Vec<Ticket>> {
    if crate::hub::numbers_are_hub_assigned(env)? {
        Ok(store.commit_batch_pending(ops, ids)?)
    } else {
        let to_number: Vec<(Ulid, ActorId)> = ids.iter().map(|id| (*id, actor.clone())).collect();
        Ok(store.commit_batch(ops, &to_number)?)
    }
}

/// The line `pm new` prints for a ticket it made: `AGT-12`, or — while the
/// number is the hub's to give — `AGT-?  <ULID>`, so the id to refer to it
/// by until then is right there.
pub(crate) fn created_line(ws: &Workspace, t: &Ticket) -> String {
    match t.number {
        Some(_) => display_id(ws, t),
        None => format!("{}  {}", display_id(ws, t), t.id),
    }
}

fn print_created_ticket(
    ctx: &Ctx<'_>,
    store: &Store,
    ws: &Workspace,
    ticket: &Ticket,
) -> Result<()> {
    if ctx.json {
        print_json(&ticket_json(ws, store, ticket)?);
    } else {
        println!("{}", created_line(ws, ticket));
    }
    Ok(())
}

/// Dispatches `pm new` across its three mutually exclusive modes: plain
/// flags, `--from-file <path>` (AC1) and `--batch <path>` (AC2-4).
pub fn new(ctx: &Ctx<'_>, args: NewArgs) -> Result<()> {
    let single_flags_set = args.title.is_some()
        || args.project.is_some()
        || args.repo.is_some()
        || args.priority.is_some()
        || !args.labels.is_empty()
        || args.description.is_some()
        || args.description_file.is_some()
        || !args.blocked_by.is_empty()
        || args.linked_github.is_some()
        || args.source.is_some();
    const CONFLICT: &str = "cannot be combined with --title/--project/--repo/--priority/--label/\
--description/--description-file/--blocked-by/--linked-github/--source";
    match (&args.from_file, &args.batch) {
        (Some(_), Some(_)) => Err(CliError::usage(
            "--from-file and --batch are mutually exclusive",
        )),
        (Some(path), None) => {
            if single_flags_set {
                return Err(CliError::usage(format!("--from-file {CONFLICT}")));
            }
            new_from_file(ctx, path)
        }
        (None, Some(path)) => {
            if single_flags_set {
                return Err(CliError::usage(format!("--batch {CONFLICT}")));
            }
            new_batch(ctx, path)
        }
        (None, None) => new_single(ctx, args),
    }
}

fn new_single(ctx: &Ctx<'_>, args: NewArgs) -> Result<()> {
    let title = non_empty(
        "--title",
        args.title
            .as_deref()
            .ok_or_else(|| CliError::usage("--title is required (or use --from-file / --batch)"))?,
    )?;
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
    let description = read_description(args.description, args.description_file)?;
    let source = args.source.as_deref().map(parse_source_flag).transpose()?;
    let linked_github = args
        .linked_github
        .as_deref()
        .map(|g| non_empty("--linked-github", g))
        .transpose()?;
    let actor = ctx.actor()?;

    let (mut store, ws) = ctx.open()?;
    let ticket = file_ticket(
        &mut store,
        &ws,
        ctx.env,
        &actor,
        NewTicket {
            title,
            project,
            repo,
            priority: args.priority.unwrap_or_default(),
            labels,
            description,
            blocked_by: args.blocked_by,
            source,
            linked_github,
        },
    )?;
    print_created_ticket(ctx, &store, &ws, &ticket)
}

/// One ticket to file, its flags already validated (trimmed, non-empty).
pub(crate) struct NewTicket {
    pub title: String,
    pub project: Option<String>,
    pub repo: Option<String>,
    pub priority: Priority,
    pub labels: BTreeSet<String>,
    pub description: Option<String>,
    /// Blocker refs as given (display ids or ULIDs), resolved here.
    pub blocked_by: Vec<String>,
    pub source: Option<Source>,
    pub linked_github: Option<String>,
}

/// Files one ticket exactly as plain `pm new` does — the initial state,
/// the project checked before any op, blockers resolved, the op set
/// [`build_create_ops`] builds, numbered by [`commit_new`] (locally, or
/// left pending when a hub is configured) — and returns it. `pub(crate)`:
/// `pm app`'s `POST /tickets` (AGT-1405) files through this same path.
pub(crate) fn file_ticket(
    store: &mut Store,
    ws: &Workspace,
    env: &Env,
    actor: &ActorId,
    spec: NewTicket,
) -> Result<Ticket> {
    let state = initial_state(ws)?;
    // Checked up front so a missing project fails before any op lands
    // (the store would reject the create too, as R2).
    if let Some(project) = &spec.project {
        require_project(store, project)?;
    }
    let blocked_by: Vec<Ulid> = spec
        .blocked_by
        .iter()
        .map(|r| find(store, ws, r).map(|t| t.id))
        .collect::<Result<_>>()?;

    let id = Ulid::new();
    let mut stamper = Stamper::new(store, actor.clone())?;
    let ops = build_create_ops(
        &mut stamper,
        id,
        &state,
        spec.title,
        spec.priority,
        spec.project,
        spec.repo,
        spec.source,
        Default::default(),
        spec.labels,
        blocked_by,
        spec.description,
        spec.linked_github,
        None,
    )?;
    let tickets = commit_new(store, env, &ops, &[id], actor)?;
    tickets
        .into_iter()
        .next()
        .ok_or_else(|| CliError::error(format!("ticket {id} vanished after create")))
}

/// `pm new --from-file <path>` (AC1): a vault-format ticket file
/// (frontmatter + sections) becomes one create op set. Unknown frontmatter
/// keys land in `ext`; the body (everything after the frontmatter) becomes
/// the description verbatim.
fn new_from_file(ctx: &Ctx<'_>, path: &Path) -> Result<()> {
    let (fm, body) = batch::load_frontmatter(path)?;
    let title = non_empty("title (frontmatter)", fm.title.as_deref().unwrap_or(""))?;
    let project = fm
        .project
        .as_deref()
        .map(|p| non_empty("project (frontmatter)", p))
        .transpose()?;
    let repo = fm
        .repo
        .as_deref()
        .map(|r| non_empty("repo (frontmatter)", r))
        .transpose()?;
    let labels: BTreeSet<String> = fm
        .labels
        .iter()
        .map(|l| non_empty("labels (frontmatter)", l))
        .collect::<Result<_>>()?;
    let linked_github = fm.linked_github.filter(|s| !s.trim().is_empty());
    let linked_pr = fm.linked_pr.filter(|s| !s.trim().is_empty());
    let source = fm.source.map(SourceFm::into_source);
    let ext = batch::ext_to_json(fm.ext)?;
    let actor = ctx.actor()?;

    let (mut store, ws) = ctx.open()?;
    let state = initial_state(&ws)?;
    if let Some(project) = &project {
        require_project(&store, project)?;
    }
    let blocked_by: Vec<Ulid> = fm
        .blocked_by
        .iter()
        .map(|r| find(&store, &ws, r).map(|t| t.id))
        .collect::<Result<_>>()?;

    let id = Ulid::new();
    let mut stamper = Stamper::new(&store, actor.clone())?;
    let description = (!body.is_empty()).then_some(body);
    let ops = build_create_ops(
        &mut stamper,
        id,
        &state,
        title,
        fm.priority.unwrap_or_default(),
        project,
        repo,
        source,
        ext,
        labels,
        blocked_by,
        description,
        linked_github,
        linked_pr,
    )?;
    let tickets = commit_new(&mut store, ctx.env, &ops, &[id], &actor)?;
    let ticket = tickets
        .into_iter()
        .next()
        .ok_or_else(|| CliError::error(format!("ticket {id} vanished after create")))?;
    print_created_ticket(ctx, &store, &ws, &ticket)
}

/// One batch entry after every field has been validated and every
/// `blocked-by` reference resolved — the point past which nothing can
/// fail before the transaction opens (AC2: a batch is all-or-nothing).
struct ValidatedEntry {
    id: Ulid,
    title: String,
    project: Option<String>,
    repo: Option<String>,
    priority: Priority,
    labels: BTreeSet<String>,
    blocked_by: Vec<Ulid>,
    description: Option<String>,
    linked_github: Option<String>,
    linked_pr: Option<String>,
    source: Option<Source>,
    ext: BTreeMap<String, Value>,
}

fn batch_entry_label(index: usize, entry: &BatchEntry) -> String {
    match &entry.ticket_ref {
        Some(r) => format!("ref '@{r}'"),
        None => format!("entry {}", index + 1),
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_batch_entry(
    store: &Store,
    ws: &Workspace,
    refs: &BTreeMap<String, Ulid>,
    id: Ulid,
    label: &str,
    entry: &BatchEntry,
) -> Result<ValidatedEntry> {
    let title = non_empty(&format!("{label}: title"), &entry.title)?;
    let project = entry
        .project
        .as_deref()
        .map(|p| non_empty(&format!("{label}: project"), p))
        .transpose()?;
    if let Some(project) = &project {
        require_project(store, project)?;
    }
    let repo = entry
        .repo
        .as_deref()
        .map(|r| non_empty(&format!("{label}: repo"), r))
        .transpose()?;
    let labels: BTreeSet<String> = entry
        .labels
        .iter()
        .map(|l| non_empty(&format!("{label}: label"), l))
        .collect::<Result<_>>()?;
    let blocked_by: Vec<Ulid> = entry
        .blocked_by
        .iter()
        .map(|r| batch::resolve_blocker(store, ws, refs, r))
        .collect::<Result<_>>()?;
    let linked_github = entry.linked_github.clone().filter(|s| !s.trim().is_empty());
    let linked_pr = entry.linked_pr.clone().filter(|s| !s.trim().is_empty());
    let source = entry.source.clone().map(SourceFm::into_source);
    let ext = batch::ext_to_json(entry.ext.clone())?;
    Ok(ValidatedEntry {
        id,
        title,
        project,
        repo,
        priority: entry.priority.unwrap_or_default(),
        labels,
        blocked_by,
        description: entry.description.clone(),
        linked_github,
        linked_pr,
        source,
        ext,
    })
}

/// `pm new --batch <path>` (AC2-4): mints every id up front so
/// `blocked-by: [@ref]` can point anywhere in the file, validates every
/// entry (so a bad `@ref` fails before any op is built), then commits the
/// whole file in one transaction.
fn new_batch(ctx: &Ctx<'_>, path: &Path) -> Result<()> {
    let file = batch::load_batch_file(path)?;
    if file.tickets.is_empty() {
        return Err(CliError::usage(format!(
            "batch file {} has no tickets",
            path.display()
        )));
    }

    let ids: Vec<Ulid> = file.tickets.iter().map(|_| Ulid::new()).collect();
    let mut refs: BTreeMap<String, Ulid> = BTreeMap::new();
    for (entry, id) in file.tickets.iter().zip(&ids) {
        if let Some(name) = &entry.ticket_ref {
            let name = name.trim_start_matches('@').trim().to_string();
            if name.is_empty() {
                return Err(CliError::usage("a batch entry's `ref` must not be empty"));
            }
            if refs.insert(name.clone(), *id).is_some() {
                return Err(CliError::usage(format!(
                    "duplicate ref '@{name}' in {}",
                    path.display()
                )));
            }
        }
    }

    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let state = initial_state(&ws)?;

    // Every entry is validated — including every `blocked-by` ref — before
    // any op is built, so a single bad entry never leaves a partial batch
    // (AC4: exit 2 naming the ref, nothing created).
    let validated: Vec<ValidatedEntry> = file
        .tickets
        .iter()
        .zip(&ids)
        .enumerate()
        .map(|(i, (entry, id))| {
            let label = batch_entry_label(i, entry);
            validate_batch_entry(&store, &ws, &refs, *id, &label, entry)
        })
        .collect::<Result<_>>()?;

    let mut stamper = Stamper::new(&store, actor.clone())?;
    let mut ops = Vec::new();
    let mut created = Vec::with_capacity(validated.len());
    for v in validated {
        created.push(v.id);
        ops.extend(build_create_ops(
            &mut stamper,
            v.id,
            &state,
            v.title,
            v.priority,
            v.project,
            v.repo,
            v.source,
            v.ext,
            v.labels,
            v.blocked_by,
            v.description,
            v.linked_github,
            v.linked_pr,
        )?);
    }

    // One transaction across every ticket's ops and number allocation (or
    // pending flag) (AC2): a failure here creates nothing.
    let tickets = commit_new(&mut store, ctx.env, &ops, &created, &actor)?;
    print_batch_result(ctx, &store, &ws, &refs, &tickets)
}

fn print_batch_result(
    ctx: &Ctx<'_>,
    store: &Store,
    ws: &Workspace,
    refs: &BTreeMap<String, Ulid>,
    tickets: &[Ticket],
) -> Result<()> {
    // A ref resolves to something `find` accepts: the display id, or the
    // ULID while the number is still the hub's to give (AC2).
    let shown = |id: Ulid| -> String {
        tickets
            .iter()
            .find(|t| t.id == id)
            .map(|t| ref_id(ws, t))
            .unwrap_or_else(|| id.to_string())
    };
    if ctx.json {
        let mut refs_json = Map::new();
        for (name, id) in refs {
            refs_json.insert(format!("@{name}"), json!(shown(*id)));
        }
        let mut out = Vec::with_capacity(tickets.len());
        for t in tickets {
            out.push(ticket_json(ws, store, t)?);
        }
        print_json(&json!({
            "schema": SCHEMA,
            "refs": Value::Object(refs_json),
            "tickets": out,
        }));
    } else {
        for t in tickets {
            println!("{}  {}", created_line(ws, t), crate::text::inline(&t.title));
        }
        if !refs.is_empty() {
            println!("refs:");
            for (name, id) in refs {
                println!("  @{name} -> {}", shown(*id));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- pm show

/// `AGT-12`, or `AGT-?` before the authority numbers it. `pub(crate)`:
/// `crate::read` prints display ids for `pm list`/`pm graph` too.
pub(crate) fn display_id(ws: &Workspace, t: &Ticket) -> String {
    // The prefix comes from a synced `workspace.set`, so it is scrubbed
    // like every other printed field (AGT-1468).
    let prefix = crate::text::inline(&ws.prefix);
    match t.number {
        Some(n) => format!("{prefix}-{n}"),
        None => format!("{prefix}-?"),
    }
}

/// A *reference* to `t`: the string to hand back to `pm` to name it
/// again. `AGT-12` once numbered; the ULID while the number is pending
/// (AGT-1398), since `AGT-?` names nothing. Every place output points at
/// another ticket by id alone — `blocked_by`, `pm new --batch`'s `refs`,
/// `pm graph`/`pm ready` waves, `pm check` findings — uses this;
/// [`display_id`] stays the label on a ticket's own row.
pub(crate) fn ref_id(ws: &Workspace, t: &Ticket) -> String {
    match t.number {
        Some(_) => display_id(ws, t),
        None => t.id.to_string(),
    }
}

/// A ticket named on the command line: `<PREFIX>-<n>` or its ULID.
pub(crate) fn find(store: &Store, ws: &Workspace, reference: &str) -> Result<Ticket> {
    let r = reference.trim();
    if let Some(prefix) = r.strip_suffix("-?")
        && prefix.eq_ignore_ascii_case(&ws.prefix)
    {
        return Err(CliError::usage(format!(
            "'{r}' is a ticket still awaiting its number from the hub: name it by its ULID \
             (`pm list --json` shows `ulid`) until `pm sync` numbers it"
        )));
    }
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
/// `id` the human id (`AGT-12`, or `AGT-?` while the hub's number is
/// pending) and the ULID under `ulid`, plus `schema` and `blocked_by`
/// (the tickets that block this one, by [`ref_id`] — AC5: "`pm show
/// --json` reflects all" the flags `pm new` accepts, including
/// `--blocked-by`). `pub(crate)`: `crate::read` reuses this for `pm list
/// --json`.
pub(crate) fn ticket_json(ws: &Workspace, store: &Store, t: &Ticket) -> Result<Value> {
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
    let blocked_by = store
        .relations(t.id)?
        .into_iter()
        .filter(|r| r.kind == RelationKind::Blocks && r.to == t.id)
        .map(|r| {
            Ok(match store.ticket(r.from)? {
                Some(other) => ref_id(ws, &other),
                None => r.from.to_string(),
            })
        })
        .collect::<Result<Vec<String>>>()?;
    out.insert("blocked_by".into(), json!(blocked_by));
    Ok(Value::Object(out))
}

/// [`ticket_json`] plus `comments: [{author, at, body}]`, oldest first
/// (`at` is the UTC `YYYY-MM-DD` date, as in `pm export md`). Only `pm
/// show` uses this: every other Ticket-shaped output (list items,
/// mutating-verb echoes) stays comment-free so `pm list --json` is light
/// (AGT-1430).
pub(crate) fn ticket_json_with_comments(
    ws: &Workspace,
    store: &Store,
    t: &Ticket,
) -> Result<Value> {
    let mut value = ticket_json(ws, store, t)?;
    let comments: Vec<Value> = store
        .comments(t.id)?
        .into_iter()
        .map(|c| {
            json!({
                "author": c.author.as_str(),
                "at": pm_core::markers::date_from_ms(c.hlc.wall_ms),
                "body": c.body,
            })
        })
        .collect();
    value["comments"] = json!(comments);
    Ok(value)
}

fn print_comments(store: &Store, t: &Ticket) -> Result<()> {
    let comments = store.comments(t.id)?;
    if comments.is_empty() {
        return Ok(());
    }
    println!();
    println!("Comments ({}):", comments.len());
    for c in comments {
        println!();
        println!(
            "{} — {}",
            pm_core::markers::date_from_ms(c.hlc.wall_ms),
            crate::text::inline(c.author.as_str())
        );
        for line in crate::text::printable(c.body.trim_end()).lines() {
            if line.is_empty() {
                println!();
            } else {
                println!("  {line}");
            }
        }
    }
    Ok(())
}

fn print_human(ws: &Workspace, t: &Ticket) {
    let dash = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
    println!("{}  {}", display_id(ws, t), crate::text::inline(&t.title));
    println!("state:     {}", crate::text::inline(&t.state));
    println!(
        "priority:  {}",
        serde_json::to_value(t.priority)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    );
    println!("project:   {}", crate::text::inline(&dash(&t.project)));
    println!("repo:      {}", crate::text::inline(&dash(&t.repo)));
    println!(
        "assignee:  {}",
        t.assignee
            .as_ref()
            .map_or_else(|| "-".into(), |a| crate::text::inline(a.as_str()))
    );
    let labels: Vec<&str> = t.labels.iter().map(String::as_str).collect();
    println!(
        "labels:    {}",
        if labels.is_empty() {
            "-".into()
        } else {
            crate::text::inline(&labels.join(", "))
        }
    );
    for (name, value) in [
        ("linked-github", &t.linked_github),
        ("linked-pr", &t.linked_pr),
        ("linear", &t.linear),
    ] {
        if let Some(value) = value {
            println!("{name}: {}", crate::text::inline(value));
        }
    }
    if t.deleted {
        println!("deleted:   true");
    }
    println!("ulid:      {}", t.id);
    crate::markers::print_block(t);
    if !t.description.is_empty() {
        println!();
        print!("{}", crate::text::printable(&t.description));
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
        Value::String(s) => println!("{}", crate::text::printable(s)),
        Value::Null => println!(),
        Value::Array(items) if items.iter().all(Value::is_string) => {
            for item in items {
                println!(
                    "{}",
                    crate::text::printable(item.as_str().unwrap_or_default())
                );
            }
        }
        other => println!("{}", crate::text::printable(&other.to_string())),
    }
    Ok(())
}

/// `--field` and `--section` (AGT-1339 AC2) are mutually exclusive: each
/// prints exactly one thing, and combining them would leave one silently
/// ignored.
pub fn show(
    ctx: &Ctx<'_>,
    reference: &str,
    field: Option<&str>,
    section: Option<&str>,
) -> Result<()> {
    if field.is_some() && section.is_some() {
        return Err(CliError::usage(
            "--field and --section are mutually exclusive",
        ));
    }
    let (store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    if let Some(section) = section {
        return crate::read::print_section(ctx, &ticket, section);
    }
    match field {
        Some(field) => print_field(
            ctx,
            &ticket_json_with_comments(&ws, &store, &ticket)?,
            field,
        )?,
        None if ctx.json => print_json(&ticket_json_with_comments(&ws, &store, &ticket)?),
        None => {
            print_human(&ws, &ticket);
            print_comments(&store, &ticket)?;
        }
    }
    Ok(())
}

// ----------------------------------------------------------------- pm set

/// Parses one `key=value`. Empty values clear optional fields. A key this
/// crate does not know as a scalar field is not an error (AC1): it lands in
/// `ext` under that key, with a warning to stderr so a typo is still
/// noticeable. `pub(crate)`: `pm app`'s field endpoint (AGT-1401) parses
/// the same `key=value` grammar so the API and the CLI agree on it.
pub(crate) fn parse_assignment(assignment: &str) -> Result<FieldSet> {
    let Some((key, value)) = assignment.split_once('=') else {
        return Err(CliError::usage(format!(
            "'{assignment}' is not key=value (e.g. title=\"New title\")"
        )));
    };
    let optional = |v: &str| {
        let v = v.trim();
        (!v.is_empty()).then(|| v.to_string())
    };
    let key = key.trim();
    match key {
        "title" => Ok(FieldSet::Title(non_empty("title", value)?)),
        "priority" => Ok(FieldSet::Priority(
            parse_priority(value.trim()).map_err(CliError::usage)?,
        )),
        "project" => Ok(FieldSet::Project(optional(value))),
        "repo" => Ok(FieldSet::Repo(optional(value))),
        "assignee" => Ok(FieldSet::Assignee(optional(value).map(ActorId::new))),
        "linked-github" => Ok(FieldSet::LinkedGithub(optional(value))),
        "linked-pr" => Ok(FieldSet::LinkedPr(optional(value))),
        "linear" => Ok(FieldSet::Linear(optional(value))),
        "not_before" | "not-before" => Ok(FieldSet::NotBefore(
            optional(value)
                .map(|v| pm_core::markers::parse_not_before(&v))
                .transpose()?,
        )),
        "parked" => Ok(FieldSet::Parked(
            optional(value)
                .map(|v| pm_core::markers::parse_parked(&v))
                .transpose()?,
        )),
        other => {
            eprintln!("pm: warning: unknown field '{other}'; stored under ext.{other}");
            Ok(FieldSet::Ext {
                key: other.to_string(),
                value: optional(value).map(Value::String),
            })
        }
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
        print_json(&ticket_json(&ws, &store, &ticket)?);
    } else {
        println!("{}", display_id(&ws, &ticket));
    }
    Ok(())
}

#[cfg(test)]
mod when_tests {
    use super::*;
    use pm_core::Hlc;

    #[test]
    fn renders_utc_date_time_without_the_counter() {
        // 2026-09-30 12:04:10.142 UTC
        let hlc = Hlc::new(1_790_769_850_142, 7);
        assert_eq!(when(&hlc), "2026-09-30 12:04 UTC");
        assert_eq!(when_secs(&hlc), "2026-09-30 12:04:10 UTC");
        assert_eq!(when(&Hlc::new(0, 0)), "1970-01-01 00:00 UTC");
        assert_eq!(
            when_secs(&Hlc::new(86_399_999, 0)),
            "1970-01-01 23:59:59 UTC"
        );
    }
}
