use std::fmt;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;

#[cfg(not(unix))]
compile_error!("mu is supported only on Unix-like systems");

mod agent;
mod anthropic;
mod applets;
mod attachment;
mod bash;
mod chat_completions;
mod cli;
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
use cli::{Args, Command, ProjectSub, SessionSub};
use config::Config;
use models::{RequestOptions, ResolvedModelChoice};
use provider::build_provider;
use provider::{ContentPart, UserContent};
use renderer::Renderer;
use runtime::{
    InvocationOverrides, StatusIncludes, StatusReport, build_status_report, resolve_invocation,
    resolve_retry_model_selection, resolve_session_model,
};

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
    output: cli::OutputFormat,
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
        if error_output_format() == cli::OutputFormat::Final {
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

fn error_output_format() -> cli::OutputFormat {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--output" || arg == "-o" {
            return match args.next().as_deref() {
                Some("final") => cli::OutputFormat::Final,
                Some("concise") => cli::OutputFormat::Concise,
                Some("full") => cli::OutputFormat::Full,
                _ => cli::OutputFormat::Detail,
            };
        }
        if let Some(value) = arg.strip_prefix("--output=") {
            return match value {
                "final" => cli::OutputFormat::Final,
                "concise" => cli::OutputFormat::Concise,
                "full" => cli::OutputFormat::Full,
                _ => cli::OutputFormat::Detail,
            };
        }
        if let Some(value) = arg.strip_prefix("-o").filter(|value| !value.is_empty()) {
            return match value {
                "final" => cli::OutputFormat::Final,
                "concise" => cli::OutputFormat::Concise,
                "full" => cli::OutputFormat::Full,
                _ => cli::OutputFormat::Detail,
            };
        }
    }
    match RESOLVED_OUTPUT.load(Ordering::Relaxed) {
        OUTPUT_FINAL => cli::OutputFormat::Final,
        OUTPUT_CONCISE => cli::OutputFormat::Concise,
        OUTPUT_FULL => cli::OutputFormat::Full,
        _ => cli::OutputFormat::Detail,
    }
}

fn set_resolved_output(format: cli::OutputFormat) {
    let value = match format {
        cli::OutputFormat::Final => OUTPUT_FINAL,
        cli::OutputFormat::Concise => OUTPUT_CONCISE,
        cli::OutputFormat::Detail => OUTPUT_DETAIL,
        cli::OutputFormat::Full => OUTPUT_FULL,
    };
    RESOLVED_OUTPUT.store(value, Ordering::Relaxed);
}

