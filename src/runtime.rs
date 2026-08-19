use anyhow::{Result, bail};
use serde::Serialize;
use std::process::Command;

use crate::compaction::soft_compaction_threshold;
use crate::config::Config;
use crate::models::{
    AvailableModelsPayload, ResolvedModelChoice, ResolvedModelRef, available_models,
    first_model_choice, resolve_model_choice, resolve_model_info,
};
use crate::skills::{CommandMeta, SkillMeta};
use crate::store::{Session, Store, UnsupportedSessionVersion};

#[derive(Debug, Clone, Default)]
pub struct InvocationOverrides {
    pub session: Option<String>,
    pub continue_current: bool,
    pub model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResolvedInvocation {
    pub attached_session: Option<Session>,
    pub model: ResolvedModelChoice,
    pub model_fallback: Option<ModelFallback>,
    pub ignored_current_session: Option<UnsupportedSessionVersion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelFallback {
    pub remembered: String,
    pub selected: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedModelSelection {
    pub model: ResolvedModelChoice,
    pub fallback: Option<ModelFallback>,
}

struct RememberedModelSelection {
    model: ResolvedModelChoice,
    unavailable: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusModel {
    pub provider_id: String,
    pub model_id: String,
    pub effort: Option<String>,
    pub canonical: String,
    pub replay_key: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub model: StatusModel,
    pub output: crate::OutputFormat,
    pub session_id: Option<String>,
    pub context_tokens: Option<u64>,
    pub context_usage_source: Option<ContextUsageSource>,
    pub project_root: Option<String>,
    pub context_window: Option<u64>,
    pub compaction_soft_threshold_tokens: Option<u64>,
    pub supported_effort_levels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<GitStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<StatusSession>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<StatusActiveTurn>,
    /// Whether the selected session's last turn finished cleanly. `false` means
    /// it was interrupted; the next prompt continues on top of it or `mu retry`
    /// resumes it. `true` when there is no selected session.
    pub clean: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compaction: Option<CompactionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_models: Option<AvailableModelsPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands: Option<Vec<CommandMeta>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skills: Option<Vec<SkillMeta>>,
    #[serde(skip)]
    pub ignored_current_session: Option<UnsupportedSessionVersion>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextUsageSource {
    Reported,
    Estimated,
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
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct StatusActiveTurn {
    pub busy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactionStatus {
    pub latest_summary_seq: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StatusIncludes {
    pub git: bool,
    pub session_details: bool,
    pub models: bool,
}

pub fn resolve_invocation(
    store: &Store,
    config: &Config,
    overrides: &InvocationOverrides,
) -> Result<ResolvedInvocation> {
    if overrides.session.is_some() && overrides.continue_current {
        bail!("use either -s/--session or -c/--continue, not both");
    }

    let (attached_session, mut ignored_current_session) =
        if let Some(id) = overrides.session.as_deref() {
            (
                Some(
                    store
                        .get_session(id)?
                        .ok_or_else(|| crate::ExitError::session_not_found(id))?,
                ),
                None,
            )
        } else if overrides.continue_current {
            match store.current_session() {
                Ok(session) => (session, None),
                Err(error) => (None, Some(error.downcast::<UnsupportedSessionVersion>()?)),
            }
        } else {
            (None, None)
        };

    let remembered = if attached_session.is_none()
        && ignored_current_session.is_none()
        && overrides.model.is_none()
    {
        let (remembered, ignored) = remembered_scope_model(store)?;
        ignored_current_session = ignored;
        remembered
    } else {
        None
    };
    let mut selection = if let Some(model_ref) = overrides.model.as_deref() {
        RememberedModelSelection {
            model: resolve_model_choice(config, model_ref)?,
            unavailable: None,
        }
    } else if let Some(session) = attached_session.as_ref() {
        resolve_session_selection(store, config, session)?
    } else {
        resolve_scope_selection(config, remembered)?
    };
    if attached_session.is_none() {
        selection.model.reset();
    } else if overrides.model.is_some()
        && let Some(session) = attached_session.as_ref()
    {
        resume_session_fallback(store, config, &session.id, &mut selection.model)?;
    }
    let selection = finish_selection(selection);

    Ok(ResolvedInvocation {
        attached_session,
        model: selection.model,
        model_fallback: selection.fallback,
        ignored_current_session,
    })
}

fn remembered_scope_model(
    store: &Store,
) -> Result<(Option<String>, Option<UnsupportedSessionVersion>)> {
    match store.current_session() {
        Ok(session) => Ok((session.and_then(|session| session.last_model), None)),
        Err(error) => match error.downcast::<UnsupportedSessionVersion>() {
            Ok(unsupported) => Ok((None, Some(unsupported))),
            Err(error) => Err(error),
        },
    }
}

fn resolve_scope_selection(
    config: &Config,
    remembered: Option<String>,
) -> Result<RememberedModelSelection> {
    if let Some(model_ref) = remembered.as_deref()
        && let Ok(mut model) = resolve_model_choice(config, model_ref)
    {
        model.reset();
        return Ok(RememberedModelSelection {
            model,
            unavailable: None,
        });
    }
    Ok(RememberedModelSelection {
        model: first_model_choice(config)?,
        unavailable: remembered,
    })
}

pub fn resolve_session_model(
    store: &Store,
    config: &Config,
    session: &Session,
) -> Result<ResolvedModelChoice> {
    Ok(resolve_session_selection(store, config, session)?.model)
}

fn resolve_session_selection(
    store: &Store,
    config: &Config,
    session: &Session,
) -> Result<RememberedModelSelection> {
    let mut selection = match session.last_model.as_deref() {
        Some(model_ref) => match resolve_model_choice(config, model_ref) {
            Ok(model) => RememberedModelSelection {
                model,
                unavailable: None,
            },
            Err(_) => {
                let mut selection =
                    resolve_scope_selection(config, remembered_scope_model(store)?.0)?;
                selection.unavailable = Some(model_ref.to_string());
                selection
            }
        },
        None => resolve_scope_selection(config, remembered_scope_model(store)?.0)?,
    };
    resume_session_fallback(store, config, &session.id, &mut selection.model)?;
    Ok(selection)
}

#[cfg(test)]
pub fn resolve_retry_model(
    store: &Store,
    config: &Config,
    session: &Session,
    override_ref: Option<&str>,
) -> Result<ResolvedModelChoice> {
    Ok(resolve_retry_model_selection(store, config, session, override_ref)?.model)
}

pub fn resolve_retry_model_selection(
    store: &Store,
    config: &Config,
    session: &Session,
    override_ref: Option<&str>,
) -> Result<ResolvedModelSelection> {
    if let Some(model) = override_ref {
        let mut choice = resolve_model_choice(config, model)?;
        resume_session_fallback(store, config, &session.id, &mut choice)?;
        return Ok(finish_selection(RememberedModelSelection {
            model: choice,
            unavailable: None,
        }));
    }
    if let Some(model) = store.latest_attempt_model(&session.id)? {
        if let Ok(mut choice) = resolve_model_choice(config, &model) {
            resume_session_fallback(store, config, &session.id, &mut choice)?;
            return Ok(finish_selection(RememberedModelSelection {
                model: choice,
                unavailable: None,
            }));
        }
        let mut selection = resolve_session_selection(store, config, session)?;
        selection.unavailable = Some(model);
        return Ok(finish_selection(selection));
    }
    resolve_session_selection(store, config, session).map(finish_selection)
}

fn finish_selection(selection: RememberedModelSelection) -> ResolvedModelSelection {
    let fallback = selection.unavailable.map(|remembered| ModelFallback {
        remembered,
        selected: selection.model.active_model().canonical.clone(),
    });
    ResolvedModelSelection {
        model: selection.model,
        fallback,
    }
}

pub fn resume_session_fallback(
    store: &Store,
    config: &Config,
    session_id: &str,
    choice: &mut ResolvedModelChoice,
) -> Result<()> {
    if !choice.is_floating() {
        return Ok(());
    }

    choice.reset();
    let model_id = choice.active_model().model_id.clone();
    if let Some(provider_id) =
        store.latest_floating_provider(session_id, &model_id, |provider_id| {
            config.model_config(provider_id, &model_id).is_some()
        })?
    {
        choice.resume_provider(&model_id, &provider_id);
    }
    Ok(())
}

pub fn build_status_report(
    store: &Store,
    config: &Config,
    overrides: &InvocationOverrides,
    project: Option<&crate::paths::Project>,
    includes: StatusIncludes,
    commands: Option<Vec<CommandMeta>>,
    skills: Option<Vec<SkillMeta>>,
) -> Result<StatusReport> {
    let resolved = resolve_invocation(store, config, overrides)?;
    let model_info = resolve_model_info(config, resolved.model.active_model());
    let (session_summary, active, compaction) = if includes.session_details {
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
        let compaction = resolved
            .attached_session
            .as_ref()
            .map(|session| {
                store
                    .latest_summary_sequence(&session.id)
                    .map(|latest_summary_seq| CompactionStatus { latest_summary_seq })
            })
            .transpose()?;
        (
            session_summary,
            Some(StatusActiveTurn { busy: active }),
            compaction,
        )
    } else {
        (None, None, None)
    };
    let clean = resolved
        .attached_session
        .as_ref()
        .map(|session| store.is_session_clean(&session.id))
        .transpose()?
        .unwrap_or(true);
    let model = status_model(config, resolved.model.active_model());
    let context_usage = context_usage(
        store,
        config,
        resolved.model.active_model(),
        resolved.attached_session.as_ref(),
    )?;

    Ok(StatusReport {
        model,
        output: config.output,
        session_id: resolved
            .attached_session
            .as_ref()
            .map(|session| session.id.clone()),
        context_tokens: context_usage.map(|(tokens, _)| tokens),
        context_usage_source: context_usage.map(|(_, source)| source),
        project_root: project.map(|project| project.root.display().to_string()),
        context_window: model_info.context_window,
        compaction_soft_threshold_tokens: if config.compaction.enabled {
            model_info
                .context_window
                .map(|window| soft_compaction_threshold(window, config.compaction.soft_fraction))
        } else {
            None
        },
        supported_effort_levels: model_info.supported_effort_levels,
        git: includes.git.then(|| project.map(git_status)).flatten(),
        session: session_summary.map(status_session),
        active,
        clean,
        compaction,
        available_models: includes.models.then(|| available_models(config)),
        commands,
        skills,
        ignored_current_session: resolved.ignored_current_session,
    })
}

fn status_model(config: &Config, model: &ResolvedModelRef) -> StatusModel {
    StatusModel {
        provider_id: model.provider_id.clone(),
        model_id: model.model_id.clone(),
        effort: model.effort.clone(),
        canonical: model.canonical.clone(),
        replay_key: config.replay_key(&model.provider_id, &model.model_id),
    }
}

fn context_usage(
    store: &Store,
    config: &Config,
    model: &ResolvedModelRef,
    session: Option<&Session>,
) -> Result<Option<(u64, ContextUsageSource)>> {
    let Some(session) = session else {
        return Ok(None);
    };
    let provider = config.provider(&model.provider_id)?;
    let api = crate::provider::classify_endpoint(&provider.endpoint)?;
    let estimate = store.context_tokens(&session.id, config, model, api)?;
    Ok(Some((
        estimate.tokens,
        if estimate.reported {
            ContextUsageSource::Reported
        } else {
            ContextUsageSource::Estimated
        },
    )))
}

fn status_session(summary: crate::store::SessionSummary) -> StatusSession {
    StatusSession {
        id: summary.id,
        title: summary.title,
        cwd: summary.cwd,
        created_at: summary.created_at,
        updated_at: summary.updated_at,
        message_count: summary.message_count,
        turn_count: summary.turn_count,
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
            .map(|info| info.git_dir.display().to_string()),
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
            providers: OrderedMap::from_iter([
                (
                    "alpha".into(),
                    ProviderConfig {
                        endpoint: "http://localhost/chat/completions".into(),
                        api_key_env: "MU_TEST_KEY".into(),
                        models: OrderedMap::from_iter([
                            (
                                "default-model".into(),
                                ModelConfig {
                                    context_window: Some(100),
                                    supported_efforts: Some(vec!["low".into(), "high".into()]),
                                    replay_key: None,
                                },
                            ),
                            (
                                "other-model".into(),
                                ModelConfig {
                                    context_window: Some(100),
                                    supported_efforts: None,
                                    replay_key: None,
                                },
                            ),
                        ]),
                    },
                ),
                (
                    "beta".into(),
                    ProviderConfig {
                        endpoint: "http://localhost/responses".into(),
                        api_key_env: String::new(),
                        models: OrderedMap::from_iter([
                            (
                                "default-model".into(),
                                ModelConfig {
                                    context_window: Some(200),
                                    supported_efforts: None,
                                    replay_key: None,
                                },
                            ),
                            (
                                "other-model".into(),
                                ModelConfig {
                                    context_window: Some(200),
                                    supported_efforts: None,
                                    replay_key: None,
                                },
                            ),
                        ]),
                    },
                ),
                (
                    "gamma".into(),
                    ProviderConfig {
                        endpoint: "http://localhost/chat/completions".into(),
                        api_key_env: String::new(),
                        models: OrderedMap::from_iter([
                            (
                                "default-model".into(),
                                ModelConfig {
                                    context_window: Some(300),
                                    supported_efforts: None,
                                    replay_key: None,
                                },
                            ),
                            (
                                "other-model".into(),
                                ModelConfig {
                                    context_window: Some(300),
                                    supported_efforts: None,
                                    replay_key: None,
                                },
                            ),
                        ]),
                    },
                ),
            ]),
            output: Default::default(),
            auto_resume: false,
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    fn finish_attempt(store: &Store, session_id: &str, model: &str, outcome: &str) {
        store
            .append_test_agent_exchange(session_id, model, outcome, 1)
            .unwrap();
    }

    fn finish_guardrail_attempt(store: &Store, config: &Config, session_id: &str, model: &str) {
        let turn_id = store
            .start_turn(session_id, "/tmp", None, &"test guardrail".into())
            .unwrap();
        let call = crate::provider::ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            arguments: r#"{"title":"Test","risk":"destructive","command":"true"}"#.into(),
        };
        let (_, call_ids) = store
            .append_message_with_bash_calls(
                session_id,
                &crate::provider::Message::assistant(None, None, Some(vec![call]), None),
            )
            .unwrap();
        let resolved = resolve_model_choice(config, model).unwrap();
        let request_model = resolved.active_model();
        let native = serde_json::json!({"model":request_model.model_id});
        let exchange_id = store
            .start_provider_request(
                session_id,
                &turn_id,
                "guardrail",
                crate::store::ProviderOrigin {
                    canonical_model_ref: model.into(),
                    provider_id: request_model.provider_id.clone(),
                    api: "test".into(),
                    endpoint: String::new(),
                    wire_model: request_model.model_id.clone(),
                    effort: request_model.effort.clone(),
                },
                store
                    .request_recipe("test.v1", &native, serde_json::json!({"kind":"guardrail"}))
                    .unwrap(),
                Some(crate::store::RequestSubject {
                    call_id: call_ids[0],
                    attempt: 1,
                }),
            )
            .unwrap();
        store
            .fail_provider_exchange(
                session_id,
                &exchange_id,
                "test",
                serde_json::json!({"message":"test failure"}),
                None,
                None,
            )
            .unwrap();
        store
            .persist_bash_result(
                session_id,
                crate::store::BashResultRecord {
                    bash_call_id: call_ids[0],
                    outcome: "error",
                    exit_code: None,
                    duration_ms: None,
                },
                "test",
                &[],
            )
            .unwrap();
    }

    #[test]
    fn new_scope_uses_first_configured_model() {
        let store = Store::open_memory().unwrap();

        let resolved =
            resolve_invocation(&store, &test_config(), &InvocationOverrides::default()).unwrap();

        assert_eq!(
            resolved.model.active_model().canonical,
            "alpha/default-model"
        );
    }

    #[test]
    fn unsupported_current_session_is_ignored_only_for_implicit_new_session() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session("/tmp").unwrap();
        store.select_session(&session.id).unwrap();
        store.set_session_version_for_test(&session.id, 1);

        let resolved =
            resolve_invocation(&store, &test_config(), &InvocationOverrides::default()).unwrap();
        assert!(resolved.attached_session.is_none());
        assert_eq!(
            resolved.ignored_current_session,
            Some(UnsupportedSessionVersion {
                session_id: Some(session.id.clone()),
                found: 1,
                supported: 2,
            })
        );

        let continued = resolve_invocation(
            &store,
            &test_config(),
            &InvocationOverrides {
                continue_current: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(continued.attached_session.is_none());
        assert!(continued.ignored_current_session.is_some());

        let explicit = InvocationOverrides {
            session: Some(session.id),
            ..Default::default()
        };
        assert!(
            resolve_invocation(&store, &test_config(), &explicit)
                .unwrap_err()
                .downcast_ref::<UnsupportedSessionVersion>()
                .is_some()
        );
    }

    #[test]
    fn explicit_model_override_wins_for_new_session() {
        let store = Store::open_memory().unwrap();

        let resolved = resolve_invocation(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: None,
                continue_current: false,
                model: Some("alpha/default-model:low".into()),
            },
        )
        .unwrap();

        assert_eq!(
            resolved.model.active_model().canonical,
            "alpha/default-model:low"
        );
        assert!(
            resolve_invocation(
                &store,
                &test_config(),
                &InvocationOverrides {
                    model: Some("alpha/removed-model".into()),
                    ..Default::default()
                },
            )
            .is_err()
        );
    }

    #[test]
    fn removed_scope_model_falls_back_for_status() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session("/tmp").unwrap();
        finish_attempt(
            &store,
            &session.id,
            "(removed)/removed-model:high",
            "completed",
        );
        store.select_session(&session.id).unwrap();

        let resolved =
            resolve_invocation(&store, &test_config(), &InvocationOverrides::default()).unwrap();
        let report = build_status_report(
            &store,
            &test_config(),
            &InvocationOverrides::default(),
            None,
            StatusIncludes::default(),
            None,
            None,
        )
        .unwrap();

        assert_eq!(
            resolved.model.active_model().canonical,
            "alpha/default-model"
        );
        assert_eq!(
            resolved.model_fallback,
            Some(ModelFallback {
                remembered: "(removed)/removed-model:high".into(),
                selected: "alpha/default-model".into(),
            })
        );
        assert_eq!(report.model.canonical, "alpha/default-model");
    }

    #[test]
    fn removed_attached_and_retry_models_continue_resolution_precedence() {
        let store = Store::open_memory().unwrap();
        let attached = store.create_session("/tmp").unwrap();
        finish_attempt(&store, &attached.id, "removed/removed-model", "completed");
        let current = store.create_session("/tmp").unwrap();
        finish_attempt(&store, &current.id, "alpha/other-model:high", "completed");
        store.select_session(&current.id).unwrap();
        let attached = store.get_session(&attached.id).unwrap().unwrap();

        let resolved = resolve_invocation(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: Some(attached.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let retry = resolve_retry_model_selection(&store, &test_config(), &attached, None).unwrap();

        assert_eq!(
            resolved.model.active_model().canonical,
            "alpha/other-model:high"
        );
        assert_eq!(
            resolved.model_fallback.as_ref().unwrap().remembered,
            "removed/removed-model"
        );
        assert_eq!(
            retry.model.active_model().canonical,
            "alpha/other-model:high"
        );
        assert_eq!(
            retry.fallback,
            Some(ModelFallback {
                remembered: "removed/removed-model".into(),
                selected: "alpha/other-model:high".into(),
            })
        );
    }

    #[test]
    fn session_attempts_do_not_leak_models_across_sessions() {
        let store = Store::open_memory().unwrap();
        let completed = store.create_session("/tmp").unwrap();
        finish_attempt(
            &store,
            &completed.id,
            "alpha/default-model:high",
            "completed",
        );
        let empty = store.create_session("/tmp").unwrap();
        let newer = store.create_session("/tmp").unwrap();
        finish_attempt(&store, &newer.id, "alpha/default-model:low", "completed");

        let attached = resolve_invocation(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: Some(completed.id),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            attached.model.active_model().canonical,
            "alpha/default-model:high"
        );

        let empty = resolve_invocation(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: Some(empty.id),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(empty.model.active_model().canonical, "alpha/default-model");
    }

    #[test]
    fn normal_and_retry_use_the_sessions_latest_requested_model() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session("/tmp").unwrap();
        finish_attempt(&store, &session.id, "alpha/default-model:high", "completed");
        finish_attempt(&store, &session.id, "alpha/default-model:low", "error");
        let session = store.get_session(&session.id).unwrap().unwrap();

        let normal = resolve_session_model(&store, &test_config(), &session).unwrap();
        let retry = resolve_retry_model(&store, &test_config(), &session, None).unwrap();
        let overridden = resolve_retry_model(
            &store,
            &test_config(),
            &session,
            Some("alpha/default-model"),
        )
        .unwrap();

        assert_eq!(normal.active_model().canonical, "alpha/default-model:low");
        assert_eq!(retry.active_model().canonical, "alpha/default-model:low");
        assert_eq!(overridden.active_model().canonical, "alpha/default-model");
    }

    #[test]
    fn floating_provider_position_is_session_local_and_effort_independent() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session("/tmp").unwrap();
        finish_attempt(&store, &session.id, "(beta)/default-model:low", "completed");
        let session = store.get_session(&session.id).unwrap().unwrap();

        let attached = resolve_session_model(&store, &test_config(), &session).unwrap();
        assert_eq!(
            attached.active_model().canonical,
            "(beta)/default-model:low"
        );

        let changed_effort = resolve_invocation(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: Some(session.id.clone()),
                model: Some("default-model:high".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            changed_effort.model.active_model().canonical,
            "(beta)/default-model:high"
        );

        let new_session = store.create_session("/tmp").unwrap();
        store.select_session(&session.id).unwrap();
        let inherited = resolve_invocation(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: Some(new_session.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            inherited.model.active_model().canonical,
            "(alpha)/default-model:low"
        );
        assert_eq!(inherited.attached_session.unwrap().id, new_session.id);

        let explicit_new = resolve_invocation(
            &store,
            &test_config(),
            &InvocationOverrides {
                model: Some("(beta)/default-model:high".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            explicit_new.model.active_model().canonical,
            "(alpha)/default-model:high"
        );
    }

    #[test]
    fn exhausted_floating_choice_sticks_to_last_provider_and_remains_retryable() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session("/tmp").unwrap();
        finish_attempt(&store, &session.id, "(alpha)/default-model:max", "error");
        finish_attempt(&store, &session.id, "(beta)/default-model:max", "error");
        let session = store.get_session(&session.id).unwrap().unwrap();

        let report = build_status_report(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: Some(session.id.clone()),
                model: Some("(alpha)/default-model:max".into()),
                ..Default::default()
            },
            None,
            StatusIncludes::default(),
            None,
            None,
        )
        .unwrap();
        let retry = resolve_retry_model(&store, &test_config(), &session, None).unwrap();

        assert_eq!(report.model.canonical, "(beta)/default-model:max");
        assert!(!report.clean);
        assert_eq!(retry.active_model().canonical, "(beta)/default-model:max");
    }

    #[test]
    fn floating_provider_position_is_remembered_by_model_across_intervening_choices() {
        let store = Store::open_memory().unwrap();
        let config = test_config();
        let session = store.create_session("/tmp").unwrap();
        finish_attempt(&store, &session.id, "(beta)/default-model:low", "completed");
        finish_attempt(&store, &session.id, "(alpha)/other-model", "completed");
        finish_attempt(&store, &session.id, "alpha/default-model:high", "completed");

        let resumed = resolve_retry_model(
            &store,
            &config,
            &store.get_session(&session.id).unwrap().unwrap(),
            Some("default-model:high"),
        )
        .unwrap();
        assert_eq!(
            resumed.active_model().canonical,
            "(beta)/default-model:high"
        );

        finish_guardrail_attempt(&store, &config, &session.id, "alpha/default-model");
        let after_fixed_guardrail = resolve_retry_model(
            &store,
            &config,
            &store.get_session(&session.id).unwrap().unwrap(),
            Some("default-model"),
        )
        .unwrap();
        assert_eq!(
            after_fixed_guardrail.active_model().canonical,
            "(beta)/default-model"
        );
    }

    #[test]
    fn floating_guardrail_attempt_advances_the_sessions_same_model_cursor() {
        let store = Store::open_memory().unwrap();
        let config = test_config();
        let session = store.create_session("/tmp").unwrap();
        finish_attempt(&store, &session.id, "(alpha)/default-model", "completed");
        finish_guardrail_attempt(&store, &config, &session.id, "(beta)/default-model");

        let resumed = resolve_retry_model(
            &store,
            &config,
            &store.get_session(&session.id).unwrap().unwrap(),
            Some("default-model:high"),
        )
        .unwrap();
        assert_eq!(
            resumed.active_model().canonical,
            "(beta)/default-model:high"
        );
    }

    #[test]
    fn missing_floating_provider_is_skipped_before_defaulting_or_erroring() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session("/tmp").unwrap();
        finish_attempt(
            &store,
            &session.id,
            "(gamma)/default-model:low",
            "completed",
        );
        finish_attempt(&store, &session.id, "(beta)/default-model:low", "completed");
        let session = store.get_session(&session.id).unwrap().unwrap();

        let mut config = test_config();
        config
            .providers
            .iter_mut()
            .find(|(id, _)| id.as_str() == "beta")
            .unwrap()
            .1
            .models = OrderedMap::default();
        let resumed = resolve_session_model(&store, &config, &session).unwrap();
        assert_eq!(
            resumed.active_model().canonical,
            "(gamma)/default-model:low"
        );

        config
            .providers
            .iter_mut()
            .find(|(id, _)| id.as_str() == "alpha")
            .unwrap()
            .1
            .models = OrderedMap::default();
        config
            .providers
            .iter_mut()
            .find(|(id, _)| id.as_str() == "gamma")
            .unwrap()
            .1
            .models = OrderedMap::default();
        assert!(resolve_session_model(&store, &config, &session).is_err());
    }

    #[test]
    fn status_report_reports_cleanliness() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session("/tmp").unwrap();
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
                continue_current: false,
                model: Some("alpha/default-model".into()),
            },
            None,
            StatusIncludes::default(),
            None,
            None,
        )
        .unwrap();

        assert!(!report.clean);

        // A completed assistant reply => clean.
        store
            .append_message(
                &session.id,
                &crate::provider::Message::assistant(Some("hello".into()), None, None, None),
            )
            .unwrap();
        let report = build_status_report(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: Some(session.id),
                continue_current: false,
                model: Some("alpha/default-model".into()),
            },
            None,
            StatusIncludes::default(),
            None,
            None,
        )
        .unwrap();
        assert!(report.clean);
    }

    #[test]
    fn context_usage_reports_when_exact_and_marks_post_compaction_estimates() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session_seeded("system prompt").unwrap();
        let overrides = InvocationOverrides {
            session: Some(session.id.clone()),
            continue_current: false,
            model: None,
        };

        let initial = build_status_report(
            &store,
            &test_config(),
            &overrides,
            None,
            StatusIncludes::default(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            initial.context_usage_source,
            Some(ContextUsageSource::Estimated)
        );

        store
            .append_test_agent_exchange(&session.id, "alpha/default-model", "completed", 25)
            .unwrap();
        let reported = build_status_report(
            &store,
            &test_config(),
            &overrides,
            None,
            StatusIncludes::default(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(reported.context_tokens, Some(25));
        assert_eq!(reported.compaction_soft_threshold_tokens, Some(70));
        assert_eq!(
            reported.context_usage_source,
            Some(ContextUsageSource::Reported)
        );

        store
            .start_turn(&session.id, "/tmp", None, &"12345678".into())
            .unwrap();
        let anchored = build_status_report(
            &store,
            &test_config(),
            &overrides,
            None,
            StatusIncludes::default(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(anchored.context_tokens, Some(27));
        assert_eq!(
            anchored.context_usage_source,
            Some(ContextUsageSource::Estimated)
        );

        let mut disabled_config = test_config();
        disabled_config.compaction.enabled = false;
        let disabled = build_status_report(
            &store,
            &disabled_config,
            &overrides,
            None,
            StatusIncludes::default(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(disabled.compaction_soft_threshold_tokens, None);

        store.append_summary(&session.id, "summary").unwrap();
        let estimated = build_status_report(
            &store,
            &test_config(),
            &overrides,
            None,
            StatusIncludes::default(),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            estimated.context_usage_source,
            Some(ContextUsageSource::Estimated)
        );
        assert_ne!(estimated.context_tokens, Some(25));
    }

    #[test]
    fn status_report_only_builds_requested_session_details() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session("/tmp").unwrap();
        let overrides = InvocationOverrides {
            session: Some(session.id),
            continue_current: false,
            model: None,
        };

        let lean = build_status_report(
            &store,
            &test_config(),
            &overrides,
            None,
            StatusIncludes::default(),
            None,
            None,
        )
        .unwrap();
        let lean_json = serde_json::to_value(&lean).unwrap();
        assert_eq!(lean_json["output"], "detail");
        assert!(lean.session.is_none());
        assert!(lean.active.is_none());
        assert!(lean.compaction.is_none());
        assert!(lean_json.get("session").is_none());
        assert!(lean_json.get("active").is_none());
        assert!(lean_json.get("compaction").is_none());

        let detailed = build_status_report(
            &store,
            &test_config(),
            &overrides,
            None,
            StatusIncludes {
                session_details: true,
                ..StatusIncludes::default()
            },
            None,
            None,
        )
        .unwrap();
        assert!(detailed.session.is_some());
        assert!(detailed.active.is_some());
        assert!(detailed.compaction.is_some());
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
