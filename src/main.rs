use std::fmt;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use serde::Deserialize;

#[cfg(not(unix))]
compile_error!("mu is supported only on Unix-like systems");

mod agent;
mod anthropic;
mod applets;
mod attachment;
mod bash;
mod chat_completions;
mod compaction;
mod config;
mod guardrail;
mod install;
mod models;
mod paths;
mod provider;
mod random;
mod redaction;
mod renderer;
mod responses;
mod runtime;
mod skills;
mod store;
mod system_prompt;

#[cfg(test)]
use attachment::MAX_ATTACHMENT_BYTES;
use attachment::load_attachments;
use config::Config;
use models::ResolvedModelChoice;
use provider::build_provider;
use provider::{ContentPart, UserContent};
use renderer::Renderer;
use runtime::{
    InvocationOverrides, StatusIncludes, StatusReport, build_status_report, resolve_invocation,
    resolve_retry_model_selection, resolve_session_model,
};

#[derive(Parser, Debug)]
#[command(
    name = "mu",
    about = "Fast terminal agent harness",
    args_conflicts_with_subcommands = true,
    subcommand_precedence_over_arg = true
)]
struct Args {
    #[command(flatten)]
    turn: TurnArgs,

    /// Run one turn from a prompt file
    #[arg(value_name = "PROMPT_FILE")]
    prompt_file: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(ClapArgs, Debug, Clone, Default)]
struct SelectionArgs {
    #[arg(short = 's', long, conflicts_with = "continue_current")]
    session: Option<String>,

    /// Continue the last selected session in this scope
    #[arg(short = 'c', long = "continue", conflicts_with = "session")]
    continue_current: bool,

    #[arg(short = 'm', long)]
    model: Option<String>,
}

#[derive(ClapArgs, Debug, Clone)]
struct TurnArgs {
    #[command(flatten)]
    selection: SelectionArgs,

    #[arg(short = 'a', long = "attach", value_name = "FILE")]
    attachments: Vec<PathBuf>,

    /// Output density (overrides config)
    #[arg(short = 'o', long, value_enum)]
    output: Option<OutputFormat>,
}

#[derive(ClapArgs, Debug, Clone)]
struct RetryArgs {
    #[command(flatten)]
    selection: SelectionArgs,

    /// Output density (overrides config)
    #[arg(short = 'o', long, value_enum)]
    output: Option<OutputFormat>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Final,
    Concise,
    #[default]
    Detail,
    Full,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Explicitly create mu project metadata in a directory
    Init {
        #[arg(long)]
        path: Option<PathBuf>,

        #[arg(long)]
        force: bool,
    },
    /// Create a new session and print its id
    New,
    /// List recent sessions
    Sessions {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Print a session transcript
    Transcript {
        #[arg(short = 's', long)]
        session: Option<String>,

        /// Output density
        #[arg(short = 'o', long, value_enum, default_value = "detail")]
        output: OutputFormat,

        /// Emit a browser-viewable xterm.js document
        #[arg(long)]
        html: bool,
    },
    /// Inspect the resolved model and context state
    Status(StatusArgs),
    /// Print the user's mu instructions and skills for a foreign agent
    Context {
        /// Emit the curated projection for another agent to ingest (user
        /// AGENTS.md and non-built-in skills, with an explanatory preamble)
        /// instead of the raw system prompt mu itself would use.
        #[arg(long)]
        export: bool,
    },
    /// Preview the resolved user prompt
    Cat {
        #[arg(value_name = "TARGET")]
        target: Option<PathBuf>,
    },
    /// Resume an interrupted (unclean) turn in a session
    Retry(RetryArgs),
    /// Force compaction for the current or selected session
    Compact {
        #[arg(short = 's', long)]
        session: Option<String>,
    },
}

#[derive(ClapArgs, Debug, Clone)]
struct StatusArgs {
    #[command(flatten)]
    selection: SelectionArgs,

    #[arg(long)]
    json: bool,

    #[arg(long)]
    include_models: bool,

    /// Include Git branch and worktree cleanliness
    #[arg(long)]
    include_git: bool,

    /// Include session counts, activity, and compaction state
    #[arg(long)]
    include_session_details: bool,

    #[arg(long)]
    include_commands: bool,

    #[arg(long)]
    include_skills: bool,
}

const MAX_SUBAGENT_TURN_DEPTH: u32 = 1;
const OUTPUT_FINAL: u8 = 0;
const OUTPUT_CONCISE: u8 = 1;
const OUTPUT_DETAIL: u8 = 2;
const OUTPUT_FULL: u8 = 3;
static RESOLVED_OUTPUT: AtomicU8 = AtomicU8::new(OUTPUT_DETAIL);

/// An error that carries a specific process exit code.
///
/// `main` downcasts to this to map well-known failure classes to the exit
/// codes documented in SPEC §11. Errors without an `ExitError` fall back to
/// the general error code `1`.
#[derive(Debug)]
struct ExitError {
    code: i32,
    message: String,
}

impl ExitError {
    /// A `--session <id>` (or `-c`) that does not resolve in the active scope.
    fn session_not_found(id: &str) -> anyhow::Error {
        anyhow::Error::new(Self {
            code: 2,
            message: format!("session not found in active scope: {id}"),
        })
    }
}

impl fmt::Display for ExitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExitError {}

#[derive(Clone)]
enum PromptSource {
    Stdin,
    File(PathBuf),
    Command {
        path: PathBuf,
        scope: skills::InstructionScope,
    },
}

#[derive(Debug)]
struct LoadedPrompt {
    text: String,
    model: Option<String>,
}

struct RunTurnArgs<'a> {
    config: &'a Config,
    store: &'a store::Store,
    session_id: &'a str,
    model: ResolvedModelChoice,
    output: OutputFormat,
    /// A short notice rendered before the turn (e.g. "resuming interrupted turn").
    preamble_notice: Option<&'a str>,
    model_fallback: Option<runtime::ModelFallback>,
    compact_at_turn_boundary: bool,
}

