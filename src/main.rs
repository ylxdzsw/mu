use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde::Serialize;

#[cfg(not(unix))]
compile_error!("mu is supported only on Unix-like systems");

mod agent;
mod bash;
mod cli;
mod compaction;
mod config;
mod env;
mod guardrail;
mod models;
mod openai;
mod paths;
mod provider;
mod redaction;
mod renderer;
mod runtime;
mod skills;
mod store;
mod system_prompt;
mod tools;
mod truncate;

use cli::{Args, Command, ProjectSub, SessionOriginArg, SessionSub};
use config::Config;
use models::RequestOptions;
use provider::{ContentPart, ImageUrl, UserContent};
use provider::{Provider, build_provider};
use renderer::Renderer;
use runtime::{InvocationOverrides, StatusReport, build_status_report, resolve_invocation};

enum PromptSource {
    Stdin,
    File(PathBuf),
    Command(PathBuf),
}

struct RunTurnArgs<'a> {
    config: &'a Config,
    provider: Arc<dyn Provider>,
    store: &'a store::Store,
    session_id: &'a str,
    request: &'a RequestOptions,
    model_context_window: Option<u64>,
    prompt: &'a str,
    output: cli::OutputFormat,
    state_dir: &'a std::path::Path,
    project_config_dir: Option<&'a std::path::Path>,
    retry_notice: Option<RetryNotice<'a>>,
}

struct RetryNotice<'a> {
    retry_count: u64,
    checkpoint_message_id: i64,
    reason: &'a str,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        let mut r = Renderer::with_format(error_output_format());
        let _ = r.error(&e.to_string());
        process::exit(1);
    }
}

fn error_output_format() -> cli::OutputFormat {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--json" {
            return cli::OutputFormat::Json;
        }
        if arg == "--output" {
            return match args.next().as_deref() {
                Some("json") => cli::OutputFormat::Json,
                Some("plain") => cli::OutputFormat::Plain,
                _ => cli::OutputFormat::Terminal,
            };
        }
        if let Some(value) = arg.strip_prefix("--output=") {
            return match value {
                "json" => cli::OutputFormat::Json,
                "plain" => cli::OutputFormat::Plain,
                _ => cli::OutputFormat::Terminal,
            };
        }
    }
    cli::OutputFormat::Terminal
}

#[derive(Debug, Serialize)]
struct ProjectInfo {
    path: String,
    is_project: bool,
    marker: Option<&'static str>,
    project_root: Option<String>,
    discovered_marker: Option<&'static str>,
    needs_confirmation: bool,
}

#[derive(Debug, Serialize)]
struct ProjectInitInfo {
    path: String,
    project_root: String,
    created_files: Vec<&'static str>,
    already_initialized: bool,
}

