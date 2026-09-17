use clap::{Parser, Subcommand};

/// agt - ticket tool
#[derive(Parser, Debug)]
#[command(name = "agt", version)]
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
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Ticket { cmd } => match cmd {
            TicketCmd::List => println!("no tickets yet"),
        },
    }
    Ok(())
}
