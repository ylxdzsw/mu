use std::path::PathBuf;

use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};

use crate::models::EffortLevel;

#[derive(Parser, Debug)]
#[command(name = "mu", about = "Fast terminal agent harness")]
pub struct Args {
    /// Run project-scoped commands against an explicit directory
    #[arg(long, global = true)]
    pub project: Option<PathBuf>,

    /// Mark newly created sessions as coming from a surface
    #[arg(long, global = true, value_enum, default_value_t = SessionOriginArg::Cli)]
    pub origin: SessionOriginArg,

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
    /// Project management
    Project {
        #[command(subcommand)]
        sub: ProjectSub,
    },
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
    /// Serve the local browser UI
    Web(WebArgs),
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
    New {
        #[arg(long)]
        json: bool,
    },
    /// List recent sessions
    List {
        #[arg(long)]
        json: bool,

        #[arg(long, default_value_t = 20)]
        limit: usize,

        #[arg(long)]
        all_origins: bool,
    },
    /// Print a session transcript
    Transcript {
        #[arg(long)]
        session: String,

        #[arg(long)]
        json: bool,
    },
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
pub enum ProjectSub {
    /// Inspect whether a directory is an existing mu project
    Inspect {
        #[arg(long)]
        path: PathBuf,

        #[arg(long)]
        json: bool,
    },
    /// Explicitly create mu project metadata in a directory
    Init {
        #[arg(long)]
        path: PathBuf,

        #[arg(long)]
        json: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SessionOriginArg {
    Cli,
    Web,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct WebArgs {
    /// Unix socket path to listen on
    #[arg(long, default_value = "/run/mu-web/mu-web.sock")]
    pub socket: PathBuf,

    /// Unix socket permissions, written as an octal mode such as 0600 or 0660
    #[arg(long, default_value = "0600")]
    pub socket_mode: String,
}
