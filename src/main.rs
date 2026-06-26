use std::io::{self, Read};
use std::path::PathBuf;
use std::process;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;

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

use cli::{Args, Command, ModelsSub, SessionSub};
use config::Config;
use models::{ModelCatalog, RequestOptions};
use provider::openai::OpenAiProvider;
use provider::Provider;
use provider::{ContentPart, ImageUrl};
use renderer::Renderer;
use runtime::{build_status_report, resolve_invocation, InvocationOverrides, StatusReport};

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

async fn run() -> Result<()> {
    let args = Args::parse();
    let cwd = std::env::current_dir()?;
    let scope = paths::discover_scope(&cwd);
    let project_config_dir = scope.project().map(|p| p.root.join(".mu"));

    match args.command {
        Some(Command::Session { sub }) => {
            let db_path = scope.session_db_path();
            match sub {
                SessionSub::New => {
                    paths::ensure_project_layout(&scope)?;
                    let store = store::Store::open(&db_path)?;
                    let latest = store.latest_session()?;
                    let inherited_model = latest.as_ref().map(|session| session.model.clone());
                    let inherited_effort = latest.as_ref().and_then(|session| session.effort);
                    let default_model = Config::try_load_for_scope(project_config_dir.as_deref())
                        .map(|c| c.default_model)
                        .unwrap_or_else(|| "gpt-4o".into());
                    let session = store.create_session(
                        &cwd.display().to_string(),
                        inherited_model.as_deref().unwrap_or(default_model.as_str()),
                        inherited_effort,
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
                    println!("{}", session.id);
                }
                SessionSub::List => {
                    if !db_path.exists() {
                        return Ok(());
                    }
                    let store = store::Store::open(&db_path)?;
                    let sessions = store.list_sessions(20)?;
                    for (s, updated) in sessions {
                        let title = s.title.unwrap_or_else(|| "(untitled)".into());
                        let effort = s
                            .effort
                            .map(|level| format!(" [{}]", level))
                            .unwrap_or_default();
                        println!("{}  {}  {}{}  {}", s.id, title, s.model, effort, updated);
                    }
                }
            }
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
                    let catalog =
                        models::refresh_model_catalog(&config.provider.base_url, &api_key).await?;
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

    // Turn invocation
    let mut stdin = String::new();
    io::stdin().read_to_string(&mut stdin)?;
    let prompt = stdin.trim_end_matches('\n').to_string();
    if prompt.is_empty() {
        bail!("empty prompt");
    }
    let attachments = load_image_attachments(&args.turn.images)?;

    let config = Config::load_for_scope(project_config_dir.as_deref())?;
    let api_key = config.api_key()?;
    let catalog = ModelCatalog::load_matching(&config.provider.base_url)?;
    let provider = Arc::new(OpenAiProvider::new(
        config.provider.base_url.clone(),
        api_key,
    )) as Arc<dyn Provider>;

    paths::ensure_project_layout(&scope)?;
    let state_dir = scope.state_dir();
    paths::ensure_dir(&state_dir)?;
    compaction::prune_spills(&state_dir);

    let db_path = scope.session_db_path();
    let store = store::Store::open(&db_path)?;
    let resolved = resolve_invocation(
        &store,
        &config,
        &InvocationOverrides {
            session: args.turn.selection.session.clone(),
            continue_latest: args.turn.selection.continue_latest,
            model: args.turn.selection.model.clone(),
            effort: args.turn.selection.effort,
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
        create_seeded_session(&store, &cwd, scope.project(), &resolved.session_seed)?
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
                content: system_prompt::cwd_changed_context(&cwd).into(),
            },
        )?;
        store.update_session_cwd(&session_id, &cwd.display().to_string())?;
    }

    run_turn(
        &config,
        provider,
        &store,
        &session_id,
        &resolved.request,
        model_info.context_window,
        &prompt,
        attachments,
        args.turn.output,
        &state_dir,
        project_config_dir.as_deref(),
    )
    .await?;

    Ok(())
}

async fn run_turn(
    config: &Config,
    provider: Arc<dyn Provider>,
    store: &store::Store,
    session_id: &str,
    request: &RequestOptions,
    model_context_window: Option<u64>,
    prompt: &str,
    attachments: Vec<ContentPart>,
    output: cli::OutputFormat,
    state_dir: &std::path::Path,
    project_config_dir: Option<&std::path::Path>,
) -> Result<()> {
    let system_prompt =
        system_prompt::build_system_prompt(&paths::global_dir(), project_config_dir, Some(store))?;
    let title: String = prompt.chars().take(60).collect();

    let mut renderer = Renderer::with_format(output);
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
) -> Result<(store::Session, bool)> {
    let session = store.create_session(&cwd.display().to_string(), &seed.model, seed.effort)?;
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
