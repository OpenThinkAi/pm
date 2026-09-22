use anyhow::Context;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod ticket;

use ticket::{Priority, State, Ticket, TicketId};

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
    /// List tickets
    List,
    /// Show one ticket file
    Show { path: PathBuf },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Ticket { cmd } => match cmd {
            TicketCmd::List => {
                let t = Ticket {
                    id: TicketId(1),
                    title: String::from("learn rust by building pm"),
                    state: State::InProgress,
                    priority: Priority::High,
                    project: None,
                };
                let project = match &t.project {
                    Some(p) => p.as_str(),
                    None => "-",
                };
                println!(
                    "{} {} {} {} {}",
                    t.id, t.state, t.priority, project, t.title
                );
            }
            TicketCmd::Show { path } => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let t = Ticket::parse(&text)?;
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
