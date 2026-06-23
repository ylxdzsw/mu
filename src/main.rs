use std::io::{self, Read};
use std::path::PathBuf;
use std::process;
use std::sync::Arc;

use anyhow::{bail, Result};
use clap::Parser;

mod agent;
mod cli;
mod compaction;
mod config;
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
use renderer::Renderer;

const ZSH_PLUGIN: &str = include_str!("../shell-plugins/mu.zsh");

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        let mut r = Renderer::new();
        let _ = r.error(&e.to_string());
        process::exit(1);
    }
}

async fn run() -> Result<()> {
    let args = Args::parse();

    match args.command {
        Some(Command::Init { sub }) => match sub {
            Some(InitSub::Zsh) => {
                print!("{ZSH_PLUGIN}");
                return Ok(());
            }
            None => {
                let config_path = paths::config_dir().join("config.jsonc");
                Config::write_starter(&config_path)?;
                eprintln!("wrote starter config to {}", config_path.display());
                eprintln!("set {} and run a turn", "OPENAI_API_KEY");
                return Ok(());
            }
        },
        Some(Command::Session { sub }) => {
            let db_path = paths::state_dir().join("mu.db");
            let store = store::Store::open(&db_path)?;
            match sub {
                SessionSub::New => {
                    let cwd = std::env::current_dir()?.display().to_string();
                    let model = Config::try_load()
                        .map(|c| c.default_model)
                        .unwrap_or_else(|| "gpt-4o".into());
                    let session = store.create_session(&cwd, &model)?;
                    println!("{}", session.id);
                }
                SessionSub::List => {
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
            let config = Config::load()?;
            let api_key = config.api_key()?;
            let provider = Arc::new(OpenAiProvider::new(
                config.provider.base_url.clone(),
                api_key,
            )) as Arc<dyn Provider>;
            let db_path = paths::state_dir().join("mu.db");
            let store = store::Store::open(&db_path)?;
            let _lock = match store::acquire_session_lock(&session) {
                Ok(lock) => lock,
                Err(_) => {
                    eprintln!("session busy");
                    process::exit(2);
                }
            };
            let config_dir = paths::config_dir();
            let cwd = std::env::current_dir()?;
            let system_prompt =
                system_prompt::build_system_prompt(&config_dir, &cwd, Some(&store))?;
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

    let config = Config::load()?;
    let api_key = config.api_key()?;
    let provider = Arc::new(OpenAiProvider::new(
        config.provider.base_url.clone(),
        api_key,
    )) as Arc<dyn Provider>;

    let state_dir = paths::state_dir();
    paths::ensure_dir(&state_dir)?;
    compaction::prune_spills(&state_dir);

    let db_path = state_dir.join("mu.db");
    let store = store::Store::open(&db_path)?;

    let effective_model = args
        .model
        .clone()
        .unwrap_or_else(|| config.default_model.clone());

    let cwd = std::env::current_dir()?;
    let config_dir = paths::config_dir();

    // Session resolution
    let session_id: String;
    let session_model: String;

    if let Some(ref id) = args.session {
        let session = match store.get_session(id)? {
            Some(s) => s,
            None => {
                eprintln!("session not found: {id}");
                process::exit(2);
            }
        };
        session_id = session.id;
        session_model = args.model.clone().unwrap_or(session.model);
        let _lock = match store::acquire_session_lock(&session_id) {
            Ok(lock) => lock,
            Err(_) => {
                eprintln!("session busy");
                process::exit(2);
            }
        };
        run_turn(
            &config,
            provider,
            &store,
            &session_id,
            &session_model,
            &prompt,
            &state_dir,
            &config_dir,
            &cwd,
        )
        .await?;
    } else {
        let session = store.create_session(&cwd.display().to_string(), &effective_model)?;
        session_id = session.id.clone();
        session_model = effective_model.clone();

        let _lock = match store::acquire_session_lock(&session_id) {
            Ok(lock) => lock,
            Err(_) => {
                eprintln!("session busy");
                process::exit(2);
            }
        };

        if let Ok(session_file) = std::env::var("MU_SESSION_FILE") {
            store::write_session_id(PathBuf::from(&session_file).as_path(), &session_id)?;
        }

        run_turn(
            &config,
            provider,
            &store,
            &session_id,
            &session_model,
            &prompt,
            &state_dir,
            &config_dir,
            &cwd,
        )
        .await?;
    }

    Ok(())
}

async fn run_turn(
    config: &Config,
    provider: Arc<dyn Provider>,
    store: &store::Store,
    session_id: &str,
    model: &str,
    prompt: &str,
    state_dir: &std::path::Path,
    config_dir: &std::path::Path,
    cwd: &std::path::Path,
) -> Result<()> {
    let system_prompt = system_prompt::build_system_prompt(config_dir, cwd, Some(store))?;
    let title: String = prompt.chars().take(60).collect();

    let mut renderer = Renderer::new();
    let mut agent = agent::AgentLoop {
        config,
        provider,
        store,
        session_id,
        model: model.to_string(),
        renderer: &mut renderer,
        state_dir,
        system_prompt,
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