fn main() {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    if let Some(applet) = applets::from_argv0(&argv0) {
        process::exit(applets::dispatch(applet));
    }

    let result = install::prepare().and_then(|()| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("initializing Mu async runtime")
            .and_then(|runtime| runtime.block_on(run()))
    });
    if let Err(e) = result {
        if error_output_format() == OutputFormat::Final {
            let _ = write_final_error(&e.to_string());
        } else {
            let mut r = Renderer::with_format(error_output_format());
            let _ = r.error(&e.to_string());
        }
        process::exit(exit_code_for(&e));
    }
}

/// Map a fatal error to a process exit code (SPEC §11).
///
/// A forwarded terminating signal wins first (`128 + signal`, so `130` for
/// SIGINT), then any error carrying an explicit `ExitError` code, otherwise the
/// general error code `1`.
fn exit_code_for(error: &anyhow::Error) -> i32 {
    if let Some(signal) = bash::cancellation_signal() {
        return 128 + signal;
    }
    if let Some(exit) = error.downcast_ref::<ExitError>() {
        return exit.code;
    }
    1
}

fn error_output_format() -> OutputFormat {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--output" || arg == "-o" {
            return match args.next().as_deref() {
                Some("final") => OutputFormat::Final,
                Some("concise") => OutputFormat::Concise,
                Some("full") => OutputFormat::Full,
                _ => OutputFormat::Detail,
            };
        }
        if let Some(value) = arg.strip_prefix("--output=") {
            return match value {
                "final" => OutputFormat::Final,
                "concise" => OutputFormat::Concise,
                "full" => OutputFormat::Full,
                _ => OutputFormat::Detail,
            };
        }
        if let Some(value) = arg.strip_prefix("-o").filter(|value| !value.is_empty()) {
            return match value {
                "final" => OutputFormat::Final,
                "concise" => OutputFormat::Concise,
                "full" => OutputFormat::Full,
                _ => OutputFormat::Detail,
            };
        }
    }
    match RESOLVED_OUTPUT.load(Ordering::Relaxed) {
        OUTPUT_FINAL => OutputFormat::Final,
        OUTPUT_CONCISE => OutputFormat::Concise,
        OUTPUT_FULL => OutputFormat::Full,
        _ => OutputFormat::Detail,
    }
}

fn set_resolved_output(format: OutputFormat) {
    let value = match format {
        OutputFormat::Final => OUTPUT_FINAL,
        OutputFormat::Concise => OUTPUT_CONCISE,
        OutputFormat::Detail => OUTPUT_DETAIL,
        OutputFormat::Full => OUTPUT_FULL,
    };
    RESOLVED_OUTPUT.store(value, Ordering::Relaxed);
}

fn resolve_output(explicit: Option<OutputFormat>, config_default: OutputFormat) -> OutputFormat {
    explicit.unwrap_or(config_default)
}

fn write_final_stdout(text: Option<&str>) -> io::Result<()> {
    let Some(text) = text else {
        return Ok(());
    };
    let mut stdout = io::stdout().lock();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

fn write_final_error(message: &str) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "error: {message}")?;
    stdout.flush()
}

fn exit_session_busy(output: OutputFormat) -> ! {
    if output == OutputFormat::Final {
        let _ = write_final_error("session busy");
    } else {
        eprintln!("session busy");
    }
    process::exit(2);
}

fn acquire_session_lock_or_exit<'a>(
    store: &'a store::Store,
    session_id: &str,
    output: OutputFormat,
) -> Result<store::SessionLock<'a>> {
    match store.acquire_session_lock(session_id) {
        Ok(lock) => Ok(lock),
        Err(error) if error.downcast_ref::<store::SessionBusy>().is_some() => {
            exit_session_busy(output)
        }
        Err(error) => Err(error),
    }
}

fn ensure_subagent_turn_allowed(depth: u32) -> Result<()> {
    if depth > MAX_SUBAGENT_TURN_DEPTH {
        bail!("subagent recursion depth exceeded: {depth} (maximum {MAX_SUBAGENT_TURN_DEPTH})");
    }
    Ok(())
}

fn resolve_existing_dir(base: &Path, path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    let path = std::fs::canonicalize(&path)
        .with_context(|| format!("resolving directory {}", path.display()))?;
    if !path.is_dir() {
        bail!("not a directory: {}", path.display());
    }
    Ok(path)
}

fn resolve_session_or_current(
    store: &store::Store,
    session: Option<&str>,
) -> Result<store::Session> {
    if let Some(id) = session {
        return store
            .get_session(id)?
            .ok_or_else(|| ExitError::session_not_found(id));
    }
    store
        .current_session()?
        .ok_or_else(|| anyhow::anyhow!("no sessions found in active scope"))
}

#[derive(Clone, Default)]
struct TranscriptBuffer(Arc<Mutex<Vec<u8>>>);

impl TranscriptBuffer {
    fn text(&self) -> Result<String> {
        Ok(String::from_utf8(
            self.0.lock().expect("transcript buffer poisoned").clone(),
        )?)
    }
}

