//! `pm edit <id>` (projects/pm/README.md §Surfaces, AGT-1345): open the
//! ticket in `$EDITOR` as frontmatter + markdown and diff the save into ops.
//!
//! The flow:
//! 1. Read the ticket's [`TicketView`] once, at open time. Everything the
//!    save is diffed against comes from this read: the rendered file, the
//!    label add-tags a `label.remove` cites, and the body's Loro state.
//! 2. Render it ([`render`]), parse the render back ([`parse`]) as the
//!    baseline, and hand the file to the editor.
//! 3. Parse the save. A parse/validation error re-opens the editor with the
//!    error in a `#` comment header above the frontmatter; the editor
//!    exiting non-zero, or the error file coming back unchanged, is an abort
//!    (exit 1, zero ops).
//! 4. [`plan`] the difference between the two parses: a `field.set` per
//!    changed scalar or `ext` key, `label.add`/`label.remove` per label, and
//!    one `body.edit` when the markdown changed. An unchanged save plans
//!    nothing and commits nothing. Everything else lands in one
//!    `commit_batch`.
//!
//! **Body edits are CRDT updates, not replacements.** The `body.edit` is
//! produced by a [`Body`] rebuilt from the open-time Loro snapshot, then
//! `diff_from_text(saved)`. Because the update is relative to the state the
//! user actually edited, anything that landed while the editor was open (a
//! concurrent `pm edit`, a synced edit from another machine) merges instead
//! of being reverted.
//!
//! **Peer ids.** Loro identifies every text op by `(peer, counter)` and
//! requires that no two replicas editing concurrently share a peer: two
//! edits minted under the same peer from the same base get the same ids and
//! each replica silently drops the other's as a duplicate, so they never
//! converge. A peer that is stable per *actor* is therefore unsafe — `matt`
//! can run two `pm edit`s at once, or edit on two machines before a sync.
//! Each edit session instead gets its own peer, [`session_peer`]: the low 64
//! bits of a fresh ULID's random component, kept clear of the view's fixed
//! peer 0 and of `u64::MAX` (which Loro reserves). Attribution does not need
//! the peer: the `body.edit` op carries the actor. The cost is one extra
//! version-vector entry per edit session, which for ticket bodies (a handful
//! of edits each) is a few bytes.
//!
//! **Which editor** (AGT-1402, decision 6): `--view`, then `edit.view` in
//! config.toml, else ui-leaf — the default only when stdin and stdout are
//! terminals (a non-interactive `pm edit` that did not ask for ui-leaf is
//! the `$EDITOR` flow, silently). ui-leaf opens the ticket view through
//! `pm app`'s launcher ([`crate::app::launch`]) and `pm edit` returns when
//! its window closes — every change it made is already an op. Without a
//! display, without a pinned ui-leaf, or with `editor` chosen, it is the
//! `$EDITOR` flow below, unchanged; a missing ui-leaf (or asking for ui-leaf
//! on a headless session) says so in one line on stderr first.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Context;
use pm_core::op::{BodyEdit, FieldSet, LabelAdd, LabelRemove};
use pm_core::{ActorId, Body, BodyState, Payload, Priority, Source, TicketView, Workspace};
use pm_store::Store;
use serde_json::Value;
use serde_json::Value as Yaml;
use ulid::Ulid;

use crate::app::{
    self,
    launch::{self, Choice, Ended},
};
use crate::batch;
use crate::exit::{CliError, Result};
use crate::verbs::{Ctx, Stamper, display_id, find, print_json, ref_id, ticket_json};
use crate::workspace::{Config, Env};

/// How many times a save that does not parse re-opens the editor before
/// `pm edit` gives up. A human never gets near it; it bounds a scripted
/// `$EDITOR` that keeps writing a broken file, so the loop cannot hang.
const MAX_REOPENS: usize = 10;

/// The frontmatter keys `pm edit` renders and reads back; an `ext` key of
/// the same name is not rendered (it would shadow the real field).
const KNOWN_KEYS: &[&str] = &[
    "title",
    "priority",
    "project",
    "repo",
    "assignee",
    "labels",
    "linked-github",
    "linked-pr",
    "linear",
    "source",
    // Read by `batch::FileFrontmatter` itself, so never editable here.
    "id",
    "state",
    "created",
    "updated",
    "blocked-by",
];

// ------------------------------------------------------------------- view

/// Which editor `pm edit` opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    /// `$EDITOR` on a frontmatter + markdown temp file.
    Editor,
    /// The ui-leaf ticket view (AGT-1402).
    UiLeaf,
}

