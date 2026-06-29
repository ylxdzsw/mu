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
mod cli;
mod compaction;
mod config;
mod env;
mod guardrail;
mod models;
mod paths;
mod provider;
mod redaction;
mod renderer;
mod runtime;
mod skills;
mod store;
mod system_prompt;
mod tools;
mod web;

use cli::{Args, Command, ModelsSub, ProjectSub, SessionOriginArg, SessionSub};
use config::Config;
use models::{ModelCatalog, RequestOptions};
use provider::Provider;
use provider::openai::OpenAiProvider;
use provider::{ContentPart, ImageUrl};
use renderer::Renderer;
use runtime::{InvocationOverrides, StatusReport, build_status_report, resolve_invocation};

enum PromptSource {
    Stdin,
    File(PathBuf),
}

struct RunTurnArgs<'a> {
    config: &'a Config,
    provider: Arc<dyn Provider>,
    store: &'a store::Store,
    session_id: &'a str,
    request: &'a RequestOptions,
    model_context_window: Option<u64>,
    prompt: &'a str,
    attachments: Vec<ContentPart>,
    output: cli::OutputFormat,
    state_dir: &'a std::path::Path,
    project_config_dir: Option<&'a std::path::Path>,
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

async fn run() -> Result<()> {
    let args = Args::parse();
    let cwd = std::env::current_dir()?;
    let scope = paths::discover_scope(&cwd);
    let project_config_dir = scope.project().map(|p| p.root.join(".mu"));
    let origin = session_origin(args.origin);
    let prompt_source = args
        .prompt_file
        .map_or(PromptSource::Stdin, PromptSource::File);
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
                ProjectSub::Init { path, json } => {
                    let root = resolve_existing_dir(&cwd, &path)?;
                    paths::ensure_project_layout_at(&root)?;
                    let info = inspect_project_path(&cwd, &root)?;
                    if json {
                        println!("{}", serde_json::to_string(&info)?);
                    } else {
                        print_project_info(&info);
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
                    let inherited_model = latest.as_ref().map(|session| session.model.clone());
                    let inherited_effort = latest.as_ref().and_then(|session| session.effort);
                    let config = Config::try_load_for_scope(project_config_dir.as_deref());
                    let default_model = config
                        .as_ref()
                        .map(|c| c.default_model.as_str())
                        .unwrap_or("gpt-4o");
                    let default_effort = config.as_ref().and_then(|c| c.default_effort);
                    let session = store.create_session_with_origin(
                        &cwd.display().to_string(),
                        inherited_model.as_deref().unwrap_or(default_model),
                        inherited_effort.or(default_effort),
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
                            let effort = s
                                .effort
                                .map(|level| format!(" [{}]", level))
                                .unwrap_or_default();
                            println!(
                                "{}  {}  {}{}{}  {}",
                                s.id, title, s.model, effort, origin, s.updated_at
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
                        let effort = s
                            .effort
                            .map(|level| format!(" [{}]", level))
                            .unwrap_or_default();
                        println!(
                            "{}  {}  {}{}{}  {}",
                            s.id, title, s.model, effort, origin, updated
                        );
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
        Some(Command::Web(web_args)) => {
            web::serve(web_args, cwd).await?;
            return Ok(());
        }
        Some(Command::Status(status_args)) => {
            let config = Config::load_for_scope(project_config_dir.as_deref())?;
            let _api_key = config.api_key()?;
            let catalog = ModelCatalog::load_matching(&config.provider.base_url)?;
            let store = open_status_store(scope.session_db_path().as_path())?;
            let report = build_status_report(
                &store,
                &config,
                &InvocationOverrides {
                    session: status_args.selection.session,
                    continue_latest: status_args.selection.continue_latest,
                    model: status_args.selection.model,
                    effort: status_args.selection.effort,
                },
                catalog.as_ref(),
                scope.project(),
            )?;
            if status_args.json {
                println!("{}", serde_json::to_string(&report)?);
            } else {
                print_status_report(&report);
            }
            return Ok(());
        }
        Some(Command::Models { sub }) => {
            match sub {
                ModelsSub::Refresh => {
                    let config = Config::load_for_scope(project_config_dir.as_deref())?;
                    let api_key = config.api_key()?;
                    let catalog = models::refresh_model_catalog(
                        &config.provider.base_url,
                        api_key.as_deref(),
                    )
                    .await?;
                    let path = ModelCatalog::cache_path();
                    catalog.save(&path)?;
                    println!(
                        "refreshed {} models into {}",
                        catalog.models.len(),
                        path.display()
                    );
                }
                ModelsSub::List { json } => {
                    let Some(catalog) = ModelCatalog::load(&ModelCatalog::cache_path())? else {
                        if json {
                            println!(
                                "{}",
                                serde_json::json!({
                                    "version": 1,
                                    "fetched_at": null,
                                    "provider": null,
                                    "models": {}
                                })
                            );
                        }
                        return Ok(());
                    };
                    if json {
                        println!("{}", serde_json::to_string(&catalog)?);
                    } else {
                        print_model_catalog(&catalog);
                    }
                }
            }
            return Ok(());
        }
        Some(Command::Compact { session }) => {
            let config = Config::load_for_scope(project_config_dir.as_deref())?;
            let api_key = config.api_key()?;
            let provider = Arc::new(OpenAiProvider::new(
                config.provider.base_url.clone(),
                api_key,
            )) as Arc<dyn Provider>;
            let db_path = scope.session_db_path();
            if !db_path.exists() {
                bail!("session not found in active scope: {session}");
            }
            let store = store::Store::open(&db_path)?;
            let session_state = store
                .get_session(&session)?
                .ok_or_else(|| anyhow::anyhow!("session not found in active scope: {session}"))?;
            let _lock = match store::acquire_session_lock(&session) {
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
                &RequestOptions {
                    model: session_state.model,
                    effort: session_state.effort,
                },
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
    let api_key = config.api_key()?;
    let catalog = ModelCatalog::load_matching(&config.provider.base_url)?;
    let provider = Arc::new(OpenAiProvider::new(
        config.provider.base_url.clone(),
        api_key,
    )) as Arc<dyn Provider>;

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
            effort: turn.selection.effort,
        },
    )?;
    let model_info = models::resolve_model_info(&config, catalog.as_ref(), &resolved.request.model);
    models::validate_effort_support(
        &resolved.request.model,
        resolved.request.effort.as_ref(),
        &model_info,
    )?;

    let (session, created) = if let Some(session) = resolved.attached_session.clone() {
        (session, false)
    } else {
        create_seeded_session(&store, cwd, scope.project(), &resolved.session_seed, origin)?
    };
    let session_id = session.id.clone();

    let _lock = match store::acquire_session_lock(&session_id) {
        Ok(lock) => lock,
        Err(_) => {
            eprintln!("session busy");
            process::exit(2);
        }
    };

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

    run_turn(RunTurnArgs {
        config: &config,
        provider,
        store: &store,
        session_id: &session_id,
        request: &resolved.request,
        model_context_window: model_info.context_window,
        prompt: &prompt,
        attachments,
        output: turn.output,
        state_dir: &state_dir,
        project_config_dir,
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
    };
    Ok(raw)
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
        attachments,
        output,
        state_dir,
        project_config_dir,
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
        attachments,
    };

    let result = agent.run_turn(prompt).await;

    match &result {
        Ok(r) => {
            let ctx_pct =
                model_context_window.map(|cw| (r.final_total_tokens as f64 / cw as f64) * 100.0);
            store.update_session(
                session_id,
                r.final_total_tokens,
                r.cost,
                Some(&title),
                &request.model,
                request.effort,
            )?;
            renderer.finish_turn()?;
            renderer.turn_summary(
                r.prompt_tokens,
                r.completion_tokens,
                ctx_pct,
                if r.cost > 0.0 { Some(r.cost) } else { None },
            )?;
            renderer.turn_done_bell(turn_started.elapsed())?;
        }
        Err(_) => {
            // completed messages already persisted
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
        &seed.model,
        seed.effort,
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

fn open_status_store(path: &std::path::Path) -> Result<store::Store> {
    if path.exists() {
        store::Store::open(path)
    } else {
        store::Store::open_memory()
    }
}

fn print_status_report(report: &StatusReport) {
    let effort = report
        .effort
        .map(|level| level.to_string())
        .unwrap_or_else(|| "unset".into());
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
    println!("effort: {effort}");
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
        if session.cost_total > 0.0 {
            println!("cost total: ${:.4}", session.cost_total);
        }
    }
    if report.active.busy {
        println!("active: busy");
    }
    println!(
        "metadata: {}",
        match report.model_metadata_source {
            models::ModelMetadataSource::Cache => "cache",
            models::ModelMetadataSource::FallbackInference => "fallback_inference",
        }
    );
    println!("supported effort levels: {effort_levels}");
}

fn print_model_catalog(catalog: &ModelCatalog) {
    println!("provider: {}", catalog.provider.base_url);
    println!("fetched_at: {}", catalog.fetched_at);
    for model in catalog.models.values() {
        let effort_levels = model
            .reasoning_effort_levels
            .as_ref()
            .map(|levels| {
                if levels.is_empty() {
                    "(none)".to_string()
                } else {
                    levels
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                }
            })
            .unwrap_or_else(|| "(unknown)".into());
        println!(
            "{}  {}  ctx={:?}  out={:?}  reasoning={:?}  effort={}",
            model.id,
            model.display_name,
            model.context_window,
            model.max_output_tokens,
            model.reasoning,
            effort_levels
        );
    }
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
    fn load_prompt_file_preserves_body() {
        let path = temp_file_path("prompt");
        std::fs::write(&path, "hello\nworld\n").unwrap();
        let prompt = load_prompt(PromptSource::File(path.clone())).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(prompt, "hello\nworld");
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
    fn load_prompt_file_trims_crlf_shebang_line() {
        let path = temp_file_path("shebang-crlf");
        std::fs::write(&path, "#!/usr/bin/env -S mu\r\nhello\r\n").unwrap();
        let prompt = load_prompt(PromptSource::File(path.clone())).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(prompt, "hello");
    }

    #[test]
    fn load_prompt_file_rejects_shebang_only() {
        let path = temp_file_path("shebang-only");
        std::fs::write(&path, "#!/usr/bin/env -S mu --output plain\n").unwrap();
        let err = load_prompt(PromptSource::File(path.clone())).unwrap_err();
        std::fs::remove_file(path).unwrap();
        assert_eq!(err.to_string(), "empty prompt");
    }

    #[test]
    fn normalize_prompt_keeps_stdin_shebang_text() {
        let prompt =
            normalize_prompt("#!/usr/bin/env -S mu --output plain\nhello\n", false).unwrap();
        assert_eq!(prompt, "#!/usr/bin/env -S mu --output plain\nhello");
    }

    #[test]
    fn normalize_prompt_trims_file_shebang_text() {
        let prompt =
            normalize_prompt("#!/usr/bin/env -S mu --output plain\nhello\n", true).unwrap();
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
