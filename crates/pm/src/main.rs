use clap::{Parser, Subcommand};
use std::cmp::Reverse;
use std::path::PathBuf;
use std::process::ExitCode;

mod backup;
mod batch;
mod check;
mod claim;
mod doctor;
mod edit;
mod exit;
mod markers;
mod mutate;
mod read;
mod ticket;
mod verbs;
mod workspace;

use ticket::{Filter, State, Ticket};

/// pm - local-first ticketing for agents and humans
///
/// Exit codes: 0 ok, 1 error (or an unhealthy database, for doctor; findings, for check), 2 usage,
/// 3 not found, 75 taken (claim).
#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    /// Workspace directory (else PM_WORKSPACE, else `workspace` in ~/.config/pm/config.toml)
    #[arg(long, global = true, value_name = "DIR")]
    workspace: Option<PathBuf>,
    /// Actor recorded on every op (PM_ACTOR wins over this; $USER is the fallback)
    #[arg(long = "as", global = true, value_name = "ACTOR")]
    as_actor: Option<String>,
    /// Machine-readable output (`{"schema": 1, ...}`)
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
// `New` carries every `pm new` flag (AGT-1346 AC5); clap enums are parsed
// once per invocation, so the size difference from `Init`/`Show`/etc. is
// not worth boxing fields over.
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Create a workspace database (saltline states: triage, in-progress, done)
    Init {
        /// Ticket id prefix, e.g. AGT
        #[arg(long)]
        prefix: String,
    },
    /// File a ticket (or several) and print their id(s)
    New {
        /// Required unless --from-file or --batch is given
        #[arg(long)]
        title: Option<String>,
        /// Project id; the project must already exist
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        /// low, medium (default), high or critical
        #[arg(long, value_parser = verbs::parse_priority)]
        priority: Option<pm_core::Priority>,
        /// Label to add; repeat or comma-separate for several
        #[arg(long = "label", value_name = "LABEL", value_delimiter = ',')]
        labels: Vec<String>,
        /// Ticket description (markdown)
        #[arg(long)]
        description: Option<String>,
        /// Read the description from a file, or `-` for stdin
        #[arg(long = "description-file", value_name = "PATH|-")]
        description_file: Option<String>,
        /// Ticket(s) this one is blocked by: AGT-N or ULID, repeat or comma-separate
        #[arg(long = "blocked-by", value_name = "ID", value_delimiter = ',')]
        blocked_by: Vec<String>,
        #[arg(long = "linked-github", value_name = "URL")]
        linked_github: Option<String>,
        /// type=manual|github|linear|jira|notion,url=…,id=…,fetched-at=…
        #[arg(long, value_name = "type=…,url=…,id=…")]
        source: Option<String>,
        /// Parse a vault-format ticket file (frontmatter + sections) into one ticket
        #[arg(long = "from-file", value_name = "PATH")]
        from_file: Option<PathBuf>,
        /// Create every ticket in a YAML batch spec, in one transaction
        #[arg(long, value_name = "PATH")]
        batch: Option<PathBuf>,
    },
    /// Print a ticket
    Show {
        /// Ticket id (AGT-12) or ULID
        id: String,
        /// Print only this field's value
        #[arg(long, value_name = "NAME")]
        field: Option<String>,
        /// Print only this markdown section of the description (e.g. "Acceptance Criteria")
        #[arg(long, value_name = "NAME")]
        section: Option<String>,
    },
    /// Set ticket fields: title, priority, project, repo, assignee, linked-github, linked-pr,
    /// linear, not_before=YYYY-MM-DD, parked=YYYY-MM-DD|forever (empty value clears; unknown
    /// keys land in ext with a warning)
    Set {
        /// Ticket id (AGT-12) or ULID
        id: String,
        /// key=value pairs
        #[arg(required = true, value_name = "KEY=VALUE")]
        assignments: Vec<String>,
    },
    /// Add or remove labels: +x adds, -y removes. Since `-y` looks like a flag,
    /// put global flags (--json, --as, --workspace) before `label`, not after.
    Label {
        /// Ticket id (AGT-12) or ULID
        id: String,
        /// +label to add, -label to remove
        #[arg(
            required = true,
            value_name = "+LABEL|-LABEL",
            allow_hyphen_values = true
        )]
        changes: Vec<String>,
    },
    /// Append a comment
    Comment {
        /// Ticket id (AGT-12) or ULID
        id: String,
        /// Comment text; omit when using --file
        text: Option<String>,
        /// Read the comment body from a file, or `-` for stdin
        #[arg(long, value_name = "PATH|-")]
        file: Option<String>,
    },
    /// Move a ticket to a workflow state
    Move {
        /// Ticket id (AGT-12) or ULID
        id: String,
        /// A state this workspace defines
        state: String,
    },
    /// Transition a ticket to the workspace's completed state
    Done {
        /// Ticket id (AGT-12) or ULID
        id: String,
        /// Appended as a comment
        #[arg(long)]
        note: Option<String>,
        /// Recorded under ext.merged_sha
        #[arg(long = "merged-sha", value_name = "SHA")]
        merged_sha: Option<String>,
        /// Recorded as linked-pr
        #[arg(long, value_name = "URL")]
        pr: Option<String>,
    },
    /// Return a started ticket to unstarted and clear its assignee
    Unclaim {
        /// Ticket id (AGT-12) or ULID
        id: String,
    },
    /// Edit a ticket in $EDITOR (frontmatter + markdown); the save becomes ops
    Edit {
        /// Ticket id (AGT-12) or ULID
        id: String,
        /// editor ($EDITOR) or ui-leaf; default: config `edit.view`, else editor
        #[arg(long, value_parser = edit::parse_view)]
        view: Option<edit::View>,
    },
    /// Take a ticket: unstarted and unassigned -> started, assigned to you (exit 75 if someone else has it)
    Claim {
        /// Ticket id (AGT-12) or ULID; omit with --ready
        id: Option<String>,
        /// Claim the lowest-numbered ready ticket instead (exit 3 if none)
        #[arg(long, conflicts_with = "id")]
        ready: bool,
        /// With --ready: only tickets in this project
        #[arg(long)]
        project: Option<String>,
        /// Record the git branch the work lands on (kept in the ticket's `ext.branch`)
        #[arg(long, value_name = "BRANCH")]
        branch: Option<String>,
    },
    /// List tickets, AND-combining whichever filters are given (each accepts comma-separated alternatives)
    List {
        #[arg(long, value_delimiter = ',')]
        project: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        state: Vec<String>,
        #[arg(long = "label", value_delimiter = ',')]
        labels: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        repo: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        assignee: Vec<String>,
        /// Only tickets with a hold set
        #[arg(long)]
        held: bool,
        /// Matches `linked-github` (`pm new`/`pm set`'s name for the same field)
        #[arg(long, visible_alias = "linked-github", value_delimiter = ',')]
        github: Vec<String>,
        /// Case-insensitive substring match against title or description
        #[arg(long)]
        search: Option<String>,
        /// Include archived tickets
        #[arg(long)]
        archived: bool,
    },
    /// List a ticket's ops, oldest first
    Log {
        /// Ticket id (AGT-12) or ULID
        id: String,
    },
    /// Counts of tickets per state, plus held/parked
    Status {
        #[arg(long)]
        project: Option<String>,
    },
    /// Dependency waves of not-yet-done tickets, plus a done flag
    Graph {
        #[arg(long)]
        project: Option<String>,
    },
    /// Hold a ticket for a human (`pm hold AGT-N "why"`), or release it (`--clear`)
    Hold {
        /// Ticket id (AGT-12) or ULID
        id: String,
        /// Why the ticket is waiting on a human
        reason: Option<String>,
        /// Clear the hold instead of setting one
        #[arg(long)]
        clear: bool,
    },
    /// List held tickets
    Holds {
        /// Only tickets in this project
        #[arg(long)]
        project: Option<String>,
    },
    /// Waive a hygiene rule for a ticket (e.g. `pm waive AGT-N R1 "standalone: why"`)
    Waive {
        /// Ticket id (AGT-12) or ULID
        id: String,
        /// The rule waived (R1, …)
        rule: String,
        /// Why
        reason: String,
    },
    /// Report invariant findings: R1, stale, held, blocker cycles, dangling relations (exit 1 if any)
    Check {
        /// Only findings touching this project's tickets
        #[arg(long)]
        project: Option<String>,
    },
    /// Check the database: constraints, and that the ticket tables replay from the op log (exit 1 if not)
    Doctor {
        /// Regenerate the ticket tables from the op log first and print what changed
        #[arg(long)]
        rebuild: bool,
    },
    /// Markdown vault tickets (legacy, reads ticket files directly)
    Ticket {
        #[command(subcommand)]
        cmd: TicketCmd,
    },
    /// Export the op log to a git repo (or restore a workspace from one), and manage its launchd timer
    Backup {
        /// Append new ops to <DIR> as JSONL, commit, and push (default: config `backup.repo`)
        #[arg(long = "to", value_name = "DIR")]
        to: Option<PathBuf>,
        /// Rebuild the resolved --workspace from <DIR>'s ops/*.jsonl instead of backing up
        #[arg(long, value_name = "DIR", conflicts_with = "to")]
        restore: Option<PathBuf>,
        #[command(subcommand)]
        cmd: Option<BackupCmd>,
    },
}

