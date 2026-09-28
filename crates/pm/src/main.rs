use clap::{Parser, Subcommand};
use std::cmp::Reverse;
use std::path::PathBuf;

mod ticket;

use ticket::{Filter, State, Ticket};

/// pm - ticket tool for the saltline vault (AGT-numbered tickets)
#[derive(Parser, Debug)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Tickets
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

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Ticket { cmd } => match cmd {
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
        },
    }
    Ok(())
}