/// Clap value parser for `--view`.
pub fn parse_view(s: &str) -> std::result::Result<View, String> {
    match s.trim() {
        "editor" => Ok(View::Editor),
        "ui-leaf" => Ok(View::UiLeaf),
        other => Err(format!(
            "unknown view '{other}': expected editor or ui-leaf"
        )),
    }
}

/// `edit.view` from config.toml, if set; a value other than `editor` or
/// `ui-leaf` is a usage error naming the file.
fn configured_view(env: &Env) -> Result<Option<View>> {
    let path = env.config_path()?;
    let Some(view) = Config::load(&path)?
        .and_then(|c| c.edit)
        .and_then(|e| e.view)
    else {
        return Ok(None);
    };
    parse_view(&view)
        .map(Some)
        .map_err(|e| CliError::usage(format!("{}: edit.view: {e}", path.display())))
}

/// `--view`, then config's `edit.view`, else ui-leaf — and whether the
/// choice was explicit (a flag or config), which decides whether a
/// headless fallback is worth a note. `pub(crate)`: `pm project edit`
/// (AGT-1405) chooses between the project view and `$EDITOR` the same way.
pub(crate) fn resolve_view(flag: Option<View>, env: &Env) -> Result<(View, bool)> {
    if let Some(view) = flag {
        return Ok((view, true));
    }
    if let Some(view) = configured_view(env)? {
        return Ok((view, true));
    }
    Ok((View::UiLeaf, false))
}

/// Whether ui-leaf may open at all: always when it was asked for (flag or
/// config); as the *default* only for an interactive invocation (stdin and
/// stdout both a terminal). A scripted `pm edit` — an agent with a scripted
/// `$EDITOR`, no TTY — must never pop a window on someone's screen and block
/// on it, so it gets the `$EDITOR` flow, silently.
fn default_may_launch(explicit: bool, interactive: bool) -> bool {
    explicit || interactive
}

/// The ui-leaf runtime to open a view in, or `None` for the `$EDITOR`
/// flow: [`default_may_launch`] for this invocation's terminals, then
/// [`launch::choose`], printing its fallback note (if any) as one stderr
/// line prefixed with `command`. Shared by `pm edit` and `pm project edit`.
pub(crate) fn ui_leaf_runtime(
    ctx: &Ctx<'_>,
    explicit: bool,
    command: &str,
) -> Result<Option<launch::Runtime>> {
    use std::io::IsTerminal as _;
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !default_may_launch(explicit, interactive) {
        return Ok(None);
    }
    match launch::choose(ctx.env, explicit)? {
        Choice::Launch(runtime) => Ok(Some(runtime)),
        Choice::Fallback(note) => {
            if let Some(note) = note {
                eprintln!(
                    "{command}: {note}; using $EDITOR (set edit.view = \"editor\" to skip ui-leaf)"
                );
            }
            Ok(None)
        }
    }
}

/// The ui-leaf path of `pm edit`: `Ok(true)` when the view opened and has
/// closed (the command is done), `Ok(false)` to continue with `$EDITOR`.
fn edit_in_ui_leaf(ctx: &Ctx<'_>, reference: &str, explicit: bool) -> Result<bool> {
    let Some(runtime) = ui_leaf_runtime(ctx, explicit, "pm")? else {
        return Ok(false);
    };
    // Resolve the ticket first: a bad id is exit 3 before any window.
    let (store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let shown = display_id(&ws, &ticket);
    // The view names the ticket to the API: the ULID while its number is
    // pending, since `AGT-?` names nothing.
    let reference = ref_id(&ws, &ticket);
    drop(store);
    match app::edit_ticket(ctx, runtime, &reference)? {
        Ended::Closed => {}
        Ended::Failed(why) => {
            eprintln!("pm: ui-leaf could not open {shown} ({why}); using $EDITOR");
            return Ok(false);
        }
    }
    let (store, ws) = ctx.open()?;
    let ticket = store
        .ticket(ticket.id)?
        .ok_or_else(|| CliError::not_found(format!("no ticket {shown}")))?;
    if ctx.json {
        print_json(&ticket_json(&ws, &store, &ticket)?);
    } else {
        println!("{shown}");
    }
    Ok(true)
}

// --------------------------------------------------------------- the file

/// A ticket as the editor file expresses it: every editable field, already
/// normalized (trimmed, blanks as `None`), so two parses compare equal
/// exactly when the user changed nothing that matters.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Parsed {
    title: String,
    priority: Priority,
    project: Option<String>,
    repo: Option<String>,
    assignee: Option<String>,
    labels: BTreeSet<String>,
    linked_github: Option<String>,
    linked_pr: Option<String>,
    linear: Option<String>,
    source: Option<Source>,
    ext: BTreeMap<String, Value>,
    body: String,
}