#[derive(Subcommand, Debug)]
enum BackupCmd {
    /// Write (and, by default, load) an hourly launchd job that runs `pm backup` for this workspace
    InstallTimer {
        /// Write the plist here instead of ~/Library/LaunchAgents; never loads it into launchd
        #[arg(long, value_name = "DIR")]
        dir: Option<PathBuf>,
        /// Write the plist without loading it
        #[arg(long = "no-load")]
        no_load: bool,
    },
    /// Report the last backup's outcome; exit 1 if it never succeeded or is more than 24h old
    Status {
        /// Which target to report on (default: config `backup.repo`)
        #[arg(long = "to", value_name = "DIR")]
        to: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum TicketCmd {
    /// List every ticket under a directory
    List {
        dir: PathBuf,
        /// Only tickets in this state
        #[arg(long)]
        state: Option<State>,
        /// Only tickets in this project
        #[arg(long)]
        project: Option<String>,
    },
    /// Show one ticket file
    Show { path: PathBuf },
}

fn main() -> ExitCode {
    // Clap exits 2 on usage errors and 0 for --help/--version.
    let cli = Cli::parse();
    let env = workspace::Env::from_process();
    let ctx = verbs::Ctx {
        env: &env,
        workspace: cli.workspace.as_deref(),
        as_flag: cli.as_actor.as_deref(),
        json: cli.json,
    };
    match run(&ctx, cli.cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("pm: {:#}", e.error);
            ExitCode::from(e.code)
        }
    }
}

fn run(ctx: &verbs::Ctx<'_>, cmd: Cmd) -> exit::Result<()> {
    match cmd {
        Cmd::Init { prefix } => verbs::init(ctx, &prefix),
        Cmd::New {
            title,
            project,
            repo,
            priority,
            labels,
            description,
            description_file,
            blocked_by,
            linked_github,
            source,
            from_file,
            batch,
        } => verbs::new(
            ctx,
            verbs::NewArgs {
                title,
                project,
                repo,
                priority,
                labels,
                description,
                description_file,
                blocked_by,
                linked_github,
                source,
                from_file,
                batch,
            },
        ),
        Cmd::Show { id, field, section } => {
            verbs::show(ctx, &id, field.as_deref(), section.as_deref())
        }
        Cmd::Set { id, assignments } => verbs::set(ctx, &id, &assignments),
        Cmd::Label { id, changes } => mutate::label(ctx, &id, &changes),
        Cmd::Comment { id, text, file } => {
            mutate::comment(ctx, &id, text.as_deref(), file.as_deref())
        }
        Cmd::Move { id, state } => mutate::mv(ctx, &id, &state),
        Cmd::Done {
            id,
            note,
            merged_sha,
            pr,
        } => mutate::done(
            ctx,
            &id,
            mutate::DoneArgs {
                note,
                merged_sha,
                pr,
            },
        ),
        Cmd::Unclaim { id } => mutate::unclaim(ctx, &id),
        Cmd::Edit { id, view } => edit::edit(ctx, &id, view),
        Cmd::Claim {
            id,
            ready,
            project,
            branch,
        } => claim::claim(
            ctx,
            claim::ClaimArgs {
                id,
                ready,
                project,
                branch,
            },
        ),
        Cmd::List {
            project,
            state,
            labels,
            repo,
            assignee,
            held,
            github,
            search,
            archived,
        } => read::list(
            ctx,
            read::ListArgs {
                project,
                state,
                label: labels,
                repo,
                assignee,
                held,
                github,
                search,
                archived,
            },
        ),
        Cmd::Log { id } => read::log(ctx, &id),
        Cmd::Status { project } => read::status(ctx, project),
        Cmd::Graph { project } => read::graph(ctx, project),
        Cmd::Hold { id, reason, clear } => markers::hold(ctx, &id, reason.as_deref(), clear),
        Cmd::Holds { project } => markers::holds(ctx, project.as_deref()),
        Cmd::Waive { id, rule, reason } => markers::waive(ctx, &id, &rule, &reason),
        Cmd::Check { project } => check::check(ctx, project.as_deref()),
        Cmd::Doctor { rebuild } => doctor::doctor(ctx, rebuild),
        Cmd::Ticket { cmd } => Ok(legacy_ticket(cmd)?),
        Cmd::Backup { to, restore, cmd } => match cmd {
            Some(BackupCmd::InstallTimer { dir, no_load }) => {
                backup::install_timer(ctx, dir, no_load)
            }
            Some(BackupCmd::Status { to }) => backup::status(ctx, to),
            None if to.is_some() && restore.is_some() => {
                unreachable!("clap's conflicts_with rules this out")
            }
            None => match restore {
                Some(dir) => backup::restore(ctx, &dir),
                None => backup::run(ctx, to),
            },
        },
    }
}

fn legacy_ticket(cmd: TicketCmd) -> anyhow::Result<()> {
    match cmd {
        TicketCmd::List {
            dir,
            state,
            project,
        } => {
            let filter = Filter { state, project };
            let mut tickets = ticket::load_dir(&dir)?;
            tickets.retain(|t| filter.matches(t));
            tickets.sort_by_key(|t| (Reverse(t.priority), t.id));
            if tickets.is_empty() {
                eprintln!("no tickets found");
            }
            for t in &tickets {
                println!("{t}");
            }
        }
        TicketCmd::Show { path } => {
            let t = Ticket::load(&path)?;
            let project = t.project.as_deref().unwrap_or("-");
            println!("id:       {}", t.id);
            println!("title:    {}", t.title);
            println!("state:    {}", t.state);
            println!("priority: {}", t.priority);
            println!("project:  {project}");
        }
    }
    Ok(())
}