fn session_origin(origin: SessionOriginArg) -> store::SessionOrigin {
    match origin {
        SessionOriginArg::Cli => store::SessionOrigin::Cli,
        SessionOriginArg::Web => store::SessionOrigin::Web,
    }
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
        discovered_marker: discovered
            .as_ref()
            .map(|project| project_marker_name(project.marker)),
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

fn project_marker_name(marker: paths::ProjectMarker) -> &'static str {
    match marker {
        paths::ProjectMarker::Mu => "mu",
        paths::ProjectMarker::Git => "git",
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

async fn run() -> Result<()> {
    let args = Args::parse();
    let cwd = std::env::current_dir()?;
    let scope = paths::discover_scope(&cwd);
    let project_config_dir = scope.project().map(|p| p.root.join(".mu"));
    let origin = session_origin(args.origin);
    let prompt_source = resolve_prompt_source(args.prompt_file, &scope)?;
    let default_turn = args.turn;

    match args.command {
        Some(Command::Project { sub }) => {
            match sub {
                ProjectSub::Inspect { path, json } => {
                    let info = inspect_project_path(&cwd, &path)?;
                    if json {
                        println!("{}", serde_json::to_string(&info)?);
                    } else {
                        print_project_info(&info);
                    }
                }
                ProjectSub::Init { path, force, json } => {
                    let root = resolve_target_dir(&cwd, path.as_deref())?;
                    let result = paths::init_project_layout_at(&root, force)?;
                    let info = ProjectInitInfo {
                        path: result.root.display().to_string(),
                        project_root: result.root.display().to_string(),
                        created_files: result.created_files,
                        already_initialized: result.already_initialized,
                    };
                    if json {
                        println!("{}", serde_json::to_string(&info)?);
                    } else {
                        print_project_init_info(&info);
                    }
                }
            }
            return Ok(());
        }
        Some(Command::Session { sub }) => {
            let db_path = scope.session_db_path();
            match sub {
                SessionSub::New { json } => {
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
                    let session = store.create_session_with_origin(
                        &cwd.display().to_string(),
                        &model,
                        origin,
                    )?;
                    store.append_message(
                        &session.id,
                        &provider::Message::User {
                            content: system_prompt::initial_environment_context(
                                &cwd,
                                scope.project(),
                                &session.id,
                            )
                            .into(),
                        },
                    )?;
                    if json {
                        let summary = store
                            .session_summary(&session.id)?
                            .ok_or_else(|| anyhow::anyhow!("session not found after create"))?;
                        println!("{}", serde_json::to_string(&summary)?);
                    } else {
                        println!("{}", session.id);
                    }
                }
                SessionSub::List {
                    json,
                    limit,
                    all_origins,
                } => {
                    if !db_path.exists() {
                        if json {
                            println!("[]");
                        }
                        return Ok(());
                    }
                    let store = store::Store::open(&db_path)?;
                    if json {
                        let sessions = if all_origins {
                            store.list_all_session_summaries(limit)?
                        } else {
                            store.list_session_summaries_by_origin(origin, limit)?
                        };
                        println!("{}", serde_json::to_string(&sessions)?);
                        return Ok(());
                    }
                    if all_origins {
                        for s in store.list_all_session_summaries(limit)? {
                            let title = s.title.unwrap_or_else(|| "(untitled)".into());
                            let origin = if s.origin == store::SessionOrigin::Cli {
                                String::new()
                            } else {
                                format!(" [{}]", s.origin)
                            };
                            println!(
                                "{}  {}  {}{}  {}",
                                s.id, title, s.model, origin, s.updated_at
                            );
                        }
                        return Ok(());
                    }
                    let sessions = if origin == store::SessionOrigin::Cli {
                        store.list_sessions(limit)?
                    } else {
                        store.list_sessions_by_origin(origin, limit)?
                    };
                    for (s, updated) in sessions {
                        debug_assert!(!s.archived);
                        let title = s.title.unwrap_or_else(|| "(untitled)".into());
                        let origin = if s.origin == store::SessionOrigin::Cli {
                            String::new()
                        } else {
                            format!(" [{}]", s.origin)
                        };
                        println!("{}  {}  {}{}  {}", s.id, title, s.model, origin, updated);
                    }
                }
                SessionSub::Transcript { session, json } => {
                    if !db_path.exists() {
                        bail!("session not found in active scope: {session}");
                    }
                    let store = store::Store::open(&db_path)?;
                    if store.get_session(&session)?.is_none() {
                        bail!("session not found in active scope: {session}");
                    }
                    let transcript = store.transcript(&session)?;
                    if json {
                        println!("{}", serde_json::to_string(&transcript)?);
                    } else {
                        for message in transcript {
                            println!("[{}:{}] {}", message.seq, message.role, message.content);
                        }
                    }
                }
                SessionSub::Archive { session } => {
                    if !db_path.exists() {
                        bail!("session not found in active scope: {session}");
                    }
                    let store = store::Store::open(&db_path)?;
                    if store.get_session(&session)?.is_none() {
                        bail!("session not found in active scope: {session}");
                    }
                    store.set_session_archived(&session, true)?;
                }
                SessionSub::Unarchive { session } => {
                    if !db_path.exists() {
                        bail!("session not found in active scope: {session}");
                    }
                    let store = store::Store::open(&db_path)?;
                    if store.get_session(&session)?.is_none() {
                        bail!("session not found in active scope: {session}");
                    }
                    store.set_session_archived(&session, false)?;
                }
            }
            return Ok(());
        }
        Some(Command::Status(status_args)) => {
            let config = Config::load_for_scope(project_config_dir.as_deref())?;
            let store = open_status_store(scope.session_db_path().as_path())?;
            let commands = if status_args.include_commands {
                Some(
                    skills::scan_instruction_index(
                        &paths::global_dir(),
                        project_config_dir.as_deref(),
                    )?
                    .commands,
                )
            } else {
                None
            };
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
            )?;
            if status_args.json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                print_status_report(&report);
            }
            return Ok(());
        }
        Some(Command::Retry(retry_args)) => {
            let config = Config::load_for_scope(project_config_dir.as_deref())?;

            paths::ensure_project_layout(&scope)?;
            let state_dir = scope.state_dir();
            paths::ensure_dir(&state_dir)?;
            compaction::prune_spills(&state_dir);

            let db_path = scope.session_db_path();
            let store = store::Store::open(&db_path)?;
            let session = resolve_retry_session(&store, &retry_args)?
                .ok_or_else(|| anyhow::anyhow!("no sessions found in active scope"))?;
            let _lock = match store.acquire_session_lock(&session.id) {
                Ok(lock) => lock,
                Err(_) => {
                    eprintln!("session busy");
                    process::exit(2);
                }
            };

            store.reconcile_pending_turn_locked(&_lock, &session.id)?;
            let pending = store
                .pending_turn(&session.id)?
                .ok_or_else(|| anyhow::anyhow!("latest session has no incomplete turn to retry"))?;
            if pending.state != store::PendingState::Incomplete {
                bail!("session does not have an incomplete turn to retry");
            }
            let prompt_content = store
                .prompt_user_content(&session.id, pending.prompt_message_id)?
                .ok_or_else(|| anyhow::anyhow!("pending prompt is missing from session history"))?;
            let prompt = prompt_content.text();

            let request = RequestOptions {
                model: models::resolve_model_ref(&config, &session.model)?,
            };
            let model_info = models::resolve_model_info(&config, &request.model);
            let provider = build_provider(&config, &request.model.provider_id)?;

            store.resume_pending_turn(&session.id)?;
            let retry_count = store.increment_pending_retry_count(&session.id)?;

            run_turn(RunTurnArgs {
                config: &config,
                provider,
                store: &store,
                session_id: &session.id,
                request: &request,
                model_context_window: model_info.context_window,
                prompt: &prompt,
                output: retry_args.output,
                state_dir: &state_dir,
                project_config_dir: project_config_dir.as_deref(),
                retry_notice: Some(RetryNotice {
                    retry_count,
                    checkpoint_message_id: pending.checkpoint_message_id,
                    reason: pending.error_message.as_deref().unwrap_or("manual retry"),
                }),
            })
            .await?;

            return Ok(());
        }
        Some(Command::Compact { session }) => {
            let config = Config::load_for_scope(project_config_dir.as_deref())?;
            let db_path = scope.session_db_path();
            if !db_path.exists() {
                bail!("session not found in active scope: {session}");
            }
            let store = store::Store::open(&db_path)?;
            let session_state = store
                .get_session(&session)?
                .ok_or_else(|| anyhow::anyhow!("session not found in active scope: {session}"))?;
            let request = RequestOptions {
                model: models::resolve_model_ref(&config, &session_state.model)?,
            };
            let provider = build_provider(&config, &request.model.provider_id)?;
            let _lock = match store.acquire_session_lock(&session) {
                Ok(lock) => lock,
                Err(_) => {
                    eprintln!("session busy");
                    process::exit(2);
                }
            };
            let system_prompt = system_prompt::build_system_prompt(
                &paths::global_dir(),
                project_config_dir.as_deref(),
                Some(&store),
            )?;
            compaction::run_compaction(
                &store,
                &config,
                &session,
                &request,
                provider.as_ref(),
                &system_prompt,
            )
            .await?;
            eprintln!("compacted session {session}");
            return Ok(());
        }
        None => {}
    }

    run_turn_from_source(
        &cwd,
        &scope,
        project_config_dir.as_deref(),
        origin,
        default_turn,
        prompt_source,
    )
    .await
}

