use std::borrow::Cow;
use std::fmt;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

mod agent;
mod anthropic;
mod applets;
mod attachment;
mod bash;
mod chat_completions;
mod compaction;
mod config;
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
use config::{Config, ConfigLoadMode};
use models::ResolvedModelChoice;
use provider::build_provider;
use provider::{ContentPart, UserContent};
use renderer::{CompactionReport, Renderer};
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

    /// Trap Bash calls at this declared risk level
    #[arg(long, value_enum)]
    trap: Option<bash::TrapLevel>,
}

#[derive(ClapArgs, Debug, Clone)]
struct RetryArgs {
    #[command(flatten)]
    selection: SelectionArgs,

    /// Output density (overrides config)
    #[arg(short = 'o', long, value_enum)]
    output: Option<OutputFormat>,

    /// Override the persisted trap level for this retry
    #[arg(long, value_enum)]
    trap: Option<bash::TrapLevel>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Deserialize, Serialize)]
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

        /// Output density (overrides config)
        #[arg(short = 'o', long, value_enum)]
        output: Option<OutputFormat>,

        /// Show only activity sent under one context epoch
        #[arg(long)]
        epoch: Option<u64>,
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

        /// Output density (overrides config)
        #[arg(short = 'o', long, value_enum)]
        output: Option<OutputFormat>,

        /// Trap Bash calls at this declared risk level
        #[arg(long, value_enum)]
        trap: Option<bash::TrapLevel>,
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

#[derive(Debug)]
struct TrappedExit;

impl fmt::Display for TrappedExit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Bash command trapped before execution")
    }
}

impl std::error::Error for TrappedExit {}

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
    project_config_dir: Option<&'a Path>,
    store: &'a store::Store,
    session_id: &'a str,
    model: ResolvedModelChoice,
    output: OutputFormat,
    trap: bash::TrapLevel,
    /// A short notice rendered before the turn (e.g. "resuming interrupted turn").
    preamble_notice: Option<&'a str>,
    model_fallback: Option<runtime::ModelFallback>,
    mode: RunTurnMode<'a>,
}

