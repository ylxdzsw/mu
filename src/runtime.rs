use anyhow::{Result, bail};
use serde::Serialize;
use std::process::Command;

use crate::config::Config;
use crate::models::{
    AvailableModelsPayload, RequestOptions, ResolvedModelInfo, ResolvedModelRef, available_models,
    first_model_ref, resolve_model_info, resolve_model_ref,
};
use crate::skills::{CommandMeta, SkillMeta};
use crate::store::{Session, Store};

#[derive(Debug, Clone, Default)]
pub struct InvocationOverrides {
    pub session: Option<String>,
    pub continue_latest: bool,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedInvocation {
    pub attached_session: Option<Session>,
    pub request: RequestOptions,
    pub session_seed: RequestOptions,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusModel {
    pub provider_id: String,
    pub model_id: String,
    pub effort: Option<String>,
    pub canonical: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub model: StatusModel,
    pub session_id: Option<String>,
    pub context_percent: Option<f64>,
    pub project_root: Option<String>,
    pub context_window: Option<u64>,
    pub supported_effort_levels: Vec<String>,
    pub git: Option<GitStatus>,
    pub session: Option<StatusSession>,
    pub active: StatusActiveTurn,
    /// Whether the selected session's last turn finished cleanly. `false` means
    /// it was interrupted; the next prompt continues on top of it or `mu retry`
    /// resumes it. `true` when there is no selected session.
    pub clean: bool,
    pub compaction: Option<CompactionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_models: Option<AvailableModelsPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<CommandMeta>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<SkillMeta>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub dirty: Option<bool>,
    pub git_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusSession {
    pub id: String,
    pub title: Option<String>,
    pub cwd: String,
    pub created_at: String,
    pub updated_at: String,
    pub message_count: u64,
    pub turn_count: u64,
    pub last_total_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct StatusActiveTurn {
    pub busy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactionStatus {
    pub latest_summary_seq: Option<i64>,
}

pub fn resolve_invocation(
    store: &Store,
    config: &Config,
    overrides: &InvocationOverrides,
) -> Result<ResolvedInvocation> {
    if overrides.session.is_some() && overrides.continue_latest {
        bail!("use either -s/--session or -c/--continue-latest, not both");
    }

    let attached_session = if let Some(id) = overrides.session.as_deref() {
        Some(
            store
                .get_session(id)?
                .ok_or_else(|| crate::ExitError::session_not_found(id))?,
        )
    } else if overrides.continue_latest {
        store.latest_session()?
    } else {
        None
    };

    if let Some(session) = attached_session {
        let stored_model = session.model.clone();
        let request_ref = overrides.model.as_deref().unwrap_or(&stored_model);
        return Ok(ResolvedInvocation {
            attached_session: Some(session),
            request: RequestOptions {
                model: resolve_model_ref(config, request_ref)?,
            },
            session_seed: RequestOptions {
                model: resolve_model_ref(config, &stored_model)?,
            },
        });
    }

    let seed_model = if let Some(model_ref) = overrides.model.as_deref() {
        resolve_model_ref(config, model_ref)?
    } else if let Some(session) = store.latest_session()? {
        resolve_model_ref(config, &session.model)?
    } else {
        first_model_ref(config)?
    };
    let request_model = if let Some(model_ref) = overrides.model.as_deref() {
        resolve_model_ref(config, model_ref)?
    } else {
        seed_model.clone()
    };

    Ok(ResolvedInvocation {
        attached_session: None,
        request: RequestOptions {
            model: request_model,
        },
        session_seed: RequestOptions { model: seed_model },
    })
}

pub fn build_status_report(
    store: &Store,
    config: &Config,
    overrides: &InvocationOverrides,
    project: Option<&crate::paths::Project>,
    include_models: bool,
    commands: Option<Vec<CommandMeta>>,
    skills: Option<Vec<SkillMeta>>,
) -> Result<StatusReport> {
    let resolved = resolve_invocation(store, config, overrides)?;
    let model_info = resolve_model_info(config, &resolved.request.model);
    let session_summary = resolved
        .attached_session
        .as_ref()
        .map(|session| store.session_summary(&session.id))
        .transpose()?
        .flatten();
    let active = resolved
        .attached_session
        .as_ref()
        .map(|session| store.is_session_busy(&session.id))
        .transpose()?
        .unwrap_or(false);
    let clean = resolved
        .attached_session
        .as_ref()
        .map(|session| store.is_session_clean(&session.id))
        .transpose()?
        .unwrap_or(true);
    let compaction = resolved
        .attached_session
        .as_ref()
        .map(|session| {
            store
                .latest_summary_sequence(&session.id)
                .map(|latest_summary_seq| CompactionStatus { latest_summary_seq })
        })
        .transpose()?;
    let model = status_model(&resolved.request.model);

    Ok(StatusReport {
        model,
        session_id: resolved
            .attached_session
            .as_ref()
            .map(|session| session.id.clone()),
        context_percent: context_percent(store, resolved.attached_session.as_ref(), &model_info),
        project_root: project.map(|project| crate::windows_msys2::display_path(&project.root)),
        context_window: model_info.context_window,
        supported_effort_levels: model_info.supported_effort_levels,
        git: project.map(git_status),
        session: session_summary.map(status_session),
        active: StatusActiveTurn { busy: active },
        clean,
        compaction,
        available_models: include_models.then(|| available_models(config)),
        commands,
        skills,
    })
}

fn status_model(model: &ResolvedModelRef) -> StatusModel {
    StatusModel {
        provider_id: model.provider_id.clone(),
        model_id: model.model_id.clone(),
        effort: model.effort.clone(),
        canonical: model.canonical.clone(),
    }
}

fn context_percent(
    store: &Store,
    session: Option<&Session>,
    model_info: &ResolvedModelInfo,
) -> Option<f64> {
    let session = session?;
    let context_window = model_info.context_window?;
    let tokens = if session.last_total_tokens > 0 {
        session.last_total_tokens
    } else {
        store.estimate_context_tokens(&session.id)
    };
    Some((tokens as f64 / context_window as f64) * 100.0)
}

fn status_session(summary: crate::store::SessionSummary) -> StatusSession {
    StatusSession {
        id: summary.id,
        title: summary.title,
        cwd: crate::windows_msys2::display_path(std::path::Path::new(&summary.cwd)),
        created_at: summary.created_at,
        updated_at: summary.updated_at,
        message_count: summary.message_count,
        turn_count: summary.turn_count,
        last_total_tokens: summary.last_total_tokens,
    }
}

fn git_status(project: &crate::paths::Project) -> GitStatus {
    let checkout_root = git_checkout_root(project);
    let (branch, dirty) = git_branch_and_dirty(checkout_root).unwrap_or((None, None));
    GitStatus {
        branch,
        dirty,
        git_dir: project
            .worktree
            .as_ref()
            .map(|info| crate::windows_msys2::display_path(&info.git_dir)),
    }
}

fn git_checkout_root(project: &crate::paths::Project) -> &std::path::Path {
    project
        .worktree
        .as_ref()
        .map(|worktree| worktree.root.as_path())
        .unwrap_or(&project.root)
}

fn git_branch_and_dirty(project_root: &std::path::Path) -> Option<(Option<String>, Option<bool>)> {
    let output = Command::new("git")
        .arg("status")
        .arg("--porcelain=v2")
        .arg("-b")
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_git_status_output(&output.stdout))
}

fn parse_git_status_output(output: &[u8]) -> (Option<String>, Option<bool>) {
    let mut branch = None;
    let mut dirty = false;

    for line in String::from_utf8_lossy(output).lines() {
        if let Some(head) = line.strip_prefix("# branch.head ") {
            if !head.is_empty() && head != "(detached)" && head != "(unknown)" {
                branch = Some(head.to_string());
            }
            continue;
        }
        if !line.is_empty() && !line.starts_with('#') {
            dirty = true;
        }
    }

    (branch, Some(dirty))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{
        CompactionConfig, Config, GuardrailConfig, LimitsConfig, ModelConfig, OrderedMap,
        ProviderConfig, RedactionConfig, TerminalBellConfig,
    };

    fn test_config() -> Config {
        Config {
            providers: OrderedMap::from_iter([(
                "alpha".into(),
                ProviderConfig {
                    endpoint: "http://localhost/chat/completions".into(),
                    api_key_env: "MU_TEST_KEY".into(),
                    models: OrderedMap::from_iter([(
                        "default-model".into(),
                        ModelConfig {
                            context_window: Some(100),
                            supported_efforts: Some(vec!["low".into(), "high".into()]),
                        },
                    )]),
                },
            )]),
            output: Default::default(),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    #[test]
    fn new_scope_uses_first_configured_model() {
        let store = Store::open_memory().unwrap();

        let resolved =
            resolve_invocation(&store, &test_config(), &InvocationOverrides::default()).unwrap();

        assert_eq!(resolved.request.model.canonical, "alpha/default-model");
        assert_eq!(resolved.session_seed.model.canonical, "alpha/default-model");
    }

    #[test]
    fn explicit_model_override_seeds_new_session_with_override() {
        let store = Store::open_memory().unwrap();
        store
            .create_session("/tmp", "alpha/default-model:high")
            .unwrap();

        let resolved = resolve_invocation(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: None,
                continue_latest: false,
                model: Some("alpha/default-model:low".into()),
            },
        )
        .unwrap();

        assert_eq!(resolved.request.model.canonical, "alpha/default-model:low");
        assert_eq!(
            resolved.session_seed.model.canonical,
            "alpha/default-model:low"
        );
    }

    #[test]
    fn status_report_reports_cleanliness() {
        let store = Store::open_memory().unwrap();
        let session = store
            .create_session("/tmp", "alpha/default-model:high")
            .unwrap();
        // A user prompt with no assistant reply => interrupted => unclean.
        store
            .append_message(
                &session.id,
                &crate::provider::Message::User {
                    content: crate::provider::UserContent::Text("hi".into()),
                },
            )
            .unwrap();
        store
            .append_message(
                &session.id,
                &crate::provider::Message::User {
                    content: crate::provider::UserContent::Text("retry".into()),
                },
            )
            .unwrap();

        let report = build_status_report(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: Some(session.id.clone()),
                continue_latest: false,
                model: None,
            },
            None,
            false,
            None,
            None,
        )
        .unwrap();

        assert!(!report.clean);

        // A completed assistant reply => clean.
        store
            .append_message(
                &session.id,
                &crate::provider::Message::Assistant {
                    content: Some("hello".into()),
                    reasoning_content: None,
                    native_replay: None,
                    tool_calls: None,
                },
            )
            .unwrap();
        let report = build_status_report(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: Some(session.id.clone()),
                continue_latest: false,
                model: None,
            },
            None,
            false,
            None,
            None,
        )
        .unwrap();
        assert!(report.clean);
    }

    #[test]
    fn parses_git_status_output_for_clean_branch() {
        let (branch, dirty) =
            parse_git_status_output(b"# branch.oid abc123\n# branch.head master\n");

        assert_eq!(branch.as_deref(), Some("master"));
        assert_eq!(dirty, Some(false));
    }

    #[test]
    fn parses_git_status_output_for_detached_dirty_repo() {
        let (branch, dirty) = parse_git_status_output(
            b"# branch.oid abc123\n# branch.head (detached)\n1 M. N... 100644 100644 100644 abc def file.txt\n",
        );

        assert_eq!(branch, None);
        assert_eq!(dirty, Some(true));
    }

    #[test]
    fn git_status_uses_the_linked_checkout_root() {
        let project = crate::paths::Project {
            root: std::path::PathBuf::from("/tmp/primary"),
            marker: crate::paths::ProjectMarker::Git,
            worktree: Some(crate::paths::GitWorktreeInfo {
                root: std::path::PathBuf::from("/tmp/linked"),
                git_dir: std::path::PathBuf::from("/tmp/primary/.git/worktrees/linked"),
                common_dir: Some(std::path::PathBuf::from("/tmp/primary/.git")),
            }),
        };

        assert_eq!(
            git_checkout_root(&project),
            std::path::Path::new("/tmp/linked")
        );
    }
}