async fn run_turn_from_source(
    cwd: &Path,
    scope: &paths::Scope,
    project_config_dir: Option<&Path>,
    origin: store::SessionOrigin,
    turn: cli::TurnArgs,
    prompt_source: PromptSource,
) -> Result<()> {
    let prompt = load_prompt(prompt_source)?;
    let attachments = load_image_attachments(&turn.images)?;

    let config = Config::load_for_scope(project_config_dir)?;

    paths::ensure_project_layout(scope)?;
    let state_dir = scope.state_dir();
    paths::ensure_dir(&state_dir)?;
    compaction::prune_spills(&state_dir);

    let db_path = scope.session_db_path();
    let store = store::Store::open(&db_path)?;
    let resolved = resolve_invocation(
        &store,
        &config,
        &InvocationOverrides {
            session: turn.selection.session.clone(),
            continue_latest: turn.selection.continue_latest,
            model: turn.selection.model.clone(),
        },
    )?;
    let model_info = models::resolve_model_info(&config, &resolved.request.model);
    let provider = build_provider(&config, &resolved.request.model.provider_id)?;

    let (session, created) = if let Some(session) = resolved.attached_session.clone() {
        (session, false)
    } else {
        create_seeded_session(&store, cwd, scope.project(), &resolved.session_seed, origin)?
    };
    let session_id = session.id.clone();

    let _lock = match store.acquire_session_lock(&session_id) {
        Ok(lock) => lock,
        Err(_) => {
            eprintln!("session busy");
            process::exit(2);
        }
    };

    store.reconcile_pending_turn_locked(&_lock, &session_id)?;

    if let Some(pending) = store.pending_turn(&session_id)? {
        let reason = pending
            .error_message
            .unwrap_or_else(|| "latest turn is still incomplete".to_string());
        bail!("session has an incomplete turn: {reason}. Run `mu retry` to continue.");
    }

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
    store.begin_pending_turn(&session_id, &prompt_content)?;

    run_turn(RunTurnArgs {
        config: &config,
        provider,
        store: &store,
        session_id: &session_id,
        request: &resolved.request,
        model_context_window: model_info.context_window,
        prompt: &prompt,
        output: turn.output,
        state_dir: &state_dir,
        project_config_dir,
        retry_notice: None,
    })
    .await?;

    Ok(())
}

