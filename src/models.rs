use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelRef {
    pub canonical: String,
    pub provider_id: String,
    pub model_id: String,
    pub effort: Option<String>,
    pub preserved_thinking: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOptions {
    pub model: ResolvedModelRef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedModelInfo {
    pub context_window: Option<u64>,
    pub supported_effort_levels: Vec<String>,
    pub preserved_thinking: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AvailableModelsPayload {
    pub providers: Vec<AvailableProvider>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AvailableProvider {
    pub id: String,
    pub models: Vec<AvailableModel>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AvailableModel {
    pub id: String,
    pub model_id: String,
    pub supported_efforts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    pub preserved_thinking: bool,
}

pub fn validate_config(config: &Config) -> Result<()> {
    first_model_ref(config)?;
    if let Some(review_model) = config.guardrail.review_model.as_deref() {
        resolve_model_ref(config, review_model)
            .with_context(|| "invalid `guardrail.review_model` in config.jsonc")?;
    }
    Ok(())
}

pub fn first_model_ref(config: &Config) -> Result<ResolvedModelRef> {
    for (provider_id, provider) in config.providers.iter() {
        if let Some((model_id, _)) = provider.models.iter().next() {
            return resolve_exact_model(config, provider_id, model_id, None);
        }
    }
    bail!("no models configured in config.jsonc")
}

pub fn resolve_model_ref(config: &Config, raw: &str) -> Result<ResolvedModelRef> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty model reference");
    }

    if let Some(resolved) = try_resolve_model(config, raw, None)? {
        return Ok(resolved);
    }

    if let Some((base, effort)) = raw.rsplit_once(':')
        && !base.is_empty()
        && !effort.is_empty()
        && let Some(resolved) = try_resolve_model(config, base, Some(effort.to_string()))?
    {
        return Ok(resolved);
    }

    if let Some((provider_id, model_id)) = explicit_provider(config, raw) {
        resolve_exact_model(config, provider_id, model_id, None)
    } else {
        resolve_implicit_model(config, raw, None)
    }
}

pub fn resolve_model_info(config: &Config, model: &ResolvedModelRef) -> ResolvedModelInfo {
    let cfg = config.model_config(&model.provider_id, &model.model_id);
    ResolvedModelInfo {
        context_window: cfg.and_then(|item| item.context_window),
        supported_effort_levels: cfg
            .and_then(|item| item.supported_efforts.clone())
            .unwrap_or_default(),
        preserved_thinking: model.preserved_thinking,
    }
}

pub fn available_models(config: &Config) -> AvailableModelsPayload {
    let providers = config
        .providers
        .iter()
        .map(|(provider_id, provider)| {
            let models = provider
                .models
                .iter()
                .map(|(model_id, model)| AvailableModel {
                    id: canonical_base(provider_id, model_id),
                    model_id: model_id.clone(),
                    supported_efforts: model.supported_efforts.clone().unwrap_or_default(),
                    context_window: model.context_window,
                    preserved_thinking: should_preserve_thinking(model_id, model),
                })
                .collect::<Vec<_>>();
            AvailableProvider {
                id: provider_id.clone(),
                models,
            }
        })
        .collect::<Vec<_>>();
    AvailableModelsPayload { providers }
}

fn explicit_provider<'a>(config: &'a Config, base: &'a str) -> Option<(&'a str, &'a str)> {
    let (provider_id, model_id) = base.split_once('/')?;
    config
        .providers
        .contains_key(provider_id)
        .then_some((provider_id, model_id))
}

fn try_resolve_model(
    config: &Config,
    raw: &str,
    effort: Option<String>,
) -> Result<Option<ResolvedModelRef>> {
    if let Some((provider_id, model_id)) = explicit_provider(config, raw) {
        return config
            .model_config(provider_id, model_id)
            .map(|_| resolve_exact_model(config, provider_id, model_id, effort))
            .transpose();
    }

    let matches = config
        .providers
        .iter()
        .filter(|(_, provider)| provider.models.contains_key(raw))
        .map(|(provider_id, _)| provider_id.as_str())
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => Ok(None),
        [provider_id] => resolve_exact_model(config, provider_id, raw, effort).map(Some),
        _ => bail!(
            "ambiguous model `{raw}`; use one of: {}",
            matches
                .iter()
                .map(|provider_id| canonical_base(provider_id, raw))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn resolve_exact_model(
    config: &Config,
    provider_id: &str,
    model_id: &str,
    effort: Option<String>,
) -> Result<ResolvedModelRef> {
    if model_id.trim().is_empty() {
        bail!("model reference `{provider_id}/` is missing a model id");
    }
    let model_config = config
        .model_config(provider_id, model_id)
        .with_context(|| format!("model not configured: {provider_id}/{model_id}"))?;

    let resolved = ResolvedModelRef {
        canonical: canonical_ref(provider_id, model_id, effort.as_deref()),
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        effort,
        preserved_thinking: should_preserve_thinking(model_id, model_config),
    };
    Ok(resolved)
}

fn should_preserve_thinking(model_id: &str, model: &crate::config::ModelConfig) -> bool {
    model.preserved_thinking.unwrap_or_else(|| {
        let model_id = model_id.to_ascii_lowercase();
        model_id.contains("deepseek") || model_id.contains("glm")
    })
}

fn resolve_implicit_model(
    config: &Config,
    model_id: &str,
    effort: Option<String>,
) -> Result<ResolvedModelRef> {
    let matches = config
        .providers
        .iter()
        .filter(|(_, provider)| provider.models.contains_key(model_id))
        .map(|(provider_id, _)| provider_id.as_str())
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => bail!("model not configured: {model_id}"),
        [provider_id] => resolve_exact_model(config, provider_id, model_id, effort),
        _ => bail!(
            "ambiguous model `{model_id}`; use one of: {}",
            matches
                .iter()
                .map(|provider_id| canonical_base(provider_id, model_id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn canonical_ref(provider_id: &str, model_id: &str, effort: Option<&str>) -> String {
    let base = canonical_base(provider_id, model_id);
    match effort {
        Some(level) => format!("{base}:{level}"),
        None => base,
    }
}

fn canonical_base(provider_id: &str, model_id: &str) -> String {
    format!("{provider_id}/{model_id}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::{
        CircuitBreakerConfig, CompactionConfig, GuardrailConfig, LimitsConfig, ModelConfig,
        OrderedMap, ProviderConfig, RedactionConfig, TerminalBellConfig,
    };

    fn test_config() -> Config {
        Config {
            providers: OrderedMap::from_iter([
                (
                    "alpha".into(),
                    ProviderConfig {
                        base_url: "https://alpha.test/v1".into(),
                        api_key_env: "ALPHA_KEY".into(),
                        models: OrderedMap::from_iter([
                            (
                                "common-model".into(),
                                ModelConfig {
                                    context_window: Some(100),
                                    supported_efforts: Some(vec![
                                        "low".into(),
                                        "medium".into(),
                                        "high".into(),
                                    ]),
                                    preserved_thinking: None,
                                },
                            ),
                            (
                                "nested/model".into(),
                                ModelConfig {
                                    context_window: Some(200),
                                    supported_efforts: None,
                                    preserved_thinking: None,
                                },
                            ),
                            (
                                "version:latest".into(),
                                ModelConfig {
                                    context_window: Some(200),
                                    supported_efforts: None,
                                    preserved_thinking: None,
                                },
                            ),
                            (
                                "DeepSeek-V4".into(),
                                ModelConfig {
                                    context_window: Some(300),
                                    supported_efforts: None,
                                    preserved_thinking: None,
                                },
                            ),
                            (
                                "GLM-5".into(),
                                ModelConfig {
                                    context_window: Some(300),
                                    supported_efforts: None,
                                    preserved_thinking: None,
                                },
                            ),
                            (
                                "replay-override".into(),
                                ModelConfig {
                                    context_window: None,
                                    supported_efforts: None,
                                    preserved_thinking: Some(true),
                                },
                            ),
                            (
                                "deepseek-disabled".into(),
                                ModelConfig {
                                    context_window: None,
                                    supported_efforts: None,
                                    preserved_thinking: Some(false),
                                },
                            ),
                        ]),
                    },
                ),
                (
                    "beta".into(),
                    ProviderConfig {
                        base_url: "https://beta.test/v1".into(),
                        api_key_env: "BETA_KEY".into(),
                        models: OrderedMap::from_iter([(
                            "common-model".into(),
                            ModelConfig {
                                context_window: Some(300),
                                supported_efforts: Some(vec!["max".into()]),
                                preserved_thinking: None,
                            },
                        )]),
                    },
                ),
            ]),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig {
                enabled: false,
                review_model: Some("alpha/common-model".into()),
                timeout_ms: 90_000,
                circuit_breaker: CircuitBreakerConfig::default(),
            },
            terminal_bell: TerminalBellConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    #[test]
    fn resolves_full_model_ref() {
        let resolved = resolve_model_ref(&test_config(), "alpha/common-model:high").unwrap();
        assert_eq!(resolved.provider_id, "alpha");
        assert_eq!(resolved.model_id, "common-model");
        assert_eq!(resolved.effort.as_deref(), Some("high"));
        assert_eq!(resolved.canonical, "alpha/common-model:high");
        assert!(!resolved.preserved_thinking);
    }

    #[test]
    fn resolves_arbitrary_effort_without_allowlist_validation() {
        let resolved =
            resolve_model_ref(&test_config(), "alpha/common-model:provider-custom").unwrap();
        assert_eq!(resolved.model_id, "common-model");
        assert_eq!(resolved.effort.as_deref(), Some("provider-custom"));
        assert_eq!(resolved.canonical, "alpha/common-model:provider-custom");

        let unlisted = resolve_model_ref(&test_config(), "alpha/nested/model:none").unwrap();
        assert_eq!(unlisted.effort.as_deref(), Some("none"));
    }

    #[test]
    fn exact_model_id_takes_precedence_over_effort_suffix() {
        let exact = resolve_model_ref(&test_config(), "alpha/version:latest").unwrap();
        assert_eq!(exact.model_id, "version:latest");
        assert_eq!(exact.effort, None);

        let with_effort = resolve_model_ref(&test_config(), "alpha/version:latest:max").unwrap();
        assert_eq!(with_effort.model_id, "version:latest");
        assert_eq!(with_effort.effort.as_deref(), Some("max"));
    }

    #[test]
    fn reasoning_replay_defaults_to_deepseek_and_honors_overrides() {
        let config = test_config();
        assert!(
            resolve_model_ref(&config, "alpha/DeepSeek-V4")
                .unwrap()
                .preserved_thinking
        );
        assert!(
            resolve_model_ref(&config, "alpha/GLM-5")
                .unwrap()
                .preserved_thinking
        );
        assert!(
            resolve_model_ref(&config, "alpha/replay-override")
                .unwrap()
                .preserved_thinking
        );
        assert!(
            !resolve_model_ref(&config, "alpha/deepseek-disabled")
                .unwrap()
                .preserved_thinking
        );
    }

    #[test]
    fn bare_model_errors_when_ambiguous() {
        let err = resolve_model_ref(&test_config(), "common-model").unwrap_err();
        assert!(err.to_string().contains("ambiguous model"));
    }

    #[test]
    fn validate_config_checks_first_and_review_models() {
        validate_config(&test_config()).unwrap();

        let mut invalid = test_config();
        invalid.guardrail.review_model = Some("alpha/missing".into());
        assert!(validate_config(&invalid).is_err());
    }

    #[test]
    fn first_model_uses_configured_order() {
        let resolved = first_model_ref(&test_config()).unwrap();
        assert_eq!(resolved.canonical, "alpha/common-model");
    }

    #[test]
    fn first_model_skips_empty_providers() {
        let mut config = test_config();
        config.providers = OrderedMap::from_iter([
            (
                "empty".into(),
                ProviderConfig {
                    base_url: "https://empty.test/v1".into(),
                    api_key_env: "EMPTY_KEY".into(),
                    models: OrderedMap::default(),
                },
            ),
            (
                "alpha".into(),
                ProviderConfig {
                    base_url: "https://alpha.test/v1".into(),
                    api_key_env: "ALPHA_KEY".into(),
                    models: OrderedMap::from_iter([(
                        "first-real".into(),
                        ModelConfig {
                            context_window: None,
                            supported_efforts: None,
                            preserved_thinking: None,
                        },
                    )]),
                },
            ),
        ]);

        let resolved = first_model_ref(&config).unwrap();
        assert_eq!(resolved.canonical, "alpha/first-real");
    }

    #[test]
    fn available_models_uses_configured_order() {
        let payload = available_models(&test_config());
        assert_eq!(payload.providers[0].id, "alpha");
        assert_eq!(payload.providers[1].id, "beta");
        assert_eq!(payload.providers[0].models[0].id, "alpha/common-model");
        assert_eq!(payload.providers[0].models[1].id, "alpha/nested/model");
    }
}
