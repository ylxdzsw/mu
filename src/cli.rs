use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "mu", about = "Fast terminal agent harness")]
pub struct Args {
    #[arg(long)]
    pub session: Option<String>,

    #[arg(long)]
    pub model: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Write starter config and create config directory
    Init {
        #[command(subcommand)]
        sub: Option<InitSub>,
    },
    /// Session management
    Session {
        #[command(subcommand)]
        sub: SessionSub,
    },
    /// Force compaction for a session
    Compact {
        #[arg(long)]
        session: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum InitSub {
    /// Print the zsh plugin for eval
    Zsh,
}

#[derive(Subcommand, Debug)]
pub enum SessionSub {
    /// Create a new session and print its id
    New,
    /// List recent sessions
    List,
}