fn load_prompt(source: PromptSource) -> Result<String> {
    let raw = match source {
        PromptSource::Stdin => {
            let mut stdin = String::new();
            io::stdin().read_to_string(&mut stdin)?;
            normalize_prompt(&stdin, false)?
        }
        PromptSource::File(path) => {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading prompt file {}", path.display()))?;
            normalize_prompt(&raw, true)?
        }
        PromptSource::Command(path) => skills::command_prompt(&path)?,
    };
    Ok(raw)
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
        model_context_window,
        prompt,
        output,
        state_dir,
        project_config_dir,
        retry_notice,
    } = args;
    let system_prompt =
        system_prompt::build_system_prompt(&paths::global_dir(), project_config_dir, Some(store))?;
    let title: String = prompt.chars().take(60).collect();

    let turn_done_bell_min_duration = config
        .terminal_bell
        .enabled
        .then_some(Duration::from_millis(config.terminal_bell.min_duration_ms));
    let mut renderer = Renderer::with_terminal_bell(output, turn_done_bell_min_duration);
    let turn_started = Instant::now();
    if let Some(retry_notice) = retry_notice {
        renderer.turn_retry(
            "manual",
            retry_notice.retry_count,
            None,
            retry_notice.checkpoint_message_id,
            retry_notice.reason,
        )?;
    }
    let mut agent = agent::AgentLoop {
        config,
        provider,
        store,
        session_id,
        request: request.clone(),
        model_context_window,
        renderer: &mut renderer,
        state_dir,
        system_prompt,
    };

    let result = agent.run_turn().await;

    match &result {
        Ok(r) => {
            let ctx_pct =
                model_context_window.map(|cw| (r.usage.total_tokens as f64 / cw as f64) * 100.0);
            store.update_session(session_id, &r.usage, Some(&title), &request.model.canonical)?;
            store.clear_pending_turn(session_id)?;
            renderer.finish_turn()?;
            renderer.turn_summary(
                r.usage.visible_input_tokens(),
                r.usage.visible_output_tokens(),
                ctx_pct,
            )?;
            renderer.turn_done_bell(turn_started.elapsed())?;
        }
        Err(error) => {
            store.mark_pending_incomplete(session_id, &error.to_string())?;
            if let Some(pending) = store.pending_turn(session_id)? {
                renderer.turn_incomplete(
                    pending.retry_count,
                    pending.checkpoint_message_id,
                    pending
                        .error_message
                        .as_deref()
                        .unwrap_or("turn interrupted"),
                )?;
            }
        }
    }

    result.map(|_| ())
}

