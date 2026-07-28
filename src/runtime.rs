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
    pub context_usage_source: Option<ContextUsageSource>,
    pub project_root: Option<String>,
    pub context_window: Option<u64>,
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
    pub last_context_tokens: u64,
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

    let model = if let Some(model_ref) = overrides.model.as_deref() {
        resolve_model_ref(config, model_ref)?
    } else if let Some(session) = attached_session.as_ref() {
        resolve_session_model(store, config, session)?
    } else {
        resolve_scope_model(store, config)?
    };

    Ok(ResolvedInvocation {
        attached_session,
        request: RequestOptions { model },
    })
}

pub fn resolve_scope_model(
    store: &Store,
    config: &Config,
) -> Result<crate::models::ResolvedModelRef> {
    if let Some(model) = store.latest_completed_model()? {
        resolve_model_ref(config, &model)
    } else {
        first_model_ref(config)
    }
}

pub fn resolve_session_model(
    store: &Store,
    config: &Config,
    session: &Session,
) -> Result<crate::models::ResolvedModelRef> {
    if let Some(model) = session.last_model.as_deref() {
        resolve_model_ref(config, model)
    } else {
        resolve_scope_model(store, config)
    }
}

pub fn resolve_retry_model(
    store: &Store,
    config: &Config,
    session: &Session,
    override_ref: Option<&str>,
) -> Result<crate::models::ResolvedModelRef> {
    if let Some(model) = override_ref {
        return resolve_model_ref(config, model);
    }
    if let Some(model) = store.latest_attempt_model(&session.id)? {
        return resolve_model_ref(config, &model);
    }
    resolve_session_model(store, config, session)
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
    let model_info = resolve_model_info(config, &resolved.request.model);
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
    let model = status_model(&resolved.request.model);
    let context_usage = context_usage(store, resolved.attached_session.as_ref(), &model_info);

    Ok(StatusReport {
        model,
        session_id: resolved
            .attached_session
            .as_ref()
            .map(|session| session.id.clone()),
        context_percent: context_usage.map(|(percent, _)| percent),
        context_usage_source: context_usage.map(|(_, source)| source),
        project_root: project.map(|project| project.root.display().to_string()),
        context_window: model_info.context_window,
        supported_effort_levels: model_info.supported_effort_levels,
        git: includes.git.then(|| project.map(git_status)).flatten(),
        session: session_summary.map(status_session),
        active,
        clean,
        compaction,
        available_models: includes.models.then(|| available_models(config)),
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

fn context_usage(
    store: &Store,
    session: Option<&Session>,
    model_info: &ResolvedModelInfo,
) -> Option<(f64, ContextUsageSource)> {
    let session = session?;
    let context_window = model_info.context_window?;
    let (tokens, source) = if session.last_context_tokens > 0 {
        (session.last_context_tokens, ContextUsageSource::Reported)
    } else {
        (
            store.estimate_context_tokens(&session.id),
            ContextUsageSource::Estimated,
        )
    };
    Some(((tokens as f64 / context_window as f64) * 100.0, source))
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
        last_context_tokens: summary.last_context_tokens,
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
            line_wrapping: true,
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    fn finish_attempt(store: &Store, session_id: &str, model: &str, outcome: &str) {
        let attempt = store.start_turn_attempt(session_id, "turn", model).unwrap();
        store
            .finish_turn_attempt(
                attempt,
                crate::store::TurnAttemptCompletion {
                    outcome,
                    error_class: None,
                    error: None,
                    partial_output: None,
                    provider_request_count: 1,
                    iteration_count: 1,
                    retry_count: 0,
                    duration_ms: 1,
                    context_tokens: 1,
                },
            )
            .unwrap();
    }

    #[test]
    fn new_scope_uses_first_configured_model() {
        let store = Store::open_memory().unwrap();

        let resolved =
            resolve_invocation(&store, &test_config(), &InvocationOverrides::default()).unwrap();

        assert_eq!(resolved.request.model.canonical, "alpha/default-model");
    }

    #[test]
    fn explicit_model_override_wins_for_new_session() {
        let store = Store::open_memory().unwrap();

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
    }

    #[test]
    fn completed_attempts_supply_session_and_scope_models() {
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
        assert_eq!(attached.request.model.canonical, "alpha/default-model:high");

        let empty = resolve_invocation(
            &store,
            &test_config(),
            &InvocationOverrides {
                session: Some(empty.id),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(empty.request.model.canonical, "alpha/default-model:low");
    }

    #[test]
    fn retry_prefers_latest_attempt_but_normal_turn_uses_latest_completed() {
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

        assert_eq!(normal.canonical, "alpha/default-model:high");
        assert_eq!(retry.canonical, "alpha/default-model:low");
        assert_eq!(overridden.canonical, "alpha/default-model");
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
                continue_latest: false,
                model: None,
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
                session: Some(session.id),
                continue_latest: false,
                model: None,
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
        let session = store
            .create_session_seeded("/tmp", "system prompt", "[environment]\ncurrent cwd")
            .unwrap();
        let overrides = InvocationOverrides {
            session: Some(session.id.clone()),
            continue_latest: false,
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
            .update_session(
                &session.id,
                &crate::provider::Usage::default(),
                25,
                None,
                "alpha/default-model",
            )
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
        assert_eq!(reported.context_percent, Some(25.0));
        assert_eq!(
            reported.context_usage_source,
            Some(ContextUsageSource::Reported)
        );

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
        assert_ne!(estimated.context_percent, Some(25.0));
    }

    #[test]
    fn status_report_only_builds_requested_session_details() {
        let store = Store::open_memory().unwrap();
        let session = store.create_session("/tmp").unwrap();
        let overrides = InvocationOverrides {
            session: Some(session.id),
            continue_latest: false,
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
