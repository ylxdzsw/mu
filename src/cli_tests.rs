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
fn retry_subcommand_parses_output_and_selection() {
    let args = Args::try_parse_from(["mu", "retry", "-c", "--output", "json"]).unwrap();
    match args.command {
        Some(Command::Retry(retry)) => {
            assert!(retry.continue_latest);
            assert!(retry.session.is_none());
            assert_eq!(retry.output, OutputFormat::Json);
        }
        other => panic!("expected retry command, got {other:?}"),
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

#[test]
fn project_init_defaults_to_current_directory() {
    let args = Args::try_parse_from(["mu", "project", "init"]).unwrap();
    match args.command {
        Some(Command::Project {
            sub: ProjectSub::Init { path, force, json },
        }) => {
            assert!(path.is_none());
            assert!(!force);
            assert!(!json);
        }
        other => panic!("expected project init command, got {other:?}"),
    }
}