fn create_seeded_session(
    store: &store::Store,
    cwd: &std::path::Path,
    project: Option<&paths::Project>,
    seed: &RequestOptions,
    origin: store::SessionOrigin,
) -> Result<(store::Session, bool)> {
    let session = store.create_session_with_origin(
        &cwd.display().to_string(),
        &seed.model.canonical,
        origin,
    )?;
    store.append_message(
        &session.id,
        &provider::Message::User {
            content: system_prompt::initial_environment_context(cwd, project, &session.id).into(),
        },
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
    if retry.session.is_some() && retry.continue_latest {
        bail!("use either -s/--session or -c/--continue-latest, not both");
    }
    if let Some(id) = retry.session.as_deref() {
        return Ok(Some(store.get_session(id)?.ok_or_else(|| {
            anyhow::anyhow!("session not found in active scope: {id}")
        })?));
    }
    store.latest_session()
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

    println!("model: {}", report.model_id);
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
    if let Some(incomplete) = &report.incomplete_turn {
        println!(
            "incomplete turn: retry_count={}{}",
            incomplete.retry_count,
            incomplete
                .error_message
                .as_deref()
                .map(|message| format!("  reason: {message}"))
                .unwrap_or_default()
        );
        println!("retry: mu retry");
    }
    println!("supported effort levels: {effort_levels}");
}

fn load_image_attachments(paths: &[PathBuf]) -> Result<Vec<ContentPart>> {
    paths
        .iter()
        .map(|path| {
            let mime = image_mime(path)?;
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading image attachment {}", path.display()))?;
            let encoded = base64_encode(&bytes);
            Ok(ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url: format!("data:{mime};base64,{encoded}"),
                },
            })
        })
        .collect()
}

fn image_mime(path: &std::path::Path) -> Result<&'static str> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Ok("image/png"),
        Some("jpg") | Some("jpeg") => Ok("image/jpeg"),
        Some("webp") => Ok("image/webp"),
        Some("gif") => Ok("image/gif"),
        _ => bail!("unsupported image attachment type: {}", path.display()),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        out.push(TABLE[(b0 >> 2) as usize] as char);
        out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
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
        std::fs::write(&path, "#!/usr/bin/env -S mu --output plain\nhello\n").unwrap();
        let prompt = load_prompt(PromptSource::File(path.clone())).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(prompt, "hello");
    }

    #[test]
    fn load_prompt_file_reports_utf8_errors_with_path() {
        let path = temp_file_path("invalid-utf8");
        std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
        let err = load_prompt(PromptSource::File(path.clone())).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert!(err.to_string().contains("reading prompt file"));
        assert!(err.to_string().contains(path.to_string_lossy().as_ref()));
    }
}
