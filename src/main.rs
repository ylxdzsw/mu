use std::fmt;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;

#[cfg(not(windows))]
compile_error!("the msys2 branch supports only Windows with MSYS2 UCRT64");

mod agent;
mod applets;
mod artifact;
mod attachment;
mod bash;
mod chat_completions;
mod cli;
mod compaction;
mod config;
mod guardrail;
mod models;
mod paths;
mod provider;
mod redaction;
mod renderer;
mod responses;
mod runtime;
mod skills;
mod store;
mod system_prompt;
mod tools;
mod windows_msys2;

#[cfg(test)]
use attachment::MAX_ATTACHMENT_BYTES;
use attachment::load_attachments;
use cli::{Args, Command, ProjectSub, SessionSub};
use config::Config;
use models::RequestOptions;
use provider::{ContentPart, UserContent};
use provider::{Provider, build_provider};
use renderer::Renderer;
use runtime::{InvocationOverrides, StatusReport, build_status_report, resolve_invocation};

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

enum PromptSource {
    Stdin,
    File(PathBuf),
    Command(PathBuf),
}

#[derive(Debug)]
struct LoadedPrompt {
    text: String,
    model: Option<String>,
}

struct RunTurnArgs<'a> {
    config: &'a Config,
    provider: Box<dyn Provider>,
    store: &'a store::Store,
    session_id: &'a str,
    request: &'a RequestOptions,
    attempt_kind: &'a str,
    model_context_window: Option<u64>,
    /// Display title source (first ~60 chars). `None` on retry, which continues
    /// an existing turn and must not overwrite the stored title.
    title: Option<&'a str>,
    output: cli::OutputFormat,
    state_dir: &'a std::path::Path,
    /// A short notice rendered before the turn (e.g. "resuming interrupted turn").
    preamble_notice: Option<&'a str>,
}