fn resolve_output(
    explicit: Option<cli::OutputFormat>,
    config_default: cli::OutputFormat,
) -> cli::OutputFormat {
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

fn exit_session_busy(output: cli::OutputFormat) -> ! {
    if output == cli::OutputFormat::Final {
        let _ = write_final_error("session busy");
    } else {
        eprintln!("session busy");
    }
    process::exit(2);
}

fn acquire_session_lock_or_exit<'a>(
    store: &'a store::Store,
    session_id: &str,
    output: cli::OutputFormat,
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

#[derive(Debug)]
struct ProjectInfo {
    path: String,
    is_project: bool,
    marker: Option<&'static str>,
    project_root: Option<String>,
    needs_confirmation: bool,
}

#[derive(Debug)]
struct ProjectInitInfo {
    path: String,
    project_root: String,
    created_files: Vec<&'static str>,
    already_initialized: bool,
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

fn resolve_target_dir(base: &Path, path: Option<&Path>) -> Result<PathBuf> {
    resolve_existing_dir(base, path.unwrap_or(base))
}

fn inspect_project_path(base: &Path, path: &Path) -> Result<ProjectInfo> {
    let path = resolve_existing_dir(base, path)?;
    let marker = project_marker_at(&path);
    let discovered = paths::discover_project(&path);
    Ok(ProjectInfo {
        path: path.display().to_string(),
        is_project: marker.is_some(),
        marker,
        project_root: discovered
            .as_ref()
            .map(|project| project.root.display().to_string()),
        needs_confirmation: marker.is_none(),
    })
}

fn project_marker_at(path: &Path) -> Option<&'static str> {
    if path.join(".mu").is_dir() {
        Some("mu")
    } else if path.join(".git").exists() {
        Some("git")
    } else {
        None
    }
}

fn print_project_info(info: &ProjectInfo) {
    println!("path: {}", info.path);
    println!("is_project: {}", info.is_project);
    println!(
        "marker: {}",
        info.marker.unwrap_or(if info.needs_confirmation {
            "(none)"
        } else {
            "unknown"
        })
    );
    println!(
        "project_root: {}",
        info.project_root.as_deref().unwrap_or("(none)")
    );
}

fn print_project_init_info(info: &ProjectInitInfo) {
    println!("path: {}", info.path);
    println!("project_root: {}", info.project_root);
    println!("already_initialized: {}", info.already_initialized);
    if info.created_files.is_empty() {
        println!("created_files: (none)");
    } else {
        println!("created_files: {}", info.created_files.join(", "));
    }
}

fn resolve_transcript_session(
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

async fn run() -> Result<()> {
    let args = Args::parse();
    let cwd = std::env::current_dir()?;
    let scope = paths::discover_scope(&cwd);
    let project_config_dir = scope.project().map(|p| p.root.join(".mu"));
    let default_turn = args.turn;
    let prompt_file = args.prompt_file;

    match args.command {
        Some(Command::Project { sub }) => {
            match sub {
                ProjectSub::Inspect { path } => {
                    let info = inspect_project_path(&cwd, &path)?;
                    print_project_info(&info);
                }
                ProjectSub::Init { path, force } => {
                    let root = resolve_target_dir(&cwd, path.as_deref())?;
                    let result = paths::init_project_layout_at(&root, force)?;
                    let info = ProjectInitInfo {
                        path: result.root.display().to_string(),
                        project_root: result.root.display().to_string(),
                        created_files: result.created_files,
                        already_initialized: result.already_initialized,
                    };
                    print_project_init_info(&info);
                }
            }
            return Ok(());
        }
        Some(Command::Session { sub }) => {
            let store_path = scope.session_store_path();
            match sub {
                SessionSub::New => {
                    if default_turn.selection.model.is_some() {
                        bail!("--model does not apply to `session new`; pass it to the first turn");
                    }
                    paths::ensure_project_layout(&scope)?;
                    let store = store::Store::open(&store_path)?;
                    let session =
                        store.create_session_seeded(&system_prompt::build_system_prompt(
                            &paths::global_dir(),
                            project_config_dir.as_deref(),
                        )?)?;
                    println!("{}", session.id);
                }
                SessionSub::List { limit } => {
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
                }
                SessionSub::Transcript { session } => {
                    if !store_path.exists() {
                        return Err(session.as_deref().map_or_else(
                            || anyhow::anyhow!("no sessions found in active scope"),
                            ExitError::session_not_found,
                        ));
                    }
                    let store = store::Store::open(&store_path)?;
                    let session = resolve_transcript_session(&store, session.as_deref())?;
                    for r in store.message_records_from_seq(&session.id, 0)? {
                        println!("[{}:{}] {}", r.seq, r.kind, r.content);

                        // Emit toolcall requests immediately under their assistant message
                        if r.kind == "assistant" {
                            for tc in &r.bash_calls {
                                println!(
                                    "[{}:toolcall] {} {}",
                                    r.seq, tc.function.name, tc.function.arguments
                                );
                            }
                        }

                        // Surface the tool schema together with the system message
                        if r.kind == "system"
                            && let Ok(schema) =
                                serde_json::to_string_pretty(&crate::bash::tool_definitions())
                        {
                            println!("[{}:system:toolschema]\n{}", r.seq, schema);
                        }
                    }
                }
            }
            return Ok(());
        }
        Some(Command::Status(status_args)) => {
            let config = Config::load_for_scope(project_config_dir.as_deref())?;
            let store = open_status_store(scope.session_store_path().as_path())?;
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
        Some(Command::Context(context_args)) => {
            // Introspection only: no provider, and no config load. Both builders
            // scan the instruction index and read AGENTS.md directly, which
            // tolerate a missing ~/.mu, so this works in any directory.
            let context = if context_args.export {
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
                if output != cli::OutputFormat::Final {
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
                return Err(ExitError::session_not_found(&session));
            }
            let store = store::Store::open(&store_path)?;
            let session_state = store
                .get_session(&session)?
                .ok_or_else(|| ExitError::session_not_found(&session))?;
            let mut model = resolve_session_model(&store, &config, &session_state)?;
            let request =
                RequestOptions::for_session(model.active_model().clone(), &session, "compaction");
            let mut provider = build_provider(&config, &request.model.provider_id)?;
            let _lock = acquire_session_lock_or_exit(&store, &session, cli::OutputFormat::Detail)?;
            store.normalize_interrupted_tail(&session)?;
            let mut renderer =
                Renderer::with_terminal_bell(cli::OutputFormat::Detail, None, config.line_wrapping);
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

async fn run_turn_from_source(
    cwd: &Path,
    scope: &paths::Scope,
    project_config_dir: Option<&Path>,
    config: &Config,
    turn: cli::TurnArgs,
    output: cli::OutputFormat,
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
            model: model_override(turn.selection.model.clone(), loaded_prompt.model),
        },
    )?;
    let session = if let Some(session) = resolved.attached_session.clone() {
        session
    } else {
        create_seeded_session(&store, project_config_dir)?
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
    // turn is durable. Standalone `session new` deliberately does not select.
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

fn model_override(explicit: Option<String>, shebang: Option<String>) -> Option<String> {
    explicit.or(shebang)
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
    if let Some(command) = skills::find_command(&index, &name) {
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
    let request = RequestOptions::for_session(model.active_model().clone(), session_id, "agent");
    let model_context_window = models::resolve_model_info(config, &request.model).context_window;
    let provider = build_provider(config, &request.model.provider_id)?;

    let turn_done_bell_min_duration = config
        .terminal_bell
        .enabled
        .then_some(Duration::from_millis(config.terminal_bell.min_duration_ms));
    let mut renderer =
        Renderer::with_terminal_bell(output, turn_done_bell_min_duration, config.line_wrapping);
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
        request,
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
            if output == cli::OutputFormat::Final {
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
            if output != cli::OutputFormat::Final {
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

fn create_seeded_session(
    store: &store::Store,
    project_config_dir: Option<&std::path::Path>,
) -> Result<store::Session> {
    store.create_session_seeded(&system_prompt::build_system_prompt(
        &paths::global_dir(),
        project_config_dir,
    )?)
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
    retry: &cli::RetryArgs,
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

fn open_status_store(path: &std::path::Path) -> Result<store::Store> {
    if path.exists() {
        store::Store::open(path)
    } else {
        store::Store::open_memory()
    }
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
    fn explicit_model_overrides_shebang_model() {
        assert_eq!(
            model_override(Some("explicit/model".into()), Some("command/model".into())).as_deref(),
            Some("explicit/model")
        );
        assert_eq!(
            model_override(None, Some("command/model".into())).as_deref(),
            Some("command/model")
        );
    }

    #[test]
    fn explicit_output_overrides_config_default() {
        assert_eq!(
            resolve_output(None, cli::OutputFormat::Concise),
            cli::OutputFormat::Concise
        );
        assert_eq!(
            resolve_output(Some(cli::OutputFormat::Full), cli::OutputFormat::Concise),
            cli::OutputFormat::Full
        );
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
    fn prompt_source_provenance_reports_resolved_kind_path_and_model() {
        let path = temp_file_path("provenance");
        std::fs::write(&path, "prompt").unwrap();
        let cwd = std::env::current_dir().unwrap();

        assert_eq!(
            prompt_source_provenance(
                &PromptSource::Command {
                    path: path.clone(),
                    scope: skills::InstructionScope::Project,
                },
                &cwd,
                Some("openai/gpt-5:high"),
            ),
            format!(
                "[project command] {} (model default: openai/gpt-5:high)",
                path.canonicalize().unwrap().display()
            )
        );
        assert_eq!(
            prompt_source_provenance(&PromptSource::File(path.clone()), &cwd, None),
            format!("[prompt file] {}", path.canonicalize().unwrap().display())
        );
        assert_eq!(
            prompt_source_provenance(&PromptSource::Stdin, &cwd, None),
            "[stdin]"
        );

        std::fs::remove_file(path).unwrap();
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
    fn transcript_defaults_to_current_session() {
        let store = store::Store::open_memory().unwrap();
        let first = store.create_session("/tmp").unwrap();
        let second = store.create_session("/tmp").unwrap();

        assert!(resolve_transcript_session(&store, None).is_err());
        store.select_session(&second.id).unwrap();
        assert_eq!(
            resolve_transcript_session(&store, None).unwrap().id,
            second.id
        );
        assert_eq!(
            resolve_transcript_session(&store, Some(&first.id))
                .unwrap()
                .id,
            first.id
        );
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
