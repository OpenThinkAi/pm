use clap::{Parser, Subcommand};
use std::cmp::Reverse;
use std::path::PathBuf;
use std::process::ExitCode;

mod batch;
mod exit;
mod ticket;
mod verbs;
mod workspace;

use ticket::{Filter, State, Ticket};

/// pm - local-first ticketing for agents and humans
///
/// Exit codes: 0 ok, 1 error, 2 usage, 3 not found.
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
    },
    /// Set ticket fields: title, priority, project, repo (empty value clears)
    Set {
        /// Ticket id (AGT-12) or ULID
        id: String,
        /// key=value pairs
        #[arg(required = true, value_name = "KEY=VALUE")]
        assignments: Vec<String>,
    },
    /// Markdown vault tickets (legacy, reads ticket files directly)
    Ticket {
        #[command(subcommand)]
        cmd: TicketCmd,
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
        Cmd::Show { id, field } => verbs::show(ctx, &id, field.as_deref()),
        Cmd::Set { id, assignments } => verbs::set(ctx, &id, &assignments),
        Cmd::Ticket { cmd } => Ok(legacy_ticket(cmd)?),
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