/// Frontmatter keys in render order. `serde_json::Map` sorts its keys, so
/// the editor file's field order lives in this vector and serializes as a
/// map in that order.
#[derive(Default)]
struct Fields(Vec<(String, Yaml)>);

impl serde::Serialize for Fields {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in &self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

fn yaml_opt(value: &Option<String>) -> Yaml {
    value.clone().map_or(Yaml::Null, Yaml::String)
}

/// The editor file for `view`: a `#` comment header (read-only facts and
/// instructions), then frontmatter, then the description.
pub(crate) fn render(ws: &Workspace, view: &TicketView) -> Result<String> {
    let t = view.snapshot();
    let mut fm = Fields::default();
    let mut put = |k: &str, v: Yaml| fm.0.push((k.to_string(), v));
    put("title", Yaml::String(t.title.clone()));
    put(
        "priority",
        serde_json::to_value(t.priority).context("rendering priority")?,
    );
    put("project", yaml_opt(&t.project));
    put("repo", yaml_opt(&t.repo));
    put(
        "assignee",
        yaml_opt(&t.assignee.as_ref().map(ActorId::to_string)),
    );
    put(
        "labels",
        Yaml::Array(t.labels.iter().cloned().map(Yaml::String).collect()),
    );
    put("linked-github", yaml_opt(&t.linked_github));
    put("linked-pr", yaml_opt(&t.linked_pr));
    put("linear", yaml_opt(&t.linear));
    if let Some(source) = &t.source {
        let mut m = serde_json::Map::new();
        for (k, v) in [
            ("type", &source.kind),
            ("url", &source.url),
            ("id", &source.id),
            ("fetched-at", &source.fetched_at),
        ] {
            m.insert(k.into(), Yaml::String(v.clone()));
        }
        put("source", Yaml::Object(m));
    }
    for (key, value) in &t.ext {
        if KNOWN_KEYS.contains(&key.as_str()) {
            continue;
        }
        put(key, value.clone());
    }
    let yaml = serde_saphyr::to_string(&fm).context("rendering frontmatter")?;
    // `key: null` reads as noise in an editor; a bare `key:` parses the
    // same. Only top-level lines (no indent) are touched.
    let yaml: String = yaml
        .lines()
        .map(|line| match line.strip_suffix(": null") {
            Some(key) if !line.starts_with([' ', '-']) => format!("{key}:\n"),
            _ => format!("{line}\n"),
        })
        .collect();
    let state = &t.state;
    Ok(format!(
        "# pm edit {id} (state: {state}; use `pm move` to change it)\n\
         # Save to apply; quit without changes to leave the ticket as it is.\n\
         ---\n{yaml}---\n\n{body}\n",
        id = display_id(ws, &t),
        body = t.description.trim(),
    ))
}

/// Drops the leading `#` comment header (and blank lines) that [`render`]
/// and the error re-open put above the opening fence.
fn strip_header(text: &str) -> &str {
    let mut rest = text;
    loop {
        let (line, tail) = rest.split_once('\n').unwrap_or((rest, ""));
        if line.starts_with('#') || line.trim().is_empty() {
            if tail.is_empty() && line == rest {
                return "";
            }
            rest = tail;
        } else {
            return rest;
        }
    }
}

fn opt(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Parses an editor file (with or without the header) into [`Parsed`].
/// Every failure is a message for the error header, not a process exit.
pub(crate) fn parse(text: &str) -> std::result::Result<Parsed, String> {
    let (mut fm, body) =
        batch::parse_frontmatter(strip_header(text)).map_err(|e| format!("{:#}", e.error))?;
    if !fm.blocked_by.is_empty() {
        return Err("blocked-by cannot be edited here".into());
    }
    let title = opt(fm.title.take()).ok_or("title must not be empty")?;
    let priority = fm
        .priority
        .ok_or("priority is required: one of low, medium, high, critical")?;
    // `assignee` and `linear` are ticket fields `FileFrontmatter` has no
    // slot for, so they arrive in its catch-all `ext`.
    let mut take_str = |key: &str| -> std::result::Result<Option<String>, String> {
        match fm.ext.remove(key) {
            None | Some(Yaml::Null) => Ok(None),
            Some(Yaml::String(s)) => Ok(opt(Some(s))),
            Some(other) => Err(format!("{key} must be a string, not {other:?}")),
        }
    };
    let assignee = take_str("assignee")?;
    let linear = take_str("linear")?;
    let labels = fm
        .labels
        .iter()
        .map(|l| {
            let l = l.trim();
            if l.is_empty() {
                Err("labels must not be empty".to_string())
            } else {
                Ok(l.to_string())
            }
        })
        .collect::<std::result::Result<_, _>>()?;
    let ext = batch::ext_to_json(fm.ext)
        .map_err(|e| format!("{:#}", e.error))?
        .into_iter()
        .filter(|(_, v)| !v.is_null())
        .collect();
    Ok(Parsed {
        title,
        priority,
        project: opt(fm.project),
        repo: opt(fm.repo),
        assignee,
        labels,
        linked_github: opt(fm.linked_github),
        linked_pr: opt(fm.linked_pr),
        linear,
        source: fm.source.map(batch::SourceFm::into_source),
        ext,
        body,
    })
}

// ------------------------------------------------------------------- diff

/// The Loro peer for one edit session (see the module docs for why it is
/// per session rather than per actor): the low 64 bits of `session`'s
/// random component, moved off 0 (the view's peer) and `u64::MAX` (Loro's
/// reserved id).
pub(crate) fn session_peer(session: Ulid) -> u64 {
    let peer = session.random() as u64;
    match peer {
        0 => 1,
        u64::MAX => u64::MAX - 1,
        p => p,
    }
}

/// The ops that turn `before` into `after`, relative to `view` (the
/// open-time view both were read from). Empty when nothing changed.
pub(crate) fn plan(
    view: &TicketView,
    before: &Parsed,
    after: &Parsed,
    peer: u64,
) -> Result<Vec<Payload>> {
    let mut out = Vec::new();
    let mut field = |changed: bool, f: FieldSet| {
        if changed {
            out.push(Payload::FieldSet(f));
        }
    };
    field(
        before.title != after.title,
        FieldSet::Title(after.title.clone()),
    );
    field(
        before.priority != after.priority,
        FieldSet::Priority(after.priority),
    );
    field(
        before.project != after.project,
        FieldSet::Project(after.project.clone()),
    );
    field(
        before.repo != after.repo,
        FieldSet::Repo(after.repo.clone()),
    );
    field(
        before.assignee != after.assignee,
        FieldSet::Assignee(after.assignee.clone().map(ActorId::new)),
    );
    field(
        before.linked_github != after.linked_github,
        FieldSet::LinkedGithub(after.linked_github.clone()),
    );
    field(
        before.linked_pr != after.linked_pr,
        FieldSet::LinkedPr(after.linked_pr.clone()),
    );
    field(
        before.linear != after.linear,
        FieldSet::Linear(after.linear.clone()),
    );
    field(
        before.source != after.source,
        FieldSet::Source(after.source.clone()),
    );
    let ext_keys: BTreeSet<&String> = before.ext.keys().chain(after.ext.keys()).collect();
    for key in ext_keys {
        let (old, new) = (before.ext.get(key), after.ext.get(key));
        field(
            old != new,
            FieldSet::Ext {
                key: key.clone(),
                value: new.cloned(),
            },
        );
    }

    for label in after.labels.difference(&before.labels) {
        out.push(Payload::LabelAdd(LabelAdd {
            label: label.clone(),
        }));
    }
    for label in before.labels.difference(&after.labels) {
        // The add-tags seen at open time: a label someone re-adds while
        // the editor is open survives the remove (OR-set, add-wins).
        out.push(Payload::LabelRemove(LabelRemove {
            label: label.clone(),
            observed: view.labels.observed(label),
        }));
    }

    if before.body != after.body {
        out.push(Payload::BodyEdit(BodyEdit {
            update: body_update(&view.body, &after.body, peer)?,
        }));
    }
    Ok(out)
}

/// The `body.edit` update that turns `state` (a ticket's description or a
/// project document) into `text`, minted under `peer` (one fresh
/// [`session_peer`] per editing session — see the module docs): a [`Body`]
/// rebuilt from the state's Loro snapshot, then `diff_from_text`, so the
/// update is relative to the state that was edited and merges with
/// anything that landed meanwhile. `pub(crate)`: `pm app`'s body endpoints
/// (AGT-1401, AGT-1405) take whole text the same way.
pub(crate) fn body_update(state: &BodyState, text: &str, peer: u64) -> Result<Vec<u8>> {
    let body_err = |e: pm_core::BodyError| CliError::error(format!("editing body: {e}"));
    let mut body = Body::with_peer(peer).map_err(body_err)?;
    body.apply(&state.snapshot().map_err(body_err)?)
        .map_err(body_err)?;
    let update = body.diff_from_text(text).map_err(body_err)?;
    Ok(update.into_bytes())
}

// ----------------------------------------------------------------- editor

/// Runs `$VISUAL`, else `$EDITOR`, else `vi` on `path` through `sh -c`, so
/// a value with arguments (`code --wait`) works. `Ok(false)` is a non-zero
/// exit: an abort. `pub(crate)`: `crate::project`'s `pm project edit`
/// (AGT-1344) opens a document's text the same way a ticket's is opened
/// here, so it reuses this rather than a second editor-launch path.
pub(crate) fn run_editor(path: &Path) -> Result<bool> {
    let editor = ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "vi".to_string());
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("sh")
        .arg(path)
        .status()
        .with_context(|| format!("running editor '{editor}'"))?;
    Ok(status.success())
}

/// The file the editor sees after a save that did not parse: the error in
/// the comment header, then the user's text minus its old header.
fn error_file(ws_id: &str, error: &str, saved: &str) -> String {
    let mut out = format!("# pm edit {ws_id}: could not save:\n");
    for line in error.lines() {
        out.push_str(&format!("#   {line}\n"));
    }
    out.push_str("# Fix it and save to retry; quit without changes to abort.\n");
    out.push_str(strip_header(saved));
    out
}

/// A temp file the editor opens; removed on drop unless kept. `pub(crate)`
/// alongside [`run_editor`], for the same reason.
pub(crate) struct TempFile {
    path: PathBuf,
    keep: bool,
}

impl TempFile {
    pub(crate) fn create(label: &str, contents: &str) -> Result<Self> {
        // The label is a ticket or project id that may have arrived through
        // a synced op: keep it to plain filename characters.
        let label: String = label
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(64)
            .collect();
        let path = std::env::temp_dir().join(format!("pm-edit-{label}-{}.md", Ulid::new()));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options
            .open(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        std::io::Write::write_all(&mut file, contents.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(TempFile { path, keep: false })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, contents: &str) -> Result<()> {
        Ok(fs::write(&self.path, contents)
            .with_context(|| format!("writing {}", self.path.display()))?)
    }

    pub(crate) fn read(&self) -> Result<String> {
        Ok(fs::read_to_string(&self.path)
            .with_context(|| format!("reading {}", self.path.display()))?)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

// ---------------------------------------------------------------- pm edit

/// `pm edit <id> [--view=editor|ui-leaf]`.
pub fn edit(ctx: &Ctx<'_>, reference: &str, view_flag: Option<View>) -> Result<()> {
    let (view, explicit) = resolve_view(view_flag, ctx.env)?;
    if view == View::UiLeaf && edit_in_ui_leaf(ctx, reference, explicit)? {
        return Ok(());
    }
    let actor = ctx.actor()?;
    let (mut store, ws) = ctx.open()?;
    let ticket = find(&store, &ws, reference)?;
    let shown = display_id(&ws, &ticket);
    let view = store
        .ticket_view(ticket.id)?
        .ok_or_else(|| CliError::not_found(format!("no ticket {shown}")))?;

    let original = render(&ws, &view)?;
    let before = parse(&original)
        .map_err(|e| CliError::error(format!("rendering {shown} for editing: {e}")))?;
    let mut file = TempFile::create(&shown, &original)?;

    let mut written = original;
    let mut reopens = 0;
    let after = loop {
        if !run_editor(&file.path)? {
            return Err(abort(&mut file, reopens, "the editor exited non-zero"));
        }
        let saved = file.read()?;
        if reopens > 0 && saved == written {
            return Err(abort(&mut file, reopens, "the file was left unchanged"));
        }
        match parse(&saved).and_then(|p| validate(&store, &p).map(|()| p)) {
            Ok(parsed) => break parsed,
            Err(error) if reopens < MAX_REOPENS => {
                reopens += 1;
                written = error_file(&shown, &error, &saved);
                file.write(&written)?;
            }
            Err(error) => {
                file.keep = true;
                return Err(CliError::error(format!(
                    "{shown}: gave up after {MAX_REOPENS} failed saves: {error}; \
                     nothing was saved (your edits are in {})",
                    file.path.display()
                )));
            }
        }
    };

    let payloads = plan(&view, &before, &after, session_peer(Ulid::new()))?;
    if payloads.is_empty() {
        eprintln!("pm: {shown}: no changes");
    } else {
        // Stamped only now, after the editor closes, so the clock is seeded
        // from ops committed while the editor was open.
        let mut stamper = Stamper::new(&store, actor)?;
        let ops: Vec<_> = payloads
            .into_iter()
            .map(|p| stamper.op(ticket.id, p))
            .collect();
        store.commit_batch(&ops, &[])?;
    }
    let ticket = store
        .ticket(ticket.id)?
        .ok_or_else(|| CliError::error(format!("ticket {shown} vanished after edit")))?;
    if ctx.json {
        print_json(&ticket_json(&ws, &store, &ticket)?);
    } else {
        println!("{shown}");
    }
    Ok(())
}

/// Checks that need the store; a failure re-opens the editor like a parse
/// error does.
fn validate(store: &Store, parsed: &Parsed) -> std::result::Result<(), String> {
    if let Some(project) = &parsed.project {
        match store.project(project) {
            Ok(Some(_)) => {}
            Ok(None) => return Err(format!("project '{project}' does not exist")),
            Err(e) => return Err(format!("looking up project '{project}': {e}")),
        }
    }
    Ok(())
}

/// Exit 1 with nothing saved. After a failed parse the user's text is
/// worth keeping, so the temp file stays and the message says where.
fn abort(file: &mut TempFile, reopens: usize, why: &str) -> CliError {
    if reopens > 0 {
        file.keep = true;
        CliError::error(format!(
            "edit aborted ({why}); nothing was saved (your edits are in {})",
            file.path.display()
        ))
    } else {
        CliError::error(format!("edit aborted ({why}); nothing was saved"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pm_core::op::{StateTransition, TicketCreate};
    use pm_core::{Hlc, Op, State, StateCategory, apply};

    #[test]
    fn the_default_view_needs_a_terminal_an_explicit_one_does_not() {
        assert!(default_may_launch(false, true), "interactive default");
        assert!(!default_may_launch(false, false), "scripted default");
        assert!(default_may_launch(true, false), "asked for, no TTY");
        assert!(default_may_launch(true, true));
    }

    fn ws() -> Workspace {
        Workspace {
            id: Ulid::new(),
            prefix: "AGT".into(),
            states: vec![State {
                name: "triage".into(),
                category: StateCategory::Unstarted,
                position: 0,
            }],
            gate_labels: Default::default(),
            model_labels: Default::default(),
            template_sections: vec![],
            stale_days: 30,
            docs_owned_by: Default::default(),
        }
    }

    fn op(id: Ulid, ms: u64, actor: &str, payload: Payload) -> Op {
        Op::new(
            Ulid::new(),
            Hlc {
                wall_ms: ms,
                counter: 0,
            },
            ActorId::new(actor),
            id,
            payload,
        )
    }

    /// A ticket with a title, labels `a` and `b`, an ext key and a body,
    /// as its log.
    fn base_ops(id: Ulid) -> Vec<Op> {
        let mut body = Body::with_peer(7).unwrap();
        let update = body.diff_from_text("alpha\nbeta\ngamma").unwrap();
        let mut ext = BTreeMap::new();
        ext.insert("team".to_string(), Value::String("eng".into()));
        vec![
            op(
                id,
                1,
                "matt",
                Payload::TicketCreate(TicketCreate {
                    title: "Original".into(),
                    state: "triage".into(),
                    priority: Priority::Medium,
                    project: None,
                    repo: None,
                    source: None,
                    ext,
                }),
            ),
            op(
                id,
                2,
                "matt",
                Payload::LabelAdd(LabelAdd { label: "a".into() }),
            ),
            op(
                id,
                3,
                "matt",
                Payload::LabelAdd(LabelAdd { label: "b".into() }),
            ),
            op(
                id,
                4,
                "matt",
                Payload::BodyEdit(BodyEdit {
                    update: update.into_bytes(),
                }),
            ),
        ]
    }

    fn view_of(id: Ulid, ops: &[Op]) -> TicketView {
        let mut view = TicketView::new(id);
        for op in ops {
            apply(&mut view, op).unwrap();
        }
        view
    }

    /// What the user's editor does: a textual edit of the rendered file.
    fn edited(view: &TicketView, f: impl Fn(String) -> String) -> (Parsed, Parsed) {
        let text = render(&ws(), view).unwrap();
        (parse(&text).unwrap(), parse(&f(text)).unwrap())
    }

    #[test]
    fn render_parses_back_to_the_ticket() {
        let id = Ulid::new();
        let view = view_of(id, &base_ops(id));
        let text = render(&ws(), &view).unwrap();
        assert!(text.starts_with("# pm edit AGT-?"), "{text}");
        let p = parse(&text).unwrap();
        assert_eq!(p.title, "Original");
        assert_eq!(p.labels, BTreeSet::from(["a".into(), "b".into()]));
        assert_eq!(p.ext.get("team"), Some(&Value::String("eng".into())));
        assert_eq!(p.body, "alpha\nbeta\ngamma");
        assert_eq!(p.project, None);
    }

    /// AGT-1465: the serde-saphyr emitter quotes whatever its own parser
    /// would misread, so tricky strings survive render -> parse untouched
    /// and the field order is the editor file's documented order.
    #[test]
    fn tricky_strings_round_trip_through_render() {
        let tricky = [
            "yes",
            "a: b",
            "# hash",
            "\"quoted\"",
            "1",
            "null",
            "x: y: z",
            "it's",
            "trailing:",
            "- dash",
            "@at",
            "é ünï ✓",
            "{brace}",
            "[1, 2]",
            "010",
            "true",
            "~",
            " padded ",
        ];
        for title in tricky {
            let id = Ulid::new();
            let mut ops = base_ops(id);
            let Payload::TicketCreate(c) = &mut ops[0].payload else {
                unreachable!()
            };
            c.title = title.to_string();
            c.ext.insert("note".into(), Value::String(title.into()));
            let text = render(&ws(), &view_of(id, &ops)).unwrap();
            let p = parse(&text).unwrap_or_else(|e| panic!("{title:?}: {e}\n{text}"));
            assert_eq!(p.title, title.trim(), "{text}");
            assert_eq!(
                p.ext.get("note"),
                Some(&Value::String(title.into())),
                "{text}"
            );
        }
        let id = Ulid::new();
        let text = render(&ws(), &view_of(id, &base_ops(id))).unwrap();
        let keys: Vec<&str> = text
            .lines()
            .skip_while(|l| *l != "---")
            .skip(1)
            .take_while(|l| *l != "---")
            .filter(|l| !l.starts_with([' ', '-']))
            .filter_map(|l| l.split(':').next())
            .collect();
        assert_eq!(
            &keys[..4],
            ["title", "priority", "project", "repo"],
            "{text}"
        );
    }

    #[test]
    fn an_unchanged_save_plans_no_ops() {
        let id = Ulid::new();
        let view = view_of(id, &base_ops(id));
        let (before, after) = edited(&view, |t| t);
        assert!(plan(&view, &before, &after, 9).unwrap().is_empty());
        // Header edits and trailing whitespace are not changes either.
        let (before, after) = edited(&view, |t| t.replace("# Save", "# whatever") + "\n\n");
        assert!(plan(&view, &before, &after, 9).unwrap().is_empty());
    }

    #[test]
    fn changes_become_field_label_and_body_ops() {
        let id = Ulid::new();
        let view = view_of(id, &base_ops(id));
        let (before, after) = edited(&view, |t| {
            t.replace("title: Original", "title: Renamed")
                .replace("- b\n", "- c\n")
                .replace("team: eng", "team: ops")
                .replace("beta", "BETA")
        });
        let ops = plan(&view, &before, &after, 9).unwrap();
        let kinds: Vec<&str> = ops.iter().map(Payload::kind).collect();
        assert_eq!(
            kinds,
            [
                "field.set",
                "field.set",
                "label.add",
                "label.remove",
                "body.edit"
            ]
        );
        assert!(ops.contains(&Payload::FieldSet(FieldSet::Title("Renamed".into()))));
        let Payload::LabelRemove(rm) = &ops[3] else {
            panic!()
        };
        assert_eq!(rm.observed, view.labels.observed(&"b".to_string()));
    }

    #[test]
    fn parse_errors_are_messages() {
        assert!(parse("no fences").unwrap_err().contains("frontmatter"));
        assert!(
            parse("---\ntitle: ''\npriority: low\n---\n")
                .unwrap_err()
                .contains("title")
        );
        assert!(parse("---\ntitle: x\npriority: urgent\n---\n").is_err());
        assert!(
            parse("---\ntitle: x\n---\n")
                .unwrap_err()
                .contains("priority")
        );
        assert!(
            parse("---\ntitle: x\npriority: low\nblocked-by: [AGT-1]\n---\n")
                .unwrap_err()
                .contains("blocked-by")
        );
        // The error header is stripped like the normal one.
        let e = error_file(
            "AGT-1",
            "boom\nline 2",
            "# old\n---\ntitle: x\npriority: low\n---\nb",
        );
        assert!(e.starts_with("# pm edit AGT-1: could not save:\n#   boom\n#   line 2\n"));
        assert_eq!(parse(&e).unwrap().body, "b");
    }

    #[test]
    fn session_peers_avoid_reserved_ids() {
        assert_ne!(session_peer(Ulid::from_parts(1, 0)), 0);
        assert_ne!(
            session_peer(Ulid::from_parts(1, u64::MAX as u128)),
            u64::MAX
        );
        assert_ne!(session_peer(Ulid::new()), session_peer(Ulid::new()));
    }

    #[test]
    fn view_flag_parses() {
        assert_eq!(parse_view("editor"), Ok(View::Editor));
        assert_eq!(parse_view("ui-leaf"), Ok(View::UiLeaf));
        assert!(parse_view("emacs").is_err());
    }

    /// Applies `a` then `b`, and `b` then `a`, on top of `base`; both
    /// orders must give the same ticket.
    fn converge(id: Ulid, base: &[Op], a: &[Op], b: &[Op]) -> pm_core::Ticket {
        let ab = view_of(id, &[base, a, b].concat()).snapshot();
        let ba = view_of(id, &[base, b, a].concat()).snapshot();
        assert_eq!(ab, ba, "op streams must converge in either order");
        ab
    }

    fn stamp(id: Ulid, start_ms: u64, actor: &str, payloads: Vec<Payload>) -> Vec<Op> {
        payloads
            .into_iter()
            .enumerate()
            .map(|(i, p)| op(id, start_ms + i as u64, actor, p))
            .collect()
    }

    /// AC3: a `pm edit` and a concurrent `pm set` (both started from the
    /// same state) converge whichever lands first.
    #[test]
    fn concurrent_edit_and_set_converge() {
        let id = Ulid::new();
        let base = base_ops(id);
        let view = view_of(id, &base);
        let (before, after) = edited(&view, |t| {
            t.replace("title: Original", "title: From edit")
                .replace("- a\n", "")
                .replace("gamma", "gamma\ndelta")
        });
        let edit = stamp(id, 100, "matt", plan(&view, &before, &after, 42).unwrap());
        // `pm set AGT-N title=… priority=high` plus a `pm label +a` re-add,
        // concurrently from another actor, stamped later.
        let set = stamp(
            id,
            200,
            "claude:loop",
            vec![
                Payload::FieldSet(FieldSet::Title("From set".into())),
                Payload::FieldSet(FieldSet::Priority(Priority::High)),
                Payload::LabelAdd(LabelAdd { label: "a".into() }),
                Payload::StateTransition(StateTransition {
                    state: "triage".into(),
                }),
            ],
        );
        let t = converge(id, &base, &edit, &set);
        assert_eq!(t.title, "From set", "LWW: the later HLC wins");
        assert_eq!(t.priority, Priority::High);
        assert!(
            t.labels.contains("a"),
            "a concurrent re-add survives the remove"
        );
        assert_eq!(t.description, "alpha\nbeta\ngamma\ndelta");
    }

    /// Two concurrent `pm edit`s by the *same* actor on the body both
    /// survive: each session has its own Loro peer, so neither update is
    /// mistaken for a duplicate of the other. (With one shared peer this
    /// test fails: the two orders diverge, one even garbling the text.)
    #[test]
    fn concurrent_body_edits_by_one_actor_merge() {
        let id = Ulid::new();
        let base = base_ops(id);
        let view = view_of(id, &base);
        let (b1, a1) = edited(&view, |t| t.replace("alpha", "ALPHA"));
        let (b2, a2) = edited(&view, |t| t.replace("gamma", "gamma\nomega"));
        let one = stamp(
            id,
            100,
            "matt",
            plan(&view, &b1, &a1, session_peer(Ulid::new())).unwrap(),
        );
        let two = stamp(
            id,
            100,
            "matt",
            plan(&view, &b2, &a2, session_peer(Ulid::new())).unwrap(),
        );
        let t = converge(id, &base, &one, &two);
        assert_eq!(t.description, "ALPHA\nbeta\ngamma\nomega");
    }
}