impl Write for TranscriptBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("transcript buffer poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn replay_transcript(
    renderer: &mut Renderer,
    events: &[store::TranscriptEvent],
    output: OutputFormat,
) -> Result<()> {
    for event in events {
        match event {
            store::TranscriptEvent::User(text) => renderer.transcript_prompt(text)?,
            store::TranscriptEvent::Assistant { turn_state, items } => {
                if output == OutputFormat::Final {
                    if turn_state == "complete" {
                        let text = items
                            .iter()
                            .filter_map(|item| match item {
                                store::TranscriptAssistantItem::Text(text) => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<String>();
                        if !text.is_empty() {
                            renderer.transcript_final_text(&text)?;
                        }
                    }
                    continue;
                }
                for item in items {
                    match item {
                        store::TranscriptAssistantItem::Reasoning(Some(reasoning))
                            if output == OutputFormat::Full && !reasoning.is_empty() =>
                        {
                            renderer
                                .reasoning_start(provider::ReasoningVisibility::StreamedTrace)?;
                            renderer.reasoning_delta(reasoning)?;
                            renderer.reasoning_end(None)?;
                        }
                        store::TranscriptAssistantItem::Text(text) => {
                            renderer.assistant_text(text)?;
                            renderer.assistant_end()?;
                        }
                        store::TranscriptAssistantItem::BashCall { arguments, result } => {
                            let args: serde_json::Value = serde_json::from_str(arguments)
                                .context("parsing persisted Bash arguments")?;
                            renderer.bash_header_full(&args)?;
                            renderer.tool_start(&args, true)?;
                            match result {
                                Some(result) if result.outcome == "completed" => {
                                    renderer.bash_output(&result.output)?;
                                    renderer.tool_finished(
                                        result
                                            .exit_code
                                            .context("completed Bash result has no exit code")?,
                                        Duration::from_millis(
                                            result.duration_ms.unwrap_or_default(),
                                        ),
                                    )?;
                                }
                                Some(result) => renderer.tool_failed(
                                    result
                                        .output
                                        .strip_prefix("error: ")
                                        .unwrap_or(&result.outcome),
                                    Duration::from_millis(result.duration_ms.unwrap_or_default()),
                                )?,
                                None => renderer.tool_failed("incomplete", Duration::ZERO)?,
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    Ok(())
}

const TRANSCRIPT_HTML_COLUMNS: usize = 100;

fn transcript_html(ansi: &str) -> Result<String> {
    let transcript = serde_json::to_string(ansi)?.replace('<', "\\u003c");
    Ok(format!(
        r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Mu transcript</title>
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/css/xterm.css">
<style>
html,body,#terminal {{ height:100%; margin:0 }}
body {{ box-sizing:border-box; padding:16px; background:#000 }}
#terminal {{ max-width:100%; overflow:hidden }}
</style>
</head>
<body>
<div id="terminal"></div>
<script src="https://cdn.jsdelivr.net/npm/@xterm/xterm@5.5.0/lib/xterm.js"></script>
<script>
const term = new Terminal({{
  cols:{TRANSCRIPT_HTML_COLUMNS}, rows:30, convertEol:true, disableStdin:true, scrollback:100000,
  fontFamily:"ui-monospace,SFMono-Regular,Menlo,Consolas,monospace",
  theme:{{background:"#000000"}}
}});
term.open(document.getElementById("terminal"));
term.write({transcript});
</script>
</body>
</html>
"##
    ))
}

async fn run() -> Result<()> {
    let args = Args::parse();
    validate_cli_args(&args)?;
    let cwd = std::env::current_dir()?;
    let scope = paths::discover_scope(&cwd);
    let project_config_dir = scope.project().map(|p| p.root.join(".mu"));
    let default_turn = args.turn;
    let prompt_file = args.prompt_file;

    match args.command {
        Some(Command::Init { path, force }) => {
            let root = resolve_existing_dir(&cwd, path.as_deref().unwrap_or(&cwd))?;
            let result = paths::init_project_layout_at(&root, force)?;
            println!("path: {}", result.root.display());
            println!("project_root: {}", result.root.display());
            println!("already_initialized: {}", result.already_initialized);
            println!(
                "created_files: {}",
                if result.created_files.is_empty() {
                    "(none)".into()
                } else {
                    result.created_files.join(", ")
                }
            );
            return Ok(());
        }
        Some(Command::New) => {
            let store_path = scope.session_store_path();
            paths::ensure_project_layout(&scope)?;
            let store = store::Store::open(&store_path)?;
            let session = store.create_session_seeded(&system_prompt::build_system_prompt(
                &paths::global_dir(),
                project_config_dir.as_deref(),
            )?)?;
            println!("{}", session.id);
            return Ok(());
        }
        Some(Command::Sessions { limit }) => {
            let store_path = scope.session_store_path();
            if !store_path.join("sessions").exists() {
                return Ok(());
            }
            let store = store::Store::open(&store_path)?;
            let sessions = store.list_sessions(limit)?;
            for (s, updated) in sessions {
                let title = s.title.unwrap_or_else(|| "(untitled)".into());
                let model = s.last_model.unwrap_or_else(|| "-".into());
                println!("{}  {}  {}  {}", s.id, title, model, updated);
            }
            return Ok(());
        }
        Some(Command::Transcript {
            session,
            output,
            html,
        }) => {
            let store_path = scope.session_store_path();
            if !store_path.exists() {
                return Err(session.as_deref().map_or_else(
                    || anyhow::anyhow!("no sessions found in active scope"),
                    ExitError::session_not_found,
                ));
            }
            let store = store::Store::open(&store_path)?;
            let session = resolve_session_or_current(&store, session.as_deref())?;
            let events = store.transcript_events(&session.id)?;
            if html {
                let buffer = TranscriptBuffer::default();
                let mut renderer = Renderer::with_transcript_output(
                    output,
                    Box::new(buffer.clone()),
                    TRANSCRIPT_HTML_COLUMNS - 1,
                );
                replay_transcript(&mut renderer, &events, output)?;
                let html = transcript_html(&buffer.text()?)?;
                io::stdout().write_all(html.as_bytes())?;
            } else {
                let mut renderer = Renderer::with_format(output);
                replay_transcript(&mut renderer, &events, output)?;
            }
            return Ok(());
        }
        Some(Command::Status(status_args)) => {
            let config = Config::load_for_scope(project_config_dir.as_deref())?;
            let store_path = scope.session_store_path();
            let store = if store_path.exists() {
                store::Store::open(&store_path)?
            } else {
                store::Store::open_memory()?
            };
            let index = if status_args.include_commands || status_args.include_skills {
                Some(skills::scan_instruction_index_with_env(
                    &paths::global_dir(),
                    project_config_dir.as_deref(),
                    &config.env,
                )?)
            } else {
                None
            };
            let commands = status_args
                .include_commands
                .then(|| index.as_ref().map(|index| index.commands.clone()))
                .flatten();
            let skills = status_args
                .include_skills
                .then(|| index.as_ref().map(|index| index.skills.clone()))
                .flatten();
            let report = build_status_report(
                &store,
                &config,
                &InvocationOverrides {
                    session: status_args.selection.session,
                    continue_current: status_args.selection.continue_current,
                    model: status_args.selection.model,
                },
                scope.project(),
                StatusIncludes {
                    git: status_args.include_git || !status_args.json,
                    session_details: status_args.include_session_details || !status_args.json,
                    models: status_args.include_models,
                },
                commands,
                skills,
            )?;
            if status_args.json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                print_status_report(&report);
            }
            return Ok(());
        }
        Some(Command::Context { export }) => {
            // Introspection only: no provider, and no config load. Both builders
            // scan the instruction index and read AGENTS.md directly, which
            // tolerate a missing ~/.mu, so this works in any directory.
            let context = if export {
                system_prompt::build_context(&paths::global_dir(), project_config_dir.as_deref())?
            } else {
                system_prompt::build_system_prompt(
                    &paths::global_dir(),
                    project_config_dir.as_deref(),
                )?
            };
            if !context.is_empty() {
                println!("{}", context);
            }
            return Ok(());
        }
        Some(Command::Cat { target }) => {
            let prompt_source = resolve_prompt_source(target, &scope)?;
            let provenance_source = prompt_source.clone();
            let prompt = load_prompt(prompt_source)?;
            let provenance =
                prompt_source_provenance(&provenance_source, &cwd, prompt.model.as_deref());
            Renderer::new().markdown_document(&provenance, &prompt.text)?;
            return Ok(());
        }
        Some(Command::Retry(retry_args)) => {
            ensure_subagent_turn_allowed(bash::subagent_depth_from_env())?;
            let config = Config::load_for_scope(project_config_dir.as_deref())?;
            let output = resolve_output(retry_args.output, config.output);
            set_resolved_output(output);

            paths::ensure_project_layout(&scope)?;
            let state_dir = scope.state_dir();
            paths::ensure_dir(&state_dir)?;

            let store_path = scope.session_store_path();
            let store = store::Store::open(&store_path)?;
            let session = resolve_retry_session(&store, &retry_args)?
                .ok_or_else(|| anyhow::anyhow!("no sessions found in active scope"))?;
            let _lock = acquire_session_lock_or_exit(&store, &session.id, output)?;
            store.normalize_interrupted_tail(&session.id)?;

            // Nothing to resume on a session whose last turn already finished.
            if store.is_session_clean(&session.id)? {
                if output != OutputFormat::Final {
                    println!("session is already complete; nothing to retry");
                }
                return Ok(());
            }

            store.select_session(&session.id)?;
            std::env::set_current_dir(&session.cwd).with_context(|| {
                format!(
                    "restoring submitted working directory for retry: {}",
                    session.cwd
                )
            })?;

            let selection = resolve_retry_model_selection(
                &store,
                &config,
                &session,
                retry_args.selection.model.as_deref(),
            )?;

            run_turn(RunTurnArgs {
                config: &config,
                store: &store,
                session_id: &session.id,
                model: selection.model,
                output,
                preamble_notice: Some("[mu] resuming incomplete turn"),
                model_fallback: selection.fallback,
                compact_at_turn_boundary: false,
            })
            .await?;

            return Ok(());
        }
        Some(Command::Compact { session }) => {
            let custom_focus = load_optional_stdin_instruction()?;
            let config = Config::load_for_scope(project_config_dir.as_deref())?;
            let store_path = scope.session_store_path();
            if !store_path.join("sessions").exists() {
                return Err(session.as_deref().map_or_else(
                    || anyhow::anyhow!("no sessions found in active scope"),
                    ExitError::session_not_found,
                ));
            }
            let store = store::Store::open(&store_path)?;
            let session_state = resolve_session_or_current(&store, session.as_deref())?;
            let session = session_state.id.clone();
            let mut model = resolve_session_model(&store, &config, &session_state)?;
            let mut provider = build_provider(&config, &model.active_model().provider_id)?;
            let _lock = acquire_session_lock_or_exit(&store, &session, OutputFormat::Detail)?;
            store.normalize_interrupted_tail(&session)?;
            let mut renderer = Renderer::with_terminal_bell(OutputFormat::Detail, None);
            let started = Instant::now();
            let outcome = compaction::run_compaction_routed(
                &store,
                &config,
                &session,
                &mut model,
                &mut provider,
                custom_focus.as_deref(),
                Some(&mut renderer),
            )
            .await?;
            let model_info = models::resolve_model_info(&config, model.active_model());
            match outcome {
                compaction::CompactionOutcome::Applied {
                    before_context_tokens,
                    after_context_tokens_estimate,
                } => renderer.compaction_result(
                    before_context_tokens,
                    after_context_tokens_estimate,
                    model_info.context_window,
                    started.elapsed(),
                )?,
                compaction::CompactionOutcome::Inapplicable { keep_recent_turns } => {
                    renderer.compaction_inapplicable(keep_recent_turns)?
                }
            }
            return Ok(());
        }
        None => {}
    }

    ensure_subagent_turn_allowed(bash::subagent_depth_from_env())?;
    let config = Config::load_for_scope(project_config_dir.as_deref())?;
    let output = resolve_output(default_turn.output, config.output);
    set_resolved_output(output);
    let prompt_source = resolve_prompt_source(prompt_file, &scope)?;
    run_turn_from_source(
        &cwd,
        &scope,
        project_config_dir.as_deref(),
        &config,
        default_turn,
        output,
        prompt_source,
    )
    .await
}

fn validate_cli_args(args: &Args) -> Result<()> {
    let has_turn_options = args.turn.selection.session.is_some()
        || args.turn.selection.continue_current
        || args.turn.selection.model.is_some()
        || !args.turn.attachments.is_empty()
        || args.turn.output.is_some();
    let reserved_prompt = args
        .prompt_file
        .as_ref()
        .and_then(|path| path.to_str())
        .is_some_and(|path| path == "help" || Command::has_subcommand(path));
    if has_turn_options && (args.command.is_some() || reserved_prompt) {
        bail!("turn arguments must not precede a management command");
    }
    Ok(())
}

async fn run_turn_from_source(
    cwd: &Path,
    scope: &paths::Scope,
    project_config_dir: Option<&Path>,
    config: &Config,
    turn: TurnArgs,
    output: OutputFormat,
    prompt_source: PromptSource,
) -> Result<()> {
    let loaded_prompt = load_prompt(prompt_source)?;
    let prompt = loaded_prompt.text;
    let attachments = load_attachments(&turn.attachments)?;

    paths::ensure_project_layout(scope)?;
    let state_dir = scope.state_dir();
    paths::ensure_dir(&state_dir)?;

    let store_path = scope.session_store_path();
    let store = store::Store::open(&store_path)?;
    let resolved = resolve_invocation(
        &store,
        config,
        &InvocationOverrides {
            session: turn.selection.session.clone(),
            continue_current: turn.selection.continue_current,
            model: turn.selection.model.clone().or(loaded_prompt.model),
        },
    )?;
    let session = if let Some(session) = resolved.attached_session.clone() {
        session
    } else {
        store.create_session_seeded(&system_prompt::build_system_prompt(
            &paths::global_dir(),
            project_config_dir,
        )?)?
    };
    let session_id = session.id.clone();

    let _lock = acquire_session_lock_or_exit(&store, &session_id, output)?;

    // If the previous turn was interrupted, normalize its tail (synthesize
    // interrupted results for any dangling tool calls) so history is valid.
    // The new prompt then lands on top of that valid history — the user can
    // redirect after a Ctrl-C without being forced to `mu retry` first.
    store.normalize_interrupted_tail(&session_id)?;

    let prompt_content = build_prompt_content(&prompt, attachments);
    let git_worktree_root = scope
        .project()
        .and_then(|project| project.worktree.as_ref())
        .map(|worktree| worktree.root.display().to_string());
    store.start_turn(
        &session_id,
        &cwd.display().to_string(),
        git_worktree_root.as_deref(),
        &prompt_content,
    )?;
    // Publish the session only after its journal lock is held and its first
    // turn is durable. Standalone `new` deliberately does not select.
    store.select_session(&session_id)?;

    run_turn(RunTurnArgs {
        config,
        store: &store,
        session_id: &session_id,
        model: resolved.model,
        output,
        preamble_notice: None,
        model_fallback: resolved.model_fallback,
        compact_at_turn_boundary: true,
    })
    .await?;

    Ok(())
}

fn load_prompt(source: PromptSource) -> Result<LoadedPrompt> {
    let stdin = io::stdin();
    load_prompt_with_stdin(source, stdin.is_terminal(), &mut stdin.lock())
}

fn load_prompt_with_stdin(
    source: PromptSource,
    stdin_is_terminal: bool,
    stdin: &mut impl Read,
) -> Result<LoadedPrompt> {
    match source {
        PromptSource::Stdin => {
            let mut prompt = String::new();
            stdin.read_to_string(&mut prompt)?;
            Ok(LoadedPrompt {
                text: normalize_prompt(&prompt, false)?,
                model: None,
            })
        }
        PromptSource::File(path) => {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading prompt file {}", path.display()))?;
            let model = skills::parse_mu_shebang(raw.lines().next().unwrap_or_default())
                .with_context(|| format!("invalid prompt file {} shebang", path.display()))?
                .and_then(|shebang| shebang.model);
            let prompt = normalize_prompt(&raw, true)?;
            Ok(LoadedPrompt {
                text: append_stdin_instruction(prompt, stdin_is_terminal, stdin)?,
                model,
            })
        }
        PromptSource::Command { path, .. } => {
            let prompt = skills::command_prompt(&path)?;
            Ok(LoadedPrompt {
                text: append_stdin_instruction(prompt.text, stdin_is_terminal, stdin)?,
                model: prompt.model,
            })
        }
    }
}

fn load_optional_stdin_instruction() -> Result<Option<String>> {
    let stdin = io::stdin();
    read_optional_stdin_instruction(stdin.is_terminal(), &mut stdin.lock())
}

fn read_optional_stdin_instruction(
    stdin_is_terminal: bool,
    stdin: &mut impl Read,
) -> Result<Option<String>> {
    if stdin_is_terminal {
        return Ok(None);
    }

    let mut instruction = String::new();
    stdin.read_to_string(&mut instruction)?;
    Ok((!instruction.is_empty()).then_some(instruction))
}

fn append_stdin_instruction(
    prompt: String,
    stdin_is_terminal: bool,
    stdin: &mut impl Read,
) -> Result<String> {
    match read_optional_stdin_instruction(stdin_is_terminal, stdin)? {
        Some(instruction) => Ok(format!("{prompt}\n---\n\n{instruction}")),
        None => Ok(prompt),
    }
}

fn resolve_prompt_source(
    prompt_file: Option<PathBuf>,
    scope: &paths::Scope,
) -> Result<PromptSource> {
    let Some(path) = prompt_file else {
        return Ok(PromptSource::Stdin);
    };
    if is_explicit_prompt_path(&path) {
        return Ok(PromptSource::File(path));
    }
    let name = path.display().to_string();
    let project_config_dir = scope.project().map(|project| project.root.join(".mu"));
    let index =
        skills::scan_instruction_index(&paths::global_dir(), project_config_dir.as_deref())?;
    if let Some(command) = index.commands.iter().find(|command| command.name == name) {
        return Ok(PromptSource::Command {
            path: PathBuf::from(&command.path),
            scope: command.scope,
        });
    }
    Ok(PromptSource::File(path))
}

fn prompt_source_provenance(source: &PromptSource, cwd: &Path, model: Option<&str>) -> String {
    let mut provenance = match source {
        PromptSource::Stdin => "[stdin]".to_string(),
        PromptSource::File(path) => {
            format!("[prompt file] {}", display_prompt_path(path, cwd).display())
        }
        PromptSource::Command { path, scope } => {
            let kind = match scope {
                skills::InstructionScope::Builtin => "builtin command",
                skills::InstructionScope::Global => "global command",
                skills::InstructionScope::Project => "project command",
            };
            format!("[{kind}] {}", display_prompt_path(path, cwd).display())
        }
    };
    if let Some(model) = model {
        provenance.push_str(&format!(" (model default: {model})"));
    }
    provenance
}

fn display_prompt_path(path: &Path, cwd: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    })
}

fn is_explicit_prompt_path(path: &Path) -> bool {
    path.is_absolute()
        || path
            .components()
            .next()
            .is_some_and(|component| matches!(component, std::path::Component::ParentDir))
        || path.to_string_lossy().starts_with("./")
}

fn normalize_prompt(raw: &str, trim_shebang: bool) -> Result<String> {
    let raw = if trim_shebang {
        trim_shebang_line(raw)
    } else {
        raw
    };
    let prompt = trim_trailing_newlines(raw).to_string();
    if prompt.is_empty() {
        bail!("empty prompt");
    }
    Ok(prompt)
}

fn trim_shebang_line(text: &str) -> &str {
    if !text.starts_with("#!") {
        return text;
    }
    match text.find('\n') {
        Some(idx) => &text[idx + 1..],
        None => "",
    }
}

fn trim_trailing_newlines(text: &str) -> &str {
    text.trim_end_matches(['\r', '\n'])
}

async fn run_turn(args: RunTurnArgs<'_>) -> Result<()> {
    let RunTurnArgs {
        config,
        store,
        session_id,
        model,
        output,
        preamble_notice,
        model_fallback,
        compact_at_turn_boundary,
    } = args;
    let active_model = model.active_model();
    let model_context_window = models::resolve_model_info(config, active_model).context_window;
    let provider = build_provider(config, &active_model.provider_id)?;

    let turn_done_bell_min_duration = config
        .terminal_bell
        .enabled
        .then_some(Duration::from_millis(config.terminal_bell.min_duration_ms));
    let mut renderer = Renderer::with_terminal_bell(output, turn_done_bell_min_duration);
    let turn_started = Instant::now();
    if let Some(notice) = preamble_notice {
        renderer.notice(notice)?;
    }
    if let Some(fallback) = model_fallback {
        renderer.notice(&format!(
            "[mu] remembered model {} is no longer configured; using {}",
            fallback.remembered, fallback.selected
        ))?;
    }
    let mut agent = agent::AgentLoop {
        config,
        model,
        provider,
        store,
        session_id,
        cache_key: Some(format!("mu:{session_id}:agent")),
        model_context_window,
        renderer: &mut renderer,
    };

    let result = if compact_at_turn_boundary {
        agent.run_turn().await
    } else {
        agent.resume_turn().await
    };

    match &result {
        Ok(r) => {
            let ctx_pct = r
                .context_window
                .map(|cw| (r.context_tokens as f64 / cw as f64) * 100.0);
            renderer.finish_turn()?;
            if output == OutputFormat::Final {
                write_final_stdout(r.final_assistant.as_deref())?;
            } else {
                let turn_elapsed = turn_started.elapsed();
                renderer.turn_summary(
                    r.usage.visible_input_tokens(),
                    r.usage.cache_read_input_tokens,
                    r.usage.cache_write_input_tokens,
                    r.usage.visible_output_tokens(),
                    ctx_pct,
                    turn_elapsed,
                )?;
                renderer.turn_done_bell(turn_elapsed)?;
            }
        }
        Err(error) => {
            // Nothing to clean up: the log ends at the last landed message.
            // A resumable completion is deliberately persisted but remains
            // unclean; other failures may leave an interrupted request or Bash
            // claim for the next turn or `mu retry` to normalize.
            if output != OutputFormat::Final {
                if let Some(exhausted) = error.downcast_ref::<agent::AutoResumeExhausted>() {
                    renderer.turn_auto_resume_exhausted(exhausted.limit() as u64)?;
                } else {
                    renderer.turn_interrupted(&error.to_string())?;
                }
            }
        }
    }

    result.map(|_| ())
}

fn build_prompt_content(prompt: &str, attachments: Vec<ContentPart>) -> UserContent {
    if attachments.is_empty() {
        return UserContent::Text(prompt.to_string());
    }
    let mut parts = vec![ContentPart::Text {
        text: prompt.to_string(),
    }];
    parts.extend(attachments);
    UserContent::Parts(parts)
}

fn resolve_retry_session(
    store: &store::Store,
    retry: &RetryArgs,
) -> Result<Option<store::Session>> {
    if retry.selection.session.is_some() && retry.selection.continue_current {
        bail!("use either -s/--session or -c/--continue, not both");
    }
    if let Some(id) = retry.selection.session.as_deref() {
        return Ok(Some(
            store
                .get_session(id)?
                .ok_or_else(|| ExitError::session_not_found(id))?,
        ));
    }
    store.current_session()
}

fn print_status_report(report: &StatusReport) {
    let session = report
        .session_id
        .clone()
        .unwrap_or_else(|| "(new session)".into());
    let context = match (report.context_tokens, report.context_window) {
        (Some(tokens), Some(window)) => format!(
            "{}{tokens} / {window} ({:.2}%)",
            if report.context_usage_source == Some(runtime::ContextUsageSource::Estimated) {
                "~"
            } else {
                ""
            },
            tokens as f64 / window as f64 * 100.0,
        ),
        _ => "n/a".into(),
    };
    let project = report
        .project_root
        .clone()
        .unwrap_or_else(|| "(global)".into());
    let effort_levels = if report.supported_effort_levels.is_empty() {
        "(none)".into()
    } else {
        report
            .supported_effort_levels
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };

    println!("model: {}", report.model.canonical);
    println!("replay key: {}", report.model.replay_key);
    println!("session: {session}");
    println!("context: {context}");
    println!("project: {project}");
    if let Some(git) = &report.git
        && let Some(branch) = &git.branch
    {
        println!(
            "git: {}{}",
            branch,
            if git.dirty.unwrap_or(false) {
                " (dirty)"
            } else {
                " (clean)"
            }
        );
    }
    if let Some(session) = &report.session {
        println!(
            "turns: {}  messages: {}  updated: {}",
            session.turn_count, session.message_count, session.updated_at
        );
    }
    if report.active.as_ref().is_some_and(|active| active.busy) {
        println!("active: busy");
    }
    if report.session_id.is_some() && !report.clean {
        println!("clean: no (last turn interrupted)");
        println!("retry: mu retry");
    }
    println!("supported effort levels: {effort_levels}");
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_file_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mu-{name}-{nanos}.tmp"))
    }

    #[test]
    fn load_prompt_file_trims_shebang_line() {
        let path = temp_file_path("shebang");
        std::fs::write(
            &path,
            "#!/usr/bin/env -S mu --model openai/gpt-5:high\nhello\n",
        )
        .unwrap();
        let mut stdin = Cursor::new("ignored instruction");
        let prompt =
            load_prompt_with_stdin(PromptSource::File(path.clone()), true, &mut stdin).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(prompt.text, "hello");
        assert_eq!(prompt.model.as_deref(), Some("openai/gpt-5:high"));
        assert_eq!(stdin.position(), 0);
    }

    #[test]
    fn prompt_file_rejects_other_mu_shebang_arguments() {
        let path = temp_file_path("invalid-shebang");
        std::fs::write(&path, "#!/usr/bin/env -S mu --output detail\nhello\n").unwrap();
        let mut stdin = Cursor::new("");
        let error =
            load_prompt_with_stdin(PromptSource::File(path.clone()), true, &mut stdin).unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert!(error.to_string().contains("invalid prompt file"));
        assert!(format!("{error:#}").contains("unsupported mu shebang arguments"));
    }

    #[test]
    fn explicit_output_overrides_config_default() {
        assert_eq!(
            resolve_output(None, OutputFormat::Concise),
            OutputFormat::Concise
        );
        assert_eq!(
            resolve_output(Some(OutputFormat::Full), OutputFormat::Concise),
            OutputFormat::Full
        );
    }

    #[test]
    fn flattened_commands_use_command_local_arguments() {
        let compact = Args::try_parse_from(["mu", "compact"]).unwrap();
        assert!(matches!(
            compact.command,
            Some(Command::Compact { session: None })
        ));
        let selected_compact =
            Args::try_parse_from(["mu", "compact", "-s", "ses_example"]).unwrap();
        assert!(matches!(
            selected_compact.command,
            Some(Command::Compact {
                session: Some(ref session),
            }) if session == "ses_example"
        ));

        let transcript =
            Args::try_parse_from(["mu", "transcript", "-s", "ses_example", "-o", "full"]).unwrap();
        assert!(matches!(
            transcript.command,
            Some(Command::Transcript {
                session: Some(ref session),
                output: OutputFormat::Full,
                html: false,
            }) if session == "ses_example"
        ));

        assert!(Args::try_parse_from(["mu", "session", "list"]).is_err());
        assert!(Args::try_parse_from(["mu", "new", "--model", "gpt-5"]).is_err());
        assert!(Args::try_parse_from(["mu", "-s", "ses_example", "-c"]).is_err());
        assert!(
            Args::try_parse_from(["mu", "status", "--session", "ses_example", "--continue"])
                .is_err()
        );

        let misplaced = Args::try_parse_from(["mu", "-m", "gpt-5", "new"]).unwrap();
        assert!(validate_cli_args(&misplaced).is_err());
    }

    #[test]
    fn prompt_file_appends_non_terminal_stdin_verbatim() {
        let path = temp_file_path("instruction");
        std::fs::write(&path, "Use the release-note format.\n").unwrap();
        let mut stdin = Cursor::new("Focus on auth.\nKeep the second line.\n");
        let prompt =
            load_prompt_with_stdin(PromptSource::File(path.clone()), false, &mut stdin).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            prompt.text,
            "Use the release-note format.\n---\n\nFocus on auth.\nKeep the second line.\n"
        );
        assert_eq!(prompt.model, None);
    }

    #[test]
    fn stdin_prompt_source_uses_stdin_as_the_complete_prompt() {
        let mut stdin = Cursor::new("# Standalone prompt\n\nBody.\n");
        let prompt = load_prompt_with_stdin(PromptSource::Stdin, false, &mut stdin).unwrap();

        assert_eq!(prompt.text, "# Standalone prompt\n\nBody.");
        assert_eq!(prompt.model, None);
    }

    #[test]
    fn optional_instruction_uses_custom_command_stdin_rules() {
        let mut terminal_stdin = Cursor::new("do not read");
        assert_eq!(
            read_optional_stdin_instruction(true, &mut terminal_stdin).unwrap(),
            None
        );
        assert_eq!(terminal_stdin.position(), 0);

        let mut empty_stdin = Cursor::new("");
        assert_eq!(
            read_optional_stdin_instruction(false, &mut empty_stdin).unwrap(),
            None
        );

        let mut piped_stdin = Cursor::new("Focus on auth.\nKeep details.\n");
        assert_eq!(
            read_optional_stdin_instruction(false, &mut piped_stdin).unwrap(),
            Some("Focus on auth.\nKeep details.\n".to_string())
        );
    }

    #[test]
    fn command_appends_non_terminal_stdin_after_headers_are_stripped() {
        let path = temp_file_path("command-instruction");
        std::fs::write(
            &path,
            "#!/usr/bin/env -S mu --model openai/gpt-5:high\n---\nname: review\ndescription: Review changes.\n---\nReview the checkout.\n",
        )
        .unwrap();
        let mut stdin = Cursor::new("Focus on auth.");
        let prompt = load_prompt_with_stdin(
            PromptSource::Command {
                path: path.clone(),
                scope: skills::InstructionScope::Project,
            },
            false,
            &mut stdin,
        )
        .unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(prompt.text, "Review the checkout.\n---\n\nFocus on auth.");
        assert_eq!(prompt.model.as_deref(), Some("openai/gpt-5:high"));
    }

    #[test]
    fn explicit_prompt_paths_bypass_command_lookup() {
        let scope = paths::Scope::Global;
        for path in ["./review.md", "../review.md", "/tmp/review.md"] {
            assert!(matches!(
                resolve_prompt_source(Some(PathBuf::from(path)), &scope).unwrap(),
                PromptSource::File(resolved) if resolved.as_path() == Path::new(path)
            ));
        }
    }

    #[test]
    fn bare_prompt_name_resolves_to_project_command() {
        let root = temp_file_path("resolve-project-command");
        let command_dir = root.join(".mu");
        let command_path = command_dir.join("preview-command.md");
        std::fs::create_dir_all(&command_dir).unwrap();
        std::fs::write(
            &command_path,
            "#!/usr/bin/env mu\nPreview the current checkout.\n",
        )
        .unwrap();
        let scope = paths::Scope::Project(paths::Project {
            root: root.clone(),
            marker: paths::ProjectMarker::Mu,
            worktree: None,
        });

        let resolved =
            resolve_prompt_source(Some(PathBuf::from("preview-command.md")), &scope).unwrap();

        assert!(matches!(
            resolved,
            PromptSource::Command {
                path,
                scope: skills::InstructionScope::Project,
            } if path == command_path.canonicalize().unwrap()
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_oversized_attachment_before_reading_it() {
        let path = std::env::temp_dir().join(format!("mu-oversized-{}.wav", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_ATTACHMENT_BYTES + 1).unwrap();
        drop(file);
        let error = load_attachments(std::slice::from_ref(&path)).unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert!(error.to_string().contains("exceeds 20 MiB limit"));
    }

    #[test]
    fn exit_code_maps_session_not_found_to_two() {
        bash::reset_cancellation_state();
        let err = ExitError::session_not_found("abc123");
        assert_eq!(exit_code_for(&err), 2);
        assert!(
            err.to_string()
                .contains("session not found in active scope: abc123")
        );
    }

    #[test]
    fn session_selection_defaults_to_current_session() {
        let store = store::Store::open_memory().unwrap();
        let first = store.create_session("/tmp").unwrap();
        let second = store.create_session("/tmp").unwrap();

        assert!(resolve_session_or_current(&store, None).is_err());
        store.select_session(&second.id).unwrap();
        assert_eq!(
            resolve_session_or_current(&store, None).unwrap().id,
            second.id
        );
        assert_eq!(
            resolve_session_or_current(&store, Some(&first.id))
                .unwrap()
                .id,
            first.id
        );
    }

    #[test]
    fn final_transcript_keeps_only_completed_assistant_text() {
        let events = vec![
            store::TranscriptEvent::User("Question".into()),
            store::TranscriptEvent::Assistant {
                turn_state: "continue".into(),
                items: vec![store::TranscriptAssistantItem::Text("Intermediate".into())],
            },
            store::TranscriptEvent::Assistant {
                turn_state: "complete".into(),
                items: vec![
                    store::TranscriptAssistantItem::Text("Final ".into()),
                    store::TranscriptAssistantItem::Reasoning(None),
                    store::TranscriptAssistantItem::Text("answer".into()),
                ],
            },
        ];
        let buffer = TranscriptBuffer::default();
        let mut renderer =
            Renderer::with_transcript_output(OutputFormat::Final, Box::new(buffer.clone()), 79);

        replay_transcript(&mut renderer, &events, OutputFormat::Final).unwrap();

        assert_eq!(buffer.text().unwrap(), "mu> Question\n\nFinal answer\n");
    }

    #[test]
    fn full_transcript_replays_reasoning_and_complete_bash_output() {
        let events = vec![
            store::TranscriptEvent::User("Question".into()),
            store::TranscriptEvent::Assistant {
                turn_state: "continue".into(),
                items: vec![
                    store::TranscriptAssistantItem::Reasoning(Some("Private trace".into())),
                    store::TranscriptAssistantItem::Text("Running **now**.".into()),
                    store::TranscriptAssistantItem::BashCall {
                        arguments:
                            r#"{"title":"Inspect","command":"printf all","risk":"readonly"}"#.into(),
                        result: Some(store::TranscriptBashResult {
                            outcome: "completed".into(),
                            output: "line1\nline2\nline3\nline4\nline5\nline6\n".into(),
                            exit_code: Some(0),
                            duration_ms: Some(5),
                        }),
                    },
                ],
            },
        ];
        let buffer = TranscriptBuffer::default();
        let mut renderer =
            Renderer::with_transcript_output(OutputFormat::Full, Box::new(buffer.clone()), 79);

        replay_transcript(&mut renderer, &events, OutputFormat::Full).unwrap();

        let transcript = buffer.text().unwrap();
        assert!(transcript.contains("Private trace"));
        assert!(transcript.contains("Running "));
        assert!(transcript.contains("line1\nline2\nline3\nline4\nline5\nline6\n"));
        assert!(!transcript.contains("omitted"));
    }

    #[test]
    fn transcript_html_uses_pinned_xterm_and_escapes_embedded_script_end() {
        let html = transcript_html("</script>\n").unwrap();

        assert!(html.contains("@xterm/xterm@5.5.0"));
        assert!(html.contains(r#"term.write("\u003c/script>\n")"#));
    }

    #[test]
    fn subagent_turn_guard_rejects_grandchild_turns() {
        assert!(ensure_subagent_turn_allowed(0).is_ok());
        assert!(ensure_subagent_turn_allowed(1).is_ok());
        let err = ensure_subagent_turn_allowed(2).unwrap_err();
        assert!(
            err.to_string()
                .contains("subagent recursion depth exceeded")
        );
    }
}