enum RunTurnMode<'a> {
    QueuedPrompt,
    Resume,
    ManualCompaction(Option<&'a str>),
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
        if e.downcast_ref::<TrappedExit>().is_none() {
            if error_output_format() == OutputFormat::Final {
                let _ = write_final_error(&e.to_string());
            } else {
                let mut r = Renderer::with_format(error_output_format());
                let _ = r.error(&e.to_string());
            }
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
    if error.downcast_ref::<TrappedExit>().is_some() {
        return 3;
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

fn warn_unsupported_session(error: &store::UnsupportedSessionVersion, action: &str) {
    eprintln!("[mu] warning: {error}; {action}");
}

fn bash_output_for_transcript(output: &str, exit_code: i32) -> Cow<'_, str> {
    let marker = format!("\n[exit code: {exit_code}]");
    if let Some(output) = output.strip_suffix(&marker) {
        return Cow::Borrowed(output);
    }

    let Some(note_start) = output.rfind("\n[… ") else {
        return Cow::Borrowed(output);
    };
    let (preview, note) = output.split_at(note_start);
    let Some(preview) = preview.strip_suffix(&marker) else {
        return Cow::Borrowed(output);
    };
    let note = if preview.is_empty() {
        note.trim_start_matches('\n')
    } else {
        note
    };
    Cow::Owned(format!("{preview}{note}"))
}

fn replay_transcript(
    renderer: &mut Renderer,
    events: &[store::TranscriptEvent],
    output: OutputFormat,
    context_window: impl Fn(&str) -> Option<u64>,
) -> Result<()> {
    for event in events {
        match event {
            store::TranscriptEvent::User {
                text,
                cwd,
                model,
                context,
                internal,
            } => {
                if output == OutputFormat::Final && *internal {
                    continue;
                }
                let context_percent = context.as_ref().and_then(|context| {
                    context_window(model.as_deref()?)
                        .filter(|window| *window > 0)
                        .map(|window| {
                            (
                                context.tokens as f64 / window as f64 * 100.0,
                                context.estimated,
                            )
                        })
                });
                renderer.transcript_prompt(model.as_deref(), context_percent, cwd, text)?;
            }
            store::TranscriptEvent::Assistant {
                turn_state,
                items,
                internal,
            } => {
                if output == OutputFormat::Final {
                    if !internal && turn_state == "complete" {
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
                                    let exit_code = result
                                        .exit_code
                                        .context("completed Bash result has no exit code")?;
                                    renderer.bash_output(&bash_output_for_transcript(
                                        &result.output,
                                        exit_code,
                                    ))?;
                                    renderer.tool_finished(
                                        exit_code,
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
            store::TranscriptEvent::CompactionTriggered {
                trigger,
                context_tokens,
                context_window,
                reason,
            } => {
                if output != OutputFormat::Final {
                    renderer.compaction_trigger(
                        *trigger,
                        *context_tokens,
                        *context_window,
                        reason.as_deref(),
                    )?;
                }
            }
            store::TranscriptEvent::CompactionApplied {
                from_epoch,
                to_epoch,
                before_context_tokens,
                before_context_window,
                after_context_tokens_estimate,
                after_context_window,
                elapsed_ms,
            } => {
                if output != OutputFormat::Final {
                    renderer.transcript_compaction_result(&CompactionReport {
                        from_epoch: *from_epoch,
                        to_epoch: *to_epoch,
                        before_context_tokens: *before_context_tokens,
                        before_context_window: *before_context_window,
                        after_context_tokens_estimate: *after_context_tokens_estimate,
                        after_context_window: *after_context_window,
                        elapsed: Duration::from_millis(*elapsed_ms),
                    })?;
                }
            }
        }
    }
    Ok(())
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
            let listing = store.list_sessions(limit)?;
            for error in &listing.skipped {
                warn_unsupported_session(error, "skipping it");
            }
            for (s, updated) in listing.sessions {
                let title = session_listing_title(s.title.as_deref());
                let model = s.last_model.unwrap_or_else(|| "-".into());
                println!("{}  {}  {}  {}", s.id, title, model, updated);
            }
            return Ok(());
        }
        Some(Command::Transcript {
            session,
            output,
            epoch,
        }) => {
            let config =
                Config::load_for_scope(project_config_dir.as_deref(), ConfigLoadMode::Permissive)?;
            let output = resolve_output(output, config.output);
            let store_path = scope.session_store_path();
            if !store_path.exists() {
                return Err(session.as_deref().map_or_else(
                    || anyhow::anyhow!("no sessions found in active scope"),
                    ExitError::session_not_found,
                ));
            }
            let store = store::Store::open(&store_path)?;
            let session = resolve_session_or_current(&store, session.as_deref())?;
            let events = store.transcript_events_for_epoch(&session.id, epoch)?;
            let context_window = |model: &str| {
                let choice = models::resolve_model_choice(&config, model).ok()?;
                models::resolve_model_info(&config, choice.active_model()).context_window
            };
            let stdout = io::stdout();
            if stdout.is_terminal()
                && events
                    .iter()
                    .any(|event| matches!(event, store::TranscriptEvent::User { .. }))
            {
                stdout.lock().write_all(b"\n")?;
            }
            let mut renderer = Renderer::with_format(output);
            replay_transcript(&mut renderer, &events, output, context_window)?;
            return Ok(());
        }
        Some(Command::Status(status_args)) => {
            let config =
                Config::load_for_scope(project_config_dir.as_deref(), ConfigLoadMode::Runtime)?;
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
            if let Some(error) = &report.ignored_current_session {
                warn_unsupported_session(error, "ignoring current-session");
            }
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
            let config =
                Config::load_for_scope(project_config_dir.as_deref(), ConfigLoadMode::Runtime)?;
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
            store.recover_interrupted_tail_for_retry(&session.id)?;

            // Nothing to resume on a session whose last turn already finished.
            if store.is_session_clean(&session.id)? && store.queued_prompt(&session.id)?.is_none() {
                if output != OutputFormat::Final {
                    println!("session is already complete; nothing to retry");
                }
                return Ok(());
            }
            let stored_trap = store.pending_trap_level(&session.id)?;

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
                project_config_dir: project_config_dir.as_deref(),
                store: &store,
                session_id: &session.id,
                model: selection.model,
                output,
                trap: retry_args.trap.unwrap_or(stored_trap),
                preamble_notice: Some("[mu] resuming incomplete turn"),
                model_fallback: selection.fallback,
                mode: RunTurnMode::Resume,
            })
            .await?;

            return Ok(());
        }
        Some(Command::Compact {
            session,
            output,
            trap,
        }) => {
            let custom_focus = load_optional_stdin_instruction()?;
            let config =
                Config::load_for_scope(project_config_dir.as_deref(), ConfigLoadMode::Runtime)?;
            let output = resolve_output(output, config.output);
            let trap = trap.unwrap_or(config.trap);
            set_resolved_output(output);
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
            let model = resolve_session_model(&store, &config, &session_state)?;
            let _lock = acquire_session_lock_or_exit(&store, &session, output)?;
            if store.pending_compaction(&session)?.is_some() {
                bail!("session compaction is incomplete; run `mu retry -s {session}`")
            }
            store.abandon_interrupted_tail(&session, store::BashNotAttemptedReason::Abandoned)?;
            if !store.has_user_turn(&session)? {
                Renderer::with_terminal_bell(output, None)
                    .notice("[mu] compaction inapplicable: session has no conversation history")?;
                return Ok(());
            }
            store.select_session(&session)?;
            run_turn(RunTurnArgs {
                config: &config,
                project_config_dir: project_config_dir.as_deref(),
                store: &store,
                session_id: &session,
                model,
                output,
                trap,
                preamble_notice: None,
                model_fallback: None,
                mode: RunTurnMode::ManualCompaction(custom_focus.as_deref()),
            })
            .await?;
            return Ok(());
        }
        None => {}
    }

    ensure_subagent_turn_allowed(bash::subagent_depth_from_env())?;
    let config = Config::load_for_scope(project_config_dir.as_deref(), ConfigLoadMode::Runtime)?;
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
        || args.turn.output.is_some()
        || args.turn.trap.is_some();
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
    if let Some(error) = &resolved.ignored_current_session {
        warn_unsupported_session(error, "starting a new session");
    }
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

    if store.pending_compaction(&session_id)?.is_some() {
        bail!("session compaction is incomplete; run `mu retry -s {session_id}`")
    }

    let attachments = load_attachments(&turn.attachments)?;

    // A new prompt redirects rather than resumes interrupted work. Started
    // calls become conservative interrupted results; calls that never crossed
    // the durable start boundary are explicitly superseded.
    store.abandon_interrupted_tail(&session_id, store::BashNotAttemptedReason::Superseded)?;

    let prompt_content = build_prompt_content(&prompt, attachments);
    let trap = turn.trap.unwrap_or(config.trap);
    let git_worktree_root = scope
        .project()
        .and_then(|project| project.worktree.as_ref())
        .map(|worktree| worktree.root.display().to_string());
    store.queue_prompt(
        &session_id,
        &cwd.display().to_string(),
        git_worktree_root.as_deref(),
        &prompt_content,
        trap,
    )?;
    // Publish the session only after its journal lock is held and its first
    // turn is durable. Standalone `new` deliberately does not select.
    store.select_session(&session_id)?;

    run_turn(RunTurnArgs {
        config,
        project_config_dir,
        store: &store,
        session_id: &session_id,
        model: resolved.model,
        output,
        trap,
        preamble_notice: None,
        model_fallback: resolved.model_fallback,
        mode: RunTurnMode::QueuedPrompt,
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
        project_config_dir,
        store,
        session_id,
        model,
        output,
        trap,
        preamble_notice,
        model_fallback,
        mode,
    } = args;
    let mut invocation_config = config.clone();
    invocation_config.trap = trap;
    let config = &invocation_config;
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
        system_prompt_source: system_prompt::SystemPromptSource::new(
            &paths::global_dir(),
            project_config_dir,
        ),
        model,
        provider,
        store,
        session_id,
        model_context_window,
        renderer: &mut renderer,
    };

    let result = match mode {
        RunTurnMode::QueuedPrompt => agent.run_queued_turn().await,
        RunTurnMode::Resume => agent.resume_turn().await,
        RunTurnMode::ManualCompaction(focus) => agent.run_manual_compaction(focus).await,
    };

    match &result {
        Ok(r) => {
            if r.soft_interrupted {
                renderer.finish_turn()?;
                renderer.soft_interrupt_complete(r.pending_bash_calls)?;
                return Ok(());
            }
            if r.trapped {
                renderer.finish_turn()?;
                return Err(TrappedExit.into());
            }
            let ctx_pct = r.context_window.map(|cw| {
                (
                    (r.context_tokens as f64 / cw as f64) * 100.0,
                    r.context_estimated,
                )
            });
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

fn session_listing_title(title: Option<&str>) -> String {
    title.unwrap_or("(untitled)").replace('\n', "  ")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

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

    fn temp_file_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mu-{name}-{nanos}.tmp"))
    }

    #[test]
    fn prompt_file_strips_its_shebang_and_appends_piped_stdin() {
        let path = temp_file_path("shebang");
        std::fs::write(
            &path,
            "#!/usr/bin/env -S mu --model openai/gpt-5:high\nhello\n",
        )
        .unwrap();
        let mut stdin = Cursor::new("Focus on auth.\nKeep the second line.\n");
        let prompt =
            load_prompt_with_stdin(PromptSource::File(path.clone()), false, &mut stdin).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(
            prompt.text,
            "hello\n---\n\nFocus on auth.\nKeep the second line.\n"
        );
        assert_eq!(prompt.model.as_deref(), Some("openai/gpt-5:high"));
    }

    #[test]
    fn flattened_commands_use_command_local_arguments() {
        let compact = Args::try_parse_from(["mu", "compact"]).unwrap();
        assert!(matches!(
            compact.command,
            Some(Command::Compact {
                session: None,
                output: None,
                trap: None,
            })
        ));
        let selected_compact =
            Args::try_parse_from(["mu", "compact", "-s", "ses_example", "-o", "full"]).unwrap();
        assert!(matches!(
            selected_compact.command,
            Some(Command::Compact {
                session: Some(ref session),
                output: Some(OutputFormat::Full),
                trap: None,
            }) if session == "ses_example"
        ));

        let transcript =
            Args::try_parse_from(["mu", "transcript", "-s", "ses_example", "-o", "full"]).unwrap();
        assert!(matches!(
            transcript.command,
            Some(Command::Transcript {
                session: Some(ref session),
                output: Some(OutputFormat::Full),
                epoch: None,
            }) if session == "ses_example"
        ));
        let transcript_default = Args::try_parse_from(["mu", "transcript"]).unwrap();
        assert!(matches!(
            transcript_default.command,
            Some(Command::Transcript { output: None, .. })
        ));

        assert!(Args::try_parse_from(["mu", "session", "list"]).is_err());
        assert!(Args::try_parse_from(["mu", "new", "--model", "gpt-5"]).is_err());
        assert!(Args::try_parse_from(["mu", "-s", "ses_example", "-c"]).is_err());
        assert_eq!(
            Args::try_parse_from(["mu", "--trap", "all"])
                .unwrap()
                .turn
                .trap,
            Some(bash::TrapLevel::All)
        );
        assert!(Args::try_parse_from(["mu", "--trap", "readonly"]).is_err());
        assert_eq!(
            match Args::try_parse_from(["mu", "retry", "--trap", "off"])
                .unwrap()
                .command
            {
                Some(Command::Retry(args)) => args.trap,
                other => panic!("unexpected command: {other:?}"),
            },
            Some(bash::TrapLevel::Off)
        );
        assert!(
            Args::try_parse_from(["mu", "status", "--session", "ses_example", "--continue"])
                .is_err()
        );

        let misplaced = Args::try_parse_from(["mu", "-m", "gpt-5", "new"]).unwrap();
        assert!(validate_cli_args(&misplaced).is_err());
    }

    #[test]
    fn session_listing_replaces_title_newlines_with_double_spaces() {
        assert_eq!(
            session_listing_title(Some("first line\nsecond line\nthird line")),
            "first line  second line  third line"
        );
        assert_eq!(session_listing_title(None), "(untitled)");
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
    fn builtin_goal_receives_its_required_custom_instruction() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("builtins/goal");
        let source = || PromptSource::Command {
            path: path.clone(),
            scope: skills::InstructionScope::Builtin,
        };

        let goal = "Finish the migration.\nKeep all tests green.";
        let mut piped_stdin = Cursor::new(goal);
        let with_goal = load_prompt_with_stdin(source(), false, &mut piped_stdin).unwrap();
        assert!(with_goal.text.ends_with(&format!("\n---\n\n{goal}")));
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
        let (file, path) =
            crate::random::create_temp_file(&std::env::temp_dir(), "mu-oversized-", ".wav")
                .unwrap();
        file.set_len(MAX_ATTACHMENT_BYTES + 1).unwrap();
        drop(file);
        let result = load_attachments(std::slice::from_ref(&path));
        std::fs::remove_file(path).unwrap();
        assert!(result.is_err());
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
            store::TranscriptEvent::User {
                text: "Question".into(),
                cwd: "/work".into(),
                model: Some("test/model".into()),
                context: Some(store::TranscriptContext {
                    tokens: 42,
                    estimated: true,
                }),
                internal: false,
            },
            store::TranscriptEvent::Assistant {
                turn_state: "continue".into(),
                items: vec![store::TranscriptAssistantItem::Text("Intermediate".into())],
                internal: false,
            },
            store::TranscriptEvent::Assistant {
                turn_state: "complete".into(),
                items: vec![
                    store::TranscriptAssistantItem::Text("Final ".into()),
                    store::TranscriptAssistantItem::Reasoning(None),
                    store::TranscriptAssistantItem::Text("answer".into()),
                ],
                internal: false,
            },
        ];
        let buffer = TranscriptBuffer::default();
        let mut renderer =
            Renderer::with_transcript_output(OutputFormat::Final, Box::new(buffer.clone()), 79);

        replay_transcript(&mut renderer, &events, OutputFormat::Final, |_| Some(100)).unwrap();

        assert_eq!(
            buffer.text().unwrap(),
            "test/model ~42% /work\nmu> Question\n\nFinal answer\n"
        );
    }

    #[test]
    fn detailed_transcripts_replay_bash_output_without_model_exit_marker() {
        let events = vec![
            store::TranscriptEvent::User {
                text: "Question".into(),
                cwd: "/work".into(),
                model: Some("test/model".into()),
                context: None,
                internal: false,
            },
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
                            output: "line1\nline2\nline3\nline4\nline5\nline6\n\n[exit code: 0]"
                                .into(),
                            exit_code: Some(0),
                            duration_ms: Some(5),
                        }),
                    },
                ],
                internal: false,
            },
        ];
        for output in [OutputFormat::Detail, OutputFormat::Full] {
            let buffer = TranscriptBuffer::default();
            let mut renderer =
                Renderer::with_transcript_output(output, Box::new(buffer.clone()), 79);

            replay_transcript(&mut renderer, &events, output, |_| None).unwrap();

            let transcript = buffer.text().unwrap();
            assert_eq!(transcript.matches("✓ exit 0").count(), 1, "{transcript:?}");
            assert!(!transcript.contains("[exit code: 0]"), "{transcript:?}");
            assert!(transcript.contains("Running "));
            if output == OutputFormat::Full {
                assert!(transcript.contains("line1\nline2\nline3\nline4\nline5\nline6\n"));
                assert!(!transcript.contains("omitted"));
                assert!(transcript.contains("Private trace"));
            } else {
                assert!(transcript.contains("line1\nline2\nline3\n"));
                assert!(transcript.contains("line5\nline6\n"));
                assert!(transcript.contains("omitted"));
                assert!(!transcript.contains("Private trace"));
            }
        }
    }

    #[test]
    fn transcript_exit_marker_removal_preserves_truncation_note() {
        let note = "[… 3 lines elided; full output was written to temporary file /tmp/result.txt]";
        let output = format!("tail\n[exit code: 7]\n{note}");
        assert_eq!(
            bash_output_for_transcript(&output, 7),
            format!("tail\n{note}")
        );

        let marker_only = format!("\n[exit code: 7]\n{note}");
        assert_eq!(bash_output_for_transcript(&marker_only, 7), note);

        let mismatched = "tail\n[exit code: 9]";
        assert_eq!(bash_output_for_transcript(mismatched, 7), mismatched);
    }
}
