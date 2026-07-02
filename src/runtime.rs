use anyhow::{Result, bail};
use serde::Serialize;
use std::process::Command;

use crate::config::Config;
use crate::models::{
    AvailableModelsPayload, RequestOptions, ResolvedModelInfo, ResolvedModelRef, available_models,
    resolve_model_info, resolve_model_ref,
};
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
    pub effort: Option<crate::models::EffortLevel>,
    pub canonical: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub model_id: String,
    pub model: StatusModel,
    pub session_id: Option<String>,
    pub context_percent: Option<f64>,
    pub project_root: Option<String>,
    pub context_window: Option<u64>,
    pub supported_effort_levels: Vec<crate::models::EffortLevel>,
    pub git: Option<GitStatus>,
    pub session: Option<StatusSession>,
    pub active: StatusActiveTurn,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_turn: Option<IncompleteTurnStatus>,
    pub compaction: Option<CompactionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_models: Option<AvailableModelsPayload>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GitStatus {
    pub branch: Option<String>,
    pub dirty: Option<bool>,
    pub git_dir: Option<String>,
    pub common_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusSession {
    pub id: String,
    pub title: Option<String>,
    pub cwd: String,
    pub origin: crate::store::SessionOrigin,
    pub archived: bool,
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
pub struct IncompleteTurnStatus {
    pub retry_count: u64,
    pub error_message: Option<String>,
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
                .ok_or_else(|| anyhow::anyhow!("session not found in active scope: {id}"))?,
        )
    } else if overrides.continue_latest {
        store.latest_session()?
    } else {
        None
    };

    if let Some(session) = attached_session.clone() {
        let request_ref = overrides.model.as_deref().unwrap_or(&session.model);
        return Ok(ResolvedInvocation {
            attached_session: Some(session.clone()),
            request: RequestOptions {
                model: resolve_model_ref(config, request_ref)?,
            },
            session_seed: RequestOptions {
                model: resolve_model_ref(config, &session.model)?,
            },
        });
    }

    let latest_scope_session = store.latest_session()?;
    let seed_ref = overrides
        .model
        .as_deref()
        .or_else(|| {
            latest_scope_session
                .as_ref()
                .map(|session| session.model.as_str())
        })
        .unwrap_or(config.default_model.as_str());
    let request_ref = overrides.model.as_deref().unwrap_or(seed_ref);

    Ok(ResolvedInvocation {
        attached_session: None,
        request: RequestOptions {
            model: resolve_model_ref(config, request_ref)?,
        },
        session_seed: RequestOptions {
            model: resolve_model_ref(config, seed_ref)?,
        },
    })
}

pub fn build_status_report(
    store: &Store,
    config: &Config,
    overrides: &InvocationOverrides,
    project: Option<&crate::paths::Project>,
    include_models: bool,
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
        .is_some_and(|session| store.is_session_busy(&session.id));
    let incomplete_turn = resolved
        .attached_session
        .as_ref()
        .map(|session| {
            store.pending_turn(&session.id).map(|pending| {
                pending.and_then(|pending| match pending.state {
                    crate::store::PendingState::Incomplete => Some(IncompleteTurnStatus {
                        retry_count: pending.retry_count,
                        error_message: pending.error_message,
                    }),
                    crate::store::PendingState::Running if !active => Some(IncompleteTurnStatus {
                        retry_count: pending.retry_count,
                        error_message: Some(
                            pending
                                .error_message
                                .unwrap_or_else(|| "previous turn was interrupted".to_string()),
                        ),
                    }),
                    crate::store::PendingState::Running => None,
                })
            })
        })
        .transpose()?
        .flatten();
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
        model_id: model.canonical.clone(),
        model,
        session_id: resolved
            .attached_session
            .as_ref()
            .map(|session| session.id.clone()),
        context_percent: context_percent(store, resolved.attached_session.as_ref(), &model_info),
        project_root: project.map(|project| project.root.display().to_string()),
        context_window: model_info.context_window,
        supported_effort_levels: model_info.supported_effort_levels,
        git: project.map(git_status),
        session: session_summary.map(status_session),
        active: StatusActiveTurn { busy: active },
        incomplete_turn,
        compaction,
        available_models: include_models.then(|| available_models(config)),
    })
}

fn status_model(model: &ResolvedModelRef) -> StatusModel {
    StatusModel {
        provider_id: model.provider_id.clone(),
        model_id: model.model_id.clone(),
        effort: model.effort,
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
        cwd: summary.cwd,
        origin: summary.origin,
        archived: summary.archived,
        created_at: summary.created_at,
        updated_at: summary.updated_at,
        message_count: summary.message_count,
        turn_count: summary.turn_count,
        last_total_tokens: summary.last_total_tokens,
    }
}

fn git_status(project: &crate::paths::Project) -> GitStatus {
    GitStatus {
        branch: git_branch(&project.root),
        dirty: git_dirty(&project.root),
        git_dir: project
            .worktree
            .as_ref()
            .map(|info| info.git_dir.display().to_string()),
        common_dir: project
            .worktree
            .as_ref()
            .and_then(|info| info.common_dir.as_ref())
            .map(|path| path.display().to_string()),
    }
}

fn git_branch(project_root: &std::path::Path) -> Option<String> {
    let output = Command::new("git")
        .arg("branch")
        .arg("--show-current")
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

fn git_dirty(project_root: &std::path::Path) -> Option<bool> {
    let output = Command::new("git")
        .arg("status")
        .arg("--porcelain")
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(!output.stdout.is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{
        CompactionConfig, Config, GuardrailConfig, LimitsConfig, ModelConfig, ProviderConfig,
        RedactionConfig, TerminalBellConfig,
    };
    use crate::models::EffortLevel;

    fn test_config() -> Config {
        Config {
            providers: HashMap::from([(
                "alpha".into(),
                ProviderConfig {
                    base_url: "http://localhost".into(),
                    api_key_env: "MU_TEST_KEY".into(),
                    models: HashMap::from([(
                        "default-model".into(),
                        ModelConfig {
                            context_window: Some(100),
                            price_per_mtok: None,
                            supported_efforts: Some(vec![EffortLevel::Low, EffortLevel::High]),
                        },
                    )]),
                },
            )]),
            default_model: "alpha/default-model:low".into(),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    #[test]
    fn explicit_model_override_seeds_new_session_with_override() {
        let store = Store::open_memory().unwrap();
        store
            .create_session_with_origin(
                "/tmp",
                "alpha/default-model:high",
                crate::store::SessionOrigin::Cli,
            )
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
    fn status_report_surfaces_incomplete_turn() {
        let store = Store::open_memory().unwrap();
        let session = store
            .create_session_with_origin(
                "/tmp",
                "alpha/default-model:high",
                crate::store::SessionOrigin::Cli,
            )
            .unwrap();
        store
            .begin_pending_turn(
                &session.id,
                &crate::provider::UserContent::Text("retry".into()),
            )
            .unwrap();
        store
            .mark_pending_incomplete(&session.id, "previous turn was interrupted")
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
        )
        .unwrap();

        let incomplete = report.incomplete_turn.expect("incomplete turn");
        assert_eq!(incomplete.retry_count, 0);
        assert_eq!(
            incomplete.error_message.as_deref(),
            Some("previous turn was interrupted")
        );
    }
}
