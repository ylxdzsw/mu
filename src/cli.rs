use std::path::PathBuf;

use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};

use crate::models::EffortLevel;

#[derive(Parser, Debug)]
#[command(name = "mu", about = "Fast terminal agent harness")]
pub struct Args {
    #[command(flatten)]
    pub turn: TurnArgs,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(ClapArgs, Debug, Clone, Default)]
pub struct SelectionArgs {
    #[arg(short = 's', long)]
    pub session: Option<String>,

    #[arg(short = 'c', long)]
    pub continue_latest: bool,

    #[arg(long)]
    pub model: Option<String>,

    #[arg(long, value_enum)]
    pub effort: Option<EffortLevel>,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct TurnArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,

    #[arg(short = 'i', long = "image")]
    pub images: Vec<PathBuf>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
    pub output: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Plain,
    Terminal,
    Json,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Session management
    Session {
        #[command(subcommand)]
        sub: SessionSub,
    },
    /// Inspect the resolved model, effort, and context state
    Status(StatusArgs),
    /// Refresh or inspect the generated provider model catalog
    Models {
        #[command(subcommand)]
        sub: ModelsSub,
    },
    /// Force compaction for a session
    Compact {
        #[arg(long)]
        session: String,
    },
}

#[derive(ClapArgs, Debug, Clone)]
pub struct StatusArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,

    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum SessionSub {
    /// Create a new session and print its id
    New,
    /// List recent sessions
    List,
    /// Hide a session from default lists
    Archive {
        #[arg(long)]
        session: String,
    },
    /// Restore an archived session to default lists
    Unarchive {
        #[arg(long)]
        session: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum ModelsSub {
    /// Refresh the generated provider model catalog
    Refresh,
    /// List the cached provider model catalog
    List {
        #[arg(long)]
        json: bool,
    },
}
