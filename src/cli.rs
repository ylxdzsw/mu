use std::path::PathBuf;

use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "mu", about = "Fast terminal agent harness")]
pub struct Args {
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

    #[arg(short = 'a', long = "attach", value_name = "FILE")]
    pub attachments: Vec<PathBuf>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Detail)]
    pub output: OutputFormat,
}

#[derive(ClapArgs, Debug, Clone)]
pub struct RetryArgs {
    #[command(flatten)]
    pub selection: SelectionArgs,

    #[arg(long, value_enum, default_value_t = OutputFormat::Detail)]
    pub output: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Final,
    Concise,
    Detail,
    Full,
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
    /// Resume an interrupted (unclean) turn in a session
    Retry(RetryArgs),
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

    #[arg(long)]
    pub include_models: bool,

    #[arg(long)]
    pub include_commands: bool,

    #[arg(long)]
    pub include_skills: bool,
}

#[derive(Subcommand, Debug)]
pub enum SessionSub {
    /// Create a new session and print its id
    New,
    /// List recent sessions
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print a session transcript
    Transcript {
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
    },
    /// Explicitly create mu project metadata in a directory
    Init {
        #[arg(long)]
        path: Option<PathBuf>,

        #[arg(long)]
        force: bool,
    },
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::*;

    #[test]
    fn parses_prompt_file_with_turn_options() {
        let args = Args::try_parse_from([
            "mu",
            "--output",
            "detail",
            "--model",
            "gpt-test",
            "-a",
            "image.png",
            "--attach",
            "audio.wav",
            "prompt.md",
        ])
        .unwrap();
        assert_eq!(args.prompt_file, Some(PathBuf::from("prompt.md")));
        assert!(args.command.is_none());
        assert_eq!(args.turn.output, OutputFormat::Detail);
        assert_eq!(args.turn.selection.model.as_deref(), Some("gpt-test"));
        assert_eq!(
            args.turn.attachments,
            vec![PathBuf::from("image.png"), PathBuf::from("audio.wav")]
        );
    }

    #[test]
    fn parses_final_output_mode() {
        let args = Args::try_parse_from(["mu", "--output", "final"]).unwrap();
        assert_eq!(args.turn.output, OutputFormat::Final);
    }

    #[test]
    fn defaults_to_detail_output_mode() {
        let args = Args::try_parse_from(["mu"]).unwrap();
        assert_eq!(args.turn.output, OutputFormat::Detail);
    }

    #[test]
    fn parses_all_output_modes_and_rejects_removed_values() {
        for (value, expected) in [
            ("final", OutputFormat::Final),
            ("concise", OutputFormat::Concise),
            ("detail", OutputFormat::Detail),
            ("full", OutputFormat::Full),
        ] {
            let args = Args::try_parse_from(["mu", "--output", value]).unwrap();
            assert_eq!(args.turn.output, expected);
        }
        for removed in ["plain", "terminal"] {
            assert!(Args::try_parse_from(["mu", "--output", removed]).is_err());
        }
    }

    #[test]
    fn exact_subcommand_names_take_priority_over_prompt_files() {
        let args = Args::try_parse_from(["mu", "status", "--json"]).unwrap();
        assert!(args.prompt_file.is_none());
        match args.command {
            Some(Command::Status(status)) => assert!(status.json),
            other => panic!("expected status command, got {other:?}"),
        }
    }

    #[test]
    fn parses_status_include_skills() {
        let args = Args::try_parse_from(["mu", "status", "--json", "--include-skills"]).unwrap();
        match args.command {
            Some(Command::Status(status)) => {
                assert!(status.json);
                assert!(status.include_skills);
            }
            other => panic!("expected status command, got {other:?}"),
        }
    }

    #[test]
    fn parses_retry_model_override() {
        let args = Args::try_parse_from([
            "mu",
            "retry",
            "-s",
            "session-1",
            "--model",
            "opencode/mimo-v2.5-free",
            "--output",
            "detail",
        ])
        .unwrap();
        match args.command {
            Some(Command::Retry(retry)) => {
                assert_eq!(retry.selection.session.as_deref(), Some("session-1"));
                assert_eq!(
                    retry.selection.model.as_deref(),
                    Some("opencode/mimo-v2.5-free")
                );
                assert_eq!(retry.output, OutputFormat::Detail);
            }
            other => panic!("expected retry command, got {other:?}"),
        }
    }
}
