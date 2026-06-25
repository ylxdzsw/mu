use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "mu", about = "Fast terminal agent harness")]
pub struct Args {
    #[arg(short = 's', long)]
    pub session: Option<String>,

    #[arg(short = 'c', long)]
    pub continue_latest: bool,

    #[arg(long)]
    pub model: Option<String>,

    #[arg(short = 'i', long = "image")]
    pub images: Vec<std::path::PathBuf>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Terminal)]
    pub output: OutputFormat,

    #[command(subcommand)]
    pub command: Option<Command>,
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
    /// Force compaction for a session
    Compact {
        #[arg(long)]
        session: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum SessionSub {
    /// Create a new session and print its id
    New,
    /// List recent sessions
    List,
}
