use std::path::PathBuf;

use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "mu", about = "Fast terminal agent harness")]
pub struct Args {
    /// Mark newly created sessions as coming from a surface
    #[arg(long, global = true, value_enum, default_value_t = SessionOriginArg::Cli)]
    pub origin: SessionOriginArg,

    #[command(flatten)]
    pub turn: TurnArgs,

    /// Run one turn from a prompt file
    #[arg(value_name = "PROMPT_FILE")]
    pub prompt_file: Option<PathBuf>,

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
    /// Inspect the resolved model and context state
    Status(StatusArgs),
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

    #[arg(long)]
    pub include_models: bool,
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
    fn parses_prompt_file_without_run() {
        let args = Args::try_parse_from(["mu", "prompt.md"]).unwrap();
        assert_eq!(args.prompt_file, Some(PathBuf::from("prompt.md")));
        assert!(args.command.is_none());
        assert_eq!(args.turn.output, OutputFormat::Terminal);
        assert!(args.turn.images.is_empty());
        assert!(args.turn.selection.session.is_none());
        assert!(!args.turn.selection.continue_latest);
    }

    #[test]
    fn parses_prompt_file_with_turn_options() {
        let args = Args::try_parse_from([
            "mu",
            "--output",
            "plain",
            "--model",
            "gpt-test",
            "-i",
            "image.png",
            "prompt.md",
        ])
        .unwrap();
        assert_eq!(args.prompt_file, Some(PathBuf::from("prompt.md")));
        assert!(args.command.is_none());
        assert_eq!(args.turn.output, OutputFormat::Plain);
        assert_eq!(args.turn.selection.model.as_deref(), Some("gpt-test"));
        assert_eq!(args.turn.images, vec![PathBuf::from("image.png")]);
    }

    #[test]
    fn parses_default_turn_without_run() {
        let args = Args::try_parse_from(["mu", "--output", "plain"]).unwrap();
        assert!(args.command.is_none());
        assert_eq!(args.turn.output, OutputFormat::Plain);
    }

    #[test]
    fn exact_subcommand_names_take_priority_over_prompt_files() {
        let args = Args::try_parse_from(["mu", "status", "--json"]).unwrap();
        assert!(args.prompt_file.is_none());
        match args.command {
            Some(Command::Status(status)) => {
                assert!(status.json);
            }
            other => panic!("expected status command, got {other:?}"),
        }
    }

    #[test]
    fn global_origin_flag_applies_to_subcommands() {
        let args = Args::try_parse_from(["mu", "--origin", "web", "session", "list"]).unwrap();
        assert_eq!(args.origin, SessionOriginArg::Web);
        match args.command {
            Some(Command::Session {
                sub:
                    SessionSub::List {
                        json,
                        limit,
                        all_origins,
                    },
            }) => {
                assert!(!json);
                assert_eq!(limit, 20);
                assert!(!all_origins);
            }
            other => panic!("expected session list command, got {other:?}"),
        }
    }

    #[test]
    fn disambiguated_prompt_file_with_dot_slash_is_allowed() {
        let args = Args::try_parse_from(["mu", "./status"]).unwrap();
        assert_eq!(args.prompt_file, Some(PathBuf::from("./status")));
        assert!(args.command.is_none());
    }

    #[test]
    fn project_subcommand_keeps_explicit_path_argument() {
        let args = Args::try_parse_from(["mu", "project", "inspect", "--path", "repo"]).unwrap();
        match args.command {
            Some(Command::Project {
                sub: ProjectSub::Inspect { path, json },
            }) => {
                assert_eq!(path, PathBuf::from("repo"));
                assert!(!json);
            }
            other => panic!("expected project inspect command, got {other:?}"),
        }
    }
}
