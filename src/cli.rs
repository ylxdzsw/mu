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

#[derive(ClapArgs, Debug, Clone)]
pub struct RunArgs {
    #[command(flatten)]
    pub turn: TurnArgs,

    #[arg(value_name = "PROMPT_FILE")]
    pub prompt_file: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Plain,
    Terminal,
    Json,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run one turn from a prompt file
    Run(RunArgs),
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn parses_run_with_prompt_file() {
        let args = Args::try_parse_from(["mu", "run", "prompt.md"]).unwrap();
        match args.command {
            Some(Command::Run(run)) => {
                assert_eq!(run.prompt_file, PathBuf::from("prompt.md"));
                assert_eq!(run.turn.output, OutputFormat::Terminal);
                assert!(run.turn.images.is_empty());
                assert!(run.turn.selection.session.is_none());
                assert!(!run.turn.selection.continue_latest);
            }
            other => panic!("expected run command, got {other:?}"),
        }
    }

    #[test]
    fn parses_run_with_turn_options() {
        let args = Args::try_parse_from([
            "mu",
            "run",
            "--output",
            "plain",
            "--model",
            "gpt-test",
            "-i",
            "image.png",
            "prompt.md",
        ])
        .unwrap();
        match args.command {
            Some(Command::Run(run)) => {
                assert_eq!(run.prompt_file, PathBuf::from("prompt.md"));
                assert_eq!(run.turn.output, OutputFormat::Plain);
                assert_eq!(run.turn.selection.model.as_deref(), Some("gpt-test"));
                assert_eq!(run.turn.images, vec![PathBuf::from("image.png")]);
            }
            other => panic!("expected run command, got {other:?}"),
        }
    }

    #[test]
    fn parses_default_turn_without_run() {
        let args = Args::try_parse_from(["mu", "--output", "plain"]).unwrap();
        assert!(args.command.is_none());
        assert_eq!(args.turn.output, OutputFormat::Plain);
    }

    #[test]
    fn keeps_existing_subcommands() {
        let args = Args::try_parse_from(["mu", "status", "--json"]).unwrap();
        match args.command {
            Some(Command::Status(status)) => {
                assert!(status.json);
            }
            other => panic!("expected status command, got {other:?}"),
        }
    }
}
