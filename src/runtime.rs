use anyhow::{Result, bail};
use serde::Serialize;
use std::process::Command;

use crate::config::Config;
use crate::models::{
    EffortLevel, ModelCatalog, ModelMetadataSource, RequestOptions, ResolvedModelInfo,
    SupportedEffortSource, validate_effort_support,
};
use crate::store::{Session, Store};

#[derive(Debug, Clone, Default)]
pub struct InvocationOverrides {
    pub session: Option<String>,
    pub continue_latest: bool,
    pub model: Option<String>,
    pub effort: Option<EffortLevel>,
}

#[derive(Debug, Clone)]
pub struct ResolvedInvocation {
    pub attached_session: Option<Session>,
    pub request: RequestOptions,
    pub session_seed: RequestOptions,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    pub model_id: String,
    pub effort: Option<EffortLevel>,
    pub session_id: Option<String>,
    pub context_percent: Option<f64>,
    pub project_root: Option<String>,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub reasoning: Option<bool>,
    pub model_metadata_source: ModelMetadataSource,
    pub supported_effort_levels_source: SupportedEffortSource,
    pub supported_effort_levels: Vec<EffortLevel>,
    pub git: Option<GitStatus>,
    pub session: Option<StatusSession>,
    pub active: StatusActiveTurn,
    pub compaction: Option<CompactionStatus>,
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
    pub cost_total: f64,
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
                .ok_or_else(|| anyhow::anyhow!("session not found in active scope: {id}"))?,
        )
    } else if overrides.continue_latest {
        store.latest_session()?
    } else {
        None
    };

    if let Some(session) = attached_session.clone() {
        return Ok(ResolvedInvocation {
            attached_session: Some(session.clone()),
            request: RequestOptions {
                model: overrides.model.clone().unwrap_or(session.model.clone()),
                effort: overrides
                    .effort
                    .or(session.effort)
                    .or(config.default_effort),
            },
            session_seed: RequestOptions {
                model: session.model,
                effort: session.effort.or(config.default_effort),
            },
        });
    }

    let has_explicit_overrides = overrides.model.is_some() || overrides.effort.is_some();
    let latest_scope_session = if has_explicit_overrides {
        None
    } else {
        store.latest_session()?
    };
    let session_seed = if let Some(session) = latest_scope_session {
        RequestOptions {
            model: session.model,
            effort: session.effort,
        }
    } else {
        RequestOptions {
            model: config.default_model.clone(),
            effort: config.default_effort,
        }
    };
    let request = RequestOptions {
        model: overrides
            .model
            .clone()
            .unwrap_or_else(|| session_seed.model.clone()),
        effort: overrides.effort.or(session_seed.effort),
    };

    Ok(ResolvedInvocation {
        attached_session: None,
        request,
        session_seed,
    })
}

pub fn build_status_report(
    store: &Store,
    config: &Config,
    overrides: &InvocationOverrides,
    catalog: Option<&ModelCatalog>,
    project: Option<&crate::paths::Project>,
) -> Result<StatusReport> {
    let resolved = resolve_invocation(store, config, overrides)?;
    let model_info = crate::models::resolve_model_info(config, catalog, &resolved.request.model);
    validate_effort_support(
        &resolved.request.model,
        resolved.request.effort.as_ref(),
        &model_info,
    )?;
    let session_summary = resolved
        .attached_session
        .as_ref()
        .map(|session| store.session_summary(&session.id))
        .transpose()?
        .flatten();
    let active = resolved
        .attached_session
        .as_ref()
        .is_some_and(|session| crate::store::is_session_busy(&session.id));
    let compaction = resolved
        .attached_session
        .as_ref()
        .map(|session| {
            store
                .latest_summary_sequence(&session.id)
                .map(|latest_summary_seq| CompactionStatus { latest_summary_seq })
        })
        .transpose()?;

    Ok(StatusReport {
        model_id: resolved.request.model.clone(),
        effort: resolved.request.effort,
        session_id: resolved
            .attached_session
            .as_ref()
            .map(|session| session.id.clone()),
        context_percent: context_percent(store, resolved.attached_session.as_ref(), &model_info),
        project_root: project.map(|project| project.root.display().to_string()),
        context_window: model_info.context_window,
        max_output_tokens: model_info.max_output_tokens,
        reasoning: model_info.reasoning,
        model_metadata_source: model_info.metadata_source,
        supported_effort_levels_source: model_info.supported_effort_source,
        supported_effort_levels: model_info.supported_effort_levels,
        git: project.map(git_status),
        session: session_summary.map(status_session),
        active: StatusActiveTurn { busy: active },
        compaction,
    })
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
        cost_total: summary.cost_total,
    }
}

