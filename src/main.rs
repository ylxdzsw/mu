use std::io::{self, Read};
use std::path::PathBuf;
use std::process;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;

mod agent;
mod cli;
mod compaction;
mod config;
mod guardrail;
mod paths;
mod provider;
mod renderer;
mod skills;
mod store;
mod system_prompt;
mod tools;

use cli::{Args, Command, InitSub, SessionSub};
use config::Config;
use provider::openai::OpenAiProvider;
use provider::Provider;
use provider::{ContentPart, ImageUrl};
use renderer::Renderer;

const ZSH_PLUGIN: &str = include_str!("../shell-plugins/mu.zsh");

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

    match args.command {
        Some(Command::Init { sub }) => match sub {
            Some(InitSub::Zsh) => {
                print!("{ZSH_PLUGIN}");
                return Ok(());
            }
            None => {
                let config_path = Config::starter_path();
                Config::write_starter(&config_path)?;
                eprintln!("wrote starter config to {}", config_path.display());
                eprintln!("set {} and run a turn", "OPENAI_API_KEY");
                return Ok(());
            }
        },
        Some(Command::Session { sub }) => {
            let db_path = scope.session_db_path();
            match sub {
                SessionSub::New => {
                    paths::ensure_project_layout(&scope)?;
                    let store = store::Store::open(&db_path)?;
                    let model = Config::try_load_for_scope(
                        scope.project().map(|p| p.root.join(".mu")).as_deref(),
                    )
                    .map(|c| c.default_model)
                    .unwrap_or_else(|| "gpt-4o".into());
                    let session = store.create_session(&cwd.display().to_string(), &model)?;
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
                        println!("{}  {}  {}  {}", s.id, title, s.model, updated);
                    }
                }
            }
            return Ok(());
        }
        Some(Command::Compact { session }) => {
            let project_config_dir = scope.project().map(|p| p.root.join(".mu"));
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
            if store.get_session(&session)?.is_none() {
                bail!("session not found in active scope: {session}");
            }
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
    let attachments = load_image_attachments(&args.images)?;

    paths::ensure_project_layout(&scope)?;
    let project_config_dir = scope.project().map(|p| p.root.join(".mu"));
    let config = Config::load_for_scope(project_config_dir.as_deref())?;
    let api_key = config.api_key()?;
    let provider = Arc::new(OpenAiProvider::new(
        config.provider.base_url.clone(),
        api_key,
    )) as Arc<dyn Provider>;

    let state_dir = scope.state_dir();
    paths::ensure_dir(&state_dir)?;
    compaction::prune_spills(&state_dir);

    let db_path = scope.session_db_path();
    let store = store::Store::open(&db_path)?;

    let effective_model = args
        .model
        .clone()
        .unwrap_or_else(|| config.default_model.clone());

    let explicit_session = args.session.as_ref();
    let (session, created) = resolve_session(
        &store,
        explicit_session.map(String::as_str),
        args.continue_latest,
        &cwd,
        &effective_model,
        scope.project(),
    )?;
    let session_id = session.id.clone();
    let session_model = args.model.clone().unwrap_or(session.model);

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
        &session_model,
        &prompt,
        attachments,
        args.output,
        &state_dir,
        project_config_dir.as_deref(),
    )
    .await?;

    Ok(())
}

fn resolve_session(
    store: &store::Store,
    explicit: Option<&str>,
    continue_latest: bool,
    cwd: &std::path::Path,
    model: &str,
    project: Option<&paths::Project>,
) -> Result<(store::Session, bool)> {
    if explicit.is_some() && continue_latest {
        bail!("use either -s/--session or -c/--continue-latest, not both");
    }

    if let Some(id) = explicit {
        let session = store
            .get_session(id)?
            .ok_or_else(|| anyhow::anyhow!("session not found in active scope: {id}"))?;
        return Ok((session, false));
    }

    if continue_latest {
        if let Some(session) = store.latest_session()? {
            return Ok((session, false));
        }
    }

    let session = store.create_session(&cwd.display().to_string(), model)?;
    store.append_message(
        &session.id,
        &provider::Message::User {
            content: system_prompt::initial_environment_context(cwd, project, &session.id).into(),
        },
    )?;
    Ok((session, true))
}

async fn run_turn(
    config: &Config,
    provider: Arc<dyn Provider>,
    store: &store::Store,
    session_id: &str,
    model: &str,
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
        model: model.to_string(),
        renderer: &mut renderer,
        state_dir,
        system_prompt,
        attachments,
    };

    let result = agent.run_turn(prompt).await;

    match &result {
        Ok(r) => {
            let ctx_pct = config
                .context_window(model)
                .map(|cw| (r.final_total_tokens as f64 / cw as f64) * 100.0);
            store.update_session(session_id, r.final_total_tokens, r.cost, Some(&title))?;
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