fn main() {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    if let Some(applet) = applets::from_argv0(&argv0) {
        process::exit(applets::dispatch(applet));
    }

    let result = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("initializing Mu async runtime")
        .and_then(|runtime| runtime.block_on(run()));
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
/// A terminating console event wins first (`128 + signal`, so `130` for
/// Ctrl-C), then any error carrying an explicit `ExitError` code, otherwise the
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
        if arg == "--output" {
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

fn acquire_session_lock_or_exit(
    store: &store::Store,
    session_id: &str,
    output: cli::OutputFormat,
) -> Result<store::SessionLock> {
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
        path: windows_msys2::display_path(&path),
        is_project: marker.is_some(),
        marker,
        project_root: discovered
            .as_ref()
            .map(|project| windows_msys2::display_path(&project.root)),
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

fn open_store_with_session(db_path: &Path, session: &str) -> Result<store::Store> {
    if !db_path.exists() {
        return Err(ExitError::session_not_found(session));
    }
    let store = store::Store::open(db_path)?;
    if store.get_session(session)?.is_none() {
        return Err(ExitError::session_not_found(session));
    }
    Ok(store)
}

async fn run() -> Result<()> {
    windows_msys2::validate_environment()?;
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
                        path: windows_msys2::display_path(&result.root),
                        project_root: windows_msys2::display_path(&result.root),
                        created_files: result.created_files,
                        already_initialized: result.already_initialized,
                    };
                    print_project_init_info(&info);
                }
            }
            return Ok(());
        }
        Some(Command::Session { sub }) => {
            let db_path = scope.session_db_path();
            match sub {
                SessionSub::New => {
                    paths::ensure_project_layout(&scope)?;
                    let store = store::Store::open(&db_path)?;
                    let latest = store.latest_session()?;
                    let model = match latest.as_ref().map(|session| session.model.clone()) {
                        Some(model) => model,
                        None => {
                            let config = Config::load_for_scope(project_config_dir.as_deref())?;
                            models::first_model_ref(&config)?.canonical
                        }
                    };
                    let session = store.create_session_seeded(
                        &cwd.display().to_string(),
                        &model,
                        &system_prompt::build_system_prompt(
                            &paths::global_dir(),
                            project_config_dir.as_deref(),
                        )?,
                        &system_prompt::initial_environment_context(&cwd, scope.project()),
                    )?;
                    println!("{}", session.id);
                }
                SessionSub::List { limit } => {
                    if !db_path.exists() {
                        return Ok(());
                    }
                    let store = store::Store::open(&db_path)?;
                    let sessions = store.list_sessions(limit)?;
                    for (s, updated) in sessions {
                        let title = s.title.unwrap_or_else(|| "(untitled)".into());
                        println!("{}  {}  {}  {}", s.id, title, s.model, updated);
                    }
                }
                SessionSub::Transcript { session } => {
                    let store = open_store_with_session(&db_path, &session)?;
                    for r in store.message_records_from_seq(&session, 0)? {
                        println!("[{}:{}] {}", r.seq, r.role, r.content);

                        // Emit toolcall requests immediately under their assistant message
                        if r.role == "assistant"
                            && let Some(calls) =
                                crate::store::parse_tool_calls(r.tool_calls_json.as_deref())
                        {
                            for tc in calls {
                                println!(
                                    "[{}:toolcall] {} {}",
                                    r.seq, tc.function.name, tc.function.arguments
                                );
                            }
                        }

                        // Surface the tool schema together with the system message
                        if r.role == "system"
                            && let Ok(schema) =
                                serde_json::to_string_pretty(&crate::tools::tool_definitions())
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
            let store = open_status_store(scope.session_db_path().as_path())?;
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
                    continue_latest: status_args.selection.continue_latest,
                    model: status_args.selection.model,
                },
                scope.project(),
                status_args.include_models,
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
        Some(Command::Retry(retry_args)) => {
            ensure_subagent_turn_allowed(bash::subagent_depth_from_env())?;
            let config = Config::load_for_scope(project_config_dir.as_deref())?;
            let output = resolve_output(retry_args.output, config.output);
            set_resolved_output(output);

            paths::ensure_project_layout(&scope)?;
            let state_dir = scope.state_dir();
            paths::ensure_dir(&state_dir)?;
            compaction::prune_spills(&state_dir);

            let db_path = scope.session_db_path();
            let store = store::Store::open(&db_path)?;
            let session = resolve_retry_session(&store, &retry_args)?
                .ok_or_else(|| anyhow::anyhow!("no sessions found in active scope"))?;
            let _lock = acquire_session_lock_or_exit(&store, &session.id, output)?;

            // Nothing to resume on a session whose last turn already finished.
            if store.is_session_clean(&session.id)? {
                if output != cli::OutputFormat::Final {
                    println!("session is already complete; nothing to retry");
                }
                return Ok(());
            }

            // Make the interrupted tail valid (synthesize results for any
            // dangling tool calls), then continue the loop with no new prompt.
            store.normalize_interrupted_tail(&session.id)?;

            let request = RequestOptions {
                model: models::resolve_model_ref(
                    &config,
                    effective_retry_model_ref(
                        &session.model,
                        retry_args.selection.model.as_deref(),
                    ),
                )?,
            };
            let model_info = models::resolve_model_info(&config, &request.model);
            let provider = build_provider(&config, &request.model.provider_id)?;

            run_turn(RunTurnArgs {
                config: &config,
                provider,
                store: &store,
                session_id: &session.id,
                request: &request,
                attempt_kind: "retry",
                model_context_window: model_info.context_window,
                title: None,
                output,
                state_dir: &state_dir,
                preamble_notice: Some("[mu] resuming interrupted turn"),
            })
            .await?;

            return Ok(());
        }
        Some(Command::Compact { session }) => {
            let custom_focus = load_optional_stdin_instruction()?;
            let config = Config::load_for_scope(project_config_dir.as_deref())?;
            let db_path = scope.session_db_path();
            if !db_path.exists() {
                return Err(ExitError::session_not_found(&session));
            }
            let store = store::Store::open(&db_path)?;
            let session_state = store
                .get_session(&session)?
                .ok_or_else(|| ExitError::session_not_found(&session))?;
            let request = RequestOptions {
                model: models::resolve_model_ref(&config, &session_state.model)?,
            };
            let provider = build_provider(&config, &request.model.provider_id)?;
            let _lock = acquire_session_lock_or_exit(&store, &session, cli::OutputFormat::Detail)?;
            compaction::run_compaction(
                &store,
                &config,
                &session,
                &request,
                provider.as_ref(),
                custom_focus.as_deref(),
            )
            .await?;
            eprintln!("compacted session {session}");
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
    compaction::prune_spills(&state_dir);

    let db_path = scope.session_db_path();
    let store = store::Store::open(&db_path)?;
    let resolved = resolve_invocation(
        &store,
        config,
        &InvocationOverrides {
            session: turn.selection.session.clone(),
            continue_latest: turn.selection.continue_latest,
            model: model_override(turn.selection.model.clone(), loaded_prompt.model),
        },
    )?;
    let model_info = models::resolve_model_info(config, &resolved.request.model);
    let provider = build_provider(config, &resolved.request.model.provider_id)?;

    let (session, created) = if let Some(session) = resolved.attached_session.clone() {
        (session, false)
    } else {
        create_seeded_session(
            &store,
            cwd,
            scope.project(),
            project_config_dir,
            &resolved.session_seed,
        )?
    };
    let session_id = session.id.clone();

    let _lock = acquire_session_lock_or_exit(&store, &session_id, output)?;

    // If the previous turn was interrupted, normalize its tail (synthesize
    // interrupted results for any dangling tool calls) so history is valid.
    // The new prompt then lands on top of that valid history — the user can
    // redirect after a Ctrl-C without being forced to `mu retry` first.
    store.normalize_interrupted_tail(&session_id)?;

    if created {
        if let Ok(session_file) = std::env::var("MU_SESSION_FILE") {
            store::write_session_id(PathBuf::from(&session_file).as_path(), &session_id)?;
        }
    } else if session.cwd != cwd.display().to_string() {
        store.append_message(
            &session_id,
            &provider::Message::User {
                content: system_prompt::cwd_changed_context(cwd).into(),
            },
        )?;
        store.update_session_cwd(&session_id, &cwd.display().to_string())?;
    }

    let prompt_content = build_prompt_content(&prompt, attachments);
    store.append_message(
        &session_id,
        &provider::Message::User {
            content: prompt_content,
        },
    )?;

    let title: String = prompt.chars().take(60).collect();
    run_turn(RunTurnArgs {
        config,
        provider,
        store: &store,
        session_id: &session_id,
        request: &resolved.request,
        attempt_kind: "turn",
        model_context_window: model_info.context_window,
        title: Some(&title),
        output,
        state_dir: &state_dir,
        preamble_notice: None,
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
        PromptSource::Command(path) => {
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
        return Ok(PromptSource::Command(PathBuf::from(&command.path)));
    }
    Ok(PromptSource::File(path))
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
        provider,
        store,
        session_id,
        request,
        attempt_kind,
        model_context_window,
        title,
        output,
        state_dir,
        preamble_notice,
    } = args;

    let turn_done_bell_min_duration = config
        .terminal_bell
        .enabled
        .then_some(Duration::from_millis(config.terminal_bell.min_duration_ms));
    let mut renderer = Renderer::with_terminal_bell(output, turn_done_bell_min_duration);
    let turn_started = Instant::now();
    if let Some(notice) = preamble_notice {
        renderer.notice(notice)?;
    }
    let mut agent = agent::AgentLoop {
        config,
        provider,
        store,
        session_id,
        request: request.clone(),
        attempt_kind,
        model_context_window,
        renderer: &mut renderer,
        state_dir,
    };

    let result = agent.run_turn().await;

    match &result {
        Ok(r) => {
            let ctx_pct =
                model_context_window.map(|cw| (r.context_tokens as f64 / cw as f64) * 100.0);
            store.update_session(
                session_id,
                &r.usage,
                r.context_tokens,
                title,
                &request.model.canonical,
            )?;
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
            // Nothing to clean up: only completed messages are persisted, so the
            // log ends at the last landed message. The session is now "unclean";
            // the next turn or `mu retry` will normalize any dangling tool call.
            if output != cli::OutputFormat::Final {
                renderer.turn_interrupted(&error.to_string())?;
            }
        }
    }

    result.map(|_| ())
}

fn create_seeded_session(
    store: &store::Store,
    cwd: &std::path::Path,
    project: Option<&paths::Project>,
    project_config_dir: Option<&std::path::Path>,
    seed: &RequestOptions,
) -> Result<(store::Session, bool)> {
    let session = store.create_session_seeded(
        &cwd.display().to_string(),
        &seed.model.canonical,
        &system_prompt::build_system_prompt(&paths::global_dir(), project_config_dir)?,
        &system_prompt::initial_environment_context(cwd, project),
    )?;
    Ok((session, true))
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
    if retry.selection.session.is_some() && retry.selection.continue_latest {
        bail!("use either -s/--session or -c/--continue-latest, not both");
    }
    if let Some(id) = retry.selection.session.as_deref() {
        return Ok(Some(
            store
                .get_session(id)?
                .ok_or_else(|| ExitError::session_not_found(id))?,
        ));
    }
    store.latest_session()
}

fn effective_retry_model_ref<'a>(stored: &'a str, override_ref: Option<&'a str>) -> &'a str {
    override_ref.unwrap_or(stored)
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
    let context = match (report.context_percent, report.context_window) {
        (Some(percent), Some(window)) => format!("{percent:.2}% of {window}"),
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
    if report.active.busy {
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
    fn retry_model_override_wins_over_stored_model() {
        assert_eq!(
            effective_retry_model_ref(
                "opencode/deepseek-v4-flash-free",
                Some("opencode/mimo-v2.5-free"),
            ),
            "opencode/mimo-v2.5-free"
        );
        assert_eq!(
            effective_retry_model_ref("opencode/deepseek-v4-flash-free", None),
            "opencode/deepseek-v4-flash-free"
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
    fn prompt_file_ignores_empty_non_terminal_stdin() {
        let path = temp_file_path("empty-instruction");
        std::fs::write(&path, "Use the release-note format.\n").unwrap();
        let mut stdin = Cursor::new("");
        let prompt =
            load_prompt_with_stdin(PromptSource::File(path.clone()), false, &mut stdin).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(prompt.text, "Use the release-note format.");
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
        let prompt =
            load_prompt_with_stdin(PromptSource::Command(path.clone()), false, &mut stdin).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(prompt.text, "Review the checkout.\n---\n\nFocus on auth.");
        assert_eq!(prompt.model.as_deref(), Some("openai/gpt-5:high"));
    }

    #[test]
    fn load_prompt_file_reports_utf8_errors_with_path() {
        let path = temp_file_path("invalid-utf8");
        std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        let mut stdin = Cursor::new("");
        let err =
            load_prompt_with_stdin(PromptSource::File(path.clone()), true, &mut stdin).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert!(err.to_string().contains("reading prompt file"));
        assert!(err.to_string().contains(path.to_string_lossy().as_ref()));
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
    fn exit_code_defaults_to_one_for_generic_errors() {
        bash::reset_cancellation_state();
        let err = anyhow::anyhow!("something went wrong");
        assert_eq!(exit_code_for(&err), 1);
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