fn git_status(project: &crate::paths::Project) -> GitStatus {
    GitStatus {
        branch: git_branch(&project.root),
        dirty: git_dirty(&project.root),
        git_dir: project
            .worktree
            .as_ref()
            .map(|worktree| worktree.git_dir.display().to_string()),
        common_dir: project
            .worktree
            .as_ref()
            .and_then(|worktree| worktree.common_dir.as_ref())
            .map(|path| path.display().to_string()),
    }
}

fn git_branch(root: &std::path::Path) -> Option<String> {
    let branch = git_output(root, &["branch", "--show-current"])?;
    if !branch.is_empty() {
        return Some(branch);
    }
    git_output(root, &["rev-parse", "--short", "HEAD"]).filter(|value| !value.is_empty())
}

fn git_dirty(root: &std::path::Path) -> Option<bool> {
    git_output(root, &["status", "--porcelain=v1"]).map(|output| !output.is_empty())
}

fn git_output(root: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::HashMap;

    use super::*;
    use crate::config::{
        CompactionConfig, Config, GuardrailConfig, LimitsConfig, ModelConfig, ProviderConfig,
        RedactionConfig,
    };
    use crate::models::{
        CachedMetadataSource, CachedMetadataSourceKind, CachedModel, CachedProviderInfo,
        EffortLevelsSource,
    };
    use crate::paths::{Project, ProjectMarker};

    fn temp_store() -> (Store, std::path::PathBuf) {
        let tmp = std::env::temp_dir().join(format!("mu-runtime-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        (Store::open(&tmp.join("mu.db")).unwrap(), tmp)
    }

    fn test_config() -> Config {
        Config {
            provider: ProviderConfig {
                base_url: "https://example.test/v1".into(),
                api_key_env: "MU_TEST_KEY".into(),
            },
            default_model: "default-model".into(),
            default_effort: None,
            models: HashMap::from([(
                "default-model".into(),
                ModelConfig {
                    context_window: Some(100),
                    price_per_mtok: None,
                },
            )]),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            terminal_bell: crate::config::TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    fn cached_model(
        id: &str,
        reasoning: Option<bool>,
        levels: Vec<EffortLevel>,
        levels_source: EffortLevelsSource,
    ) -> CachedModel {
        CachedModel {
            id: id.into(),
            display_name: id.into(),
            context_window: Some(100),
            max_output_tokens: None,
            reasoning,
            reasoning_effort_levels: Some(levels),
            reasoning_effort_levels_source: levels_source,
            metadata_source: CachedMetadataSource {
                kind: CachedMetadataSourceKind::ProviderModelsEndpoint,
                provider_id: None,
                api_url: None,
            },
        }
    }

    fn catalog(models: impl IntoIterator<Item = (String, CachedModel)>) -> ModelCatalog {
        ModelCatalog {
            version: 1,
            fetched_at: "2026-06-26T00:00:00Z".into(),
            provider: CachedProviderInfo {
                base_url: "https://example.test/v1".into(),
            },
            models: BTreeMap::from_iter(models),
        }
    }

    #[test]
    fn explicit_flags_override_session_and_scope() {
        let (store, tmp) = temp_store();
        let config = test_config();
        let latest = store
            .create_session("/tmp", "scope-model", Some(EffortLevel::High))
            .unwrap();
        store
            .update_session(
                &latest.id,
                0,
                0.0,
                None,
                "scope-model",
                Some(EffortLevel::High),
            )
            .unwrap();
        let attached = store
            .create_session("/tmp", "session-model", Some(EffortLevel::Low))
            .unwrap();

        let resolved = resolve_invocation(
            &store,
            &config,
            &InvocationOverrides {
                session: Some(attached.id.clone()),
                continue_latest: false,
                model: Some("flag-model".into()),
                effort: Some(EffortLevel::Max),
            },
        )
        .unwrap();

        assert_eq!(resolved.request.model, "flag-model");
        assert_eq!(resolved.request.effort, Some(EffortLevel::Max));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn attached_session_reuses_last_model_and_effort() {
        let (store, tmp) = temp_store();
        let config = test_config();
        let session = store
            .create_session("/tmp", "session-model", Some(EffortLevel::Medium))
            .unwrap();

        let resolved = resolve_invocation(
            &store,
            &config,
            &InvocationOverrides {
                session: Some(session.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(resolved.request.model, "session-model");
        assert_eq!(resolved.request.effort, Some(EffortLevel::Medium));
        assert!(resolved.attached_session.is_some());
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn new_session_inherits_from_latest_scope_when_no_flags_are_set() {
        let (store, tmp) = temp_store();
        let config = test_config();
        let latest = store
            .create_session("/tmp", "scope-model", Some(EffortLevel::High))
            .unwrap();

        let resolved =
            resolve_invocation(&store, &config, &InvocationOverrides::default()).unwrap();

        assert_eq!(resolved.request.model, latest.model);
        assert_eq!(resolved.request.effort, latest.effort);
        assert_eq!(resolved.session_seed.model, latest.model);
        assert!(resolved.attached_session.is_none());
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn explicit_new_session_flags_fall_back_to_default_seed_and_unset_effort() {
        let (store, tmp) = temp_store();
        let config = test_config();
        store
            .create_session("/tmp", "scope-model", Some(EffortLevel::High))
            .unwrap();

        let resolved = resolve_invocation(
            &store,
            &config,
            &InvocationOverrides {
                model: Some("flag-model".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(resolved.request.model, "flag-model");
        assert_eq!(resolved.request.effort, None);
        assert_eq!(resolved.session_seed.model, "default-model");
        assert_eq!(resolved.session_seed.effort, None);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn new_session_uses_config_default_effort_without_flags() {
        let (store, tmp) = temp_store();
        let mut config = test_config();
        config.default_effort = Some(EffortLevel::High);

        let resolved =
            resolve_invocation(&store, &config, &InvocationOverrides::default()).unwrap();

        assert_eq!(resolved.request.model, "default-model");
        assert_eq!(resolved.request.effort, Some(EffortLevel::High));
        assert_eq!(resolved.session_seed.effort, Some(EffortLevel::High));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn model_override_without_effort_uses_config_default_effort() {
        let (store, tmp) = temp_store();
        let mut config = test_config();
        config.default_effort = Some(EffortLevel::Medium);
        store
            .create_session("/tmp", "scope-model", Some(EffortLevel::High))
            .unwrap();

        let resolved = resolve_invocation(
            &store,
            &config,
            &InvocationOverrides {
                model: Some("flag-model".into()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(resolved.request.model, "flag-model");
        assert_eq!(resolved.request.effort, Some(EffortLevel::Medium));
        assert_eq!(resolved.session_seed.model, "default-model");
        assert_eq!(resolved.session_seed.effort, Some(EffortLevel::Medium));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn stored_session_effort_overrides_config_default_effort() {
        let (store, tmp) = temp_store();
        let mut config = test_config();
        config.default_effort = Some(EffortLevel::High);
        let session = store
            .create_session("/tmp", "session-model", Some(EffortLevel::Low))
            .unwrap();

        let resolved = resolve_invocation(
            &store,
            &config,
            &InvocationOverrides {
                session: Some(session.id.clone()),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(resolved.request.effort, Some(EffortLevel::Low));
        assert_eq!(resolved.session_seed.effort, Some(EffortLevel::Low));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn explicit_effort_overrides_config_default_effort() {
        let (store, tmp) = temp_store();
        let mut config = test_config();
        config.default_effort = Some(EffortLevel::Low);

        let resolved = resolve_invocation(
            &store,
            &config,
            &InvocationOverrides {
                effort: Some(EffortLevel::Max),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(resolved.request.effort, Some(EffortLevel::Max));
        assert_eq!(resolved.session_seed.effort, Some(EffortLevel::Low));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn status_is_valid_without_a_session() {
        let (store, tmp) = temp_store();
        let mut config = test_config();
        config.default_effort = Some(EffortLevel::Medium);

        let status =
            build_status_report(&store, &config, &InvocationOverrides::default(), None, None)
                .unwrap();

        assert_eq!(status.model_id, "default-model");
        assert_eq!(status.effort, Some(EffortLevel::Medium));
        assert!(status.session_id.is_none());
        assert!(status.context_percent.is_none());
        assert!(status.project_root.is_none());
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn status_reports_context_percent_for_existing_session() {
        let (store, tmp) = temp_store();
        let config = test_config();
        let session = store
            .create_session("/tmp", "default-model", Some(EffortLevel::Low))
            .unwrap();
        store
            .update_session(
                &session.id,
                25,
                0.0,
                None,
                "default-model",
                Some(EffortLevel::Low),
            )
            .unwrap();

        let status = build_status_report(
            &store,
            &config,
            &InvocationOverrides {
                session: Some(session.id.clone()),
                ..Default::default()
            },
            None,
            Some(&Project {
                root: std::path::PathBuf::from("/tmp/project"),
                marker: ProjectMarker::Git,
                worktree: None,
            }),
        )
        .unwrap();

        assert_eq!(status.context_percent, Some(25.0));
        assert_eq!(status.project_root.as_deref(), Some("/tmp/project"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn explicit_effort_is_rejected_for_known_non_reasoning_models() {
        let (store, tmp) = temp_store();
        let config = test_config();
        let catalog = catalog([(
            "text-only".into(),
            cached_model(
                "text-only",
                Some(false),
                vec![],
                EffortLevelsSource::Inferred,
            ),
        )]);

        let err = build_status_report(
            &store,
            &config,
            &InvocationOverrides {
                model: Some("text-only".into()),
                effort: Some(EffortLevel::Low),
                ..Default::default()
            },
            Some(&catalog),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("non-reasoning"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn explicit_effort_is_rejected_when_cached_levels_exclude_it() {
        let (store, tmp) = temp_store();
        let config = test_config();
        let catalog = catalog([(
            "reasoner".into(),
            cached_model(
                "reasoner",
                Some(true),
                vec![EffortLevel::Medium, EffortLevel::High],
                EffortLevelsSource::Explicit,
            ),
        )]);

        let err = build_status_report(
            &store,
            &config,
            &InvocationOverrides {
                model: Some("reasoner".into()),
                effort: Some(EffortLevel::Low),
                ..Default::default()
            },
            Some(&catalog),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("supported levels: medium, high"));
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn inferred_support_accepts_effort_when_metadata_is_missing() {
        let (store, tmp) = temp_store();
        let config = test_config();

        let status = build_status_report(
            &store,
            &config,
            &InvocationOverrides {
                model: Some("custom-model".into()),
                effort: Some(EffortLevel::Max),
                ..Default::default()
            },
            None,
            None,
        )
        .unwrap();

        assert_eq!(status.effort, Some(EffortLevel::Max));
        assert_eq!(
            status.model_metadata_source,
            ModelMetadataSource::FallbackInference
        );
        assert_eq!(
            status.supported_effort_levels,
            EffortLevel::canonical().to_vec()
        );
        let _ = std::fs::remove_dir_all(tmp);
    }
}
