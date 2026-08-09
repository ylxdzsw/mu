use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelRef {
    pub canonical: String,
    pub provider_id: String,
    pub model_id: String,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelChoice {
    candidates: Vec<ResolvedModelRef>,
    active: usize,
    floating: bool,
}

impl ResolvedModelChoice {
    pub fn fixed(model: ResolvedModelRef) -> Self {
        Self {
            candidates: vec![model],
            active: 0,
            floating: false,
        }
    }

    pub fn active_model(&self) -> &ResolvedModelRef {
        &self.candidates[self.active]
    }

    pub fn is_floating(&self) -> bool {
        self.floating
    }

    pub fn reset(&mut self) {
        self.active = 0;
    }

    #[cfg(test)]
    pub fn resume_from(&mut self, previous: &ResolvedModelChoice) -> bool {
        if !previous.floating {
            return false;
        }
        self.resume_provider(
            &previous.active_model().model_id,
            &previous.active_model().provider_id,
        )
    }

    pub fn resume_provider(&mut self, model_id: &str, provider_id: &str) -> bool {
        if !self.floating || self.active_model().model_id != model_id {
            return false;
        }
        let Some(active) = self
            .candidates
            .iter()
            .position(|candidate| candidate.provider_id == provider_id)
        else {
            return false;
        };
        self.active = active;
        true
    }

    pub fn advance(&mut self) -> bool {
        if !self.floating || self.active + 1 >= self.candidates.len() {
            return false;
        }
        self.active += 1;
        true
    }

    #[cfg(test)]
    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOptions {
    pub model: ResolvedModelRef,
    pub cache_key: Option<String>,
}

impl RequestOptions {
    pub fn for_session(model: ResolvedModelRef, session_id: &str, purpose: &str) -> Self {
        Self {
            model,
            cache_key: Some(format!("mu:{session_id}:{purpose}")),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedModelInfo {
    pub context_window: Option<u64>,
    pub supported_effort_levels: Vec<String>,
    pub replay_key: String,
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
    pub replay_key: String,
}

pub fn validate_config(config: &Config) -> Result<()> {
    first_model_ref(config)?;
    if let Some(review_model) = config.guardrail.review_model.as_deref() {
        resolve_model_choice(config, review_model)
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

pub fn first_model_choice(config: &Config) -> Result<ResolvedModelChoice> {
    first_model_ref(config).map(ResolvedModelChoice::fixed)
}

#[cfg(test)]
pub fn resolve_model_ref(config: &Config, raw: &str) -> Result<ResolvedModelRef> {
    resolve_model_choice(config, raw).map(|choice| choice.active_model().clone())
}

pub fn resolve_model_choice(config: &Config, raw: &str) -> Result<ResolvedModelChoice> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty model reference");
    }

    if let Some((provider_id, rest)) = parenthesized_provider(raw) {
        let (model_id, effort) = split_floating_model(config, rest);
        return resolve_floating_model(config, model_id, effort, Some(provider_id));
    }

    if let Some((provider_id, model_id)) = explicit_provider(config, raw) {
        let (model_id, effort) = split_exact_model(config, provider_id, model_id);
        return resolve_exact_model(config, provider_id, model_id, effort)
            .map(ResolvedModelChoice::fixed);
    }

    if raw.contains('/') {
        bail!("model not configured: {raw}");
    }

    let (model_id, effort) = split_floating_model(config, raw);
    if config
        .providers
        .iter()
        .any(|(_, provider)| provider.models.contains_key(model_id))
    {
        return resolve_floating_model(config, model_id, effort, None);
    }

    bail!("model not configured: {model_id}")
}

pub fn resolve_model_info(config: &Config, model: &ResolvedModelRef) -> ResolvedModelInfo {
    let cfg = config.model_config(&model.provider_id, &model.model_id);
    ResolvedModelInfo {
        context_window: cfg.and_then(|item| item.context_window),
        supported_effort_levels: cfg
            .and_then(|item| item.supported_efforts.clone())
            .unwrap_or_default(),
        replay_key: config.replay_key(&model.provider_id, &model.model_id),
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
                    replay_key: config.replay_key(provider_id, model_id),
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

fn parenthesized_provider(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.strip_prefix('(')?;
    let (provider_id, rest) = rest.split_once(")/")?;
    (!provider_id.is_empty() && !rest.is_empty()).then_some((provider_id, rest))
}

fn split_exact_model<'a>(
    config: &Config,
    provider_id: &str,
    raw: &'a str,
) -> (&'a str, Option<String>) {
    if config.model_config(provider_id, raw).is_some() {
        return (raw, None);
    }
    raw.rsplit_once(':')
        .filter(|(model_id, effort)| {
            !model_id.is_empty()
                && !effort.is_empty()
                && config.model_config(provider_id, model_id).is_some()
        })
        .map_or((raw, None), |(model_id, effort)| {
            (model_id, Some(effort.to_string()))
        })
}

fn split_floating_model<'a>(config: &Config, raw: &'a str) -> (&'a str, Option<String>) {
    if config
        .providers
        .iter()
        .any(|(_, provider)| provider.models.contains_key(raw))
    {
        return (raw, None);
    }
    raw.rsplit_once(':')
        .filter(|(model_id, effort)| {
            !model_id.is_empty()
                && !effort.is_empty()
                && config
                    .providers
                    .iter()
                    .any(|(_, provider)| provider.models.contains_key(model_id))
        })
        .map_or((raw, None), |(model_id, effort)| {
            (model_id, Some(effort.to_string()))
        })
}

fn resolve_floating_model(
    config: &Config,
    model_id: &str,
    effort: Option<String>,
    preferred_provider: Option<&str>,
) -> Result<ResolvedModelChoice> {
    let candidates = config
        .providers
        .iter()
        .filter(|(_, provider)| provider.models.contains_key(model_id))
        .map(|(provider_id, _)| resolve_model(config, provider_id, model_id, effort.clone(), true))
        .collect::<Result<Vec<_>>>()?;
    if candidates.is_empty() {
        bail!("model not configured: {model_id}");
    }
    let active = preferred_provider
        .and_then(|provider_id| {
            candidates
                .iter()
                .position(|candidate| candidate.provider_id == provider_id)
        })
        .unwrap_or(0);
    Ok(ResolvedModelChoice {
        candidates,
        active,
        floating: true,
    })
}

fn resolve_exact_model(
    config: &Config,
    provider_id: &str,
    model_id: &str,
    effort: Option<String>,
) -> Result<ResolvedModelRef> {
    resolve_model(config, provider_id, model_id, effort, false)
}

fn resolve_model(
    config: &Config,
    provider_id: &str,
    model_id: &str,
    effort: Option<String>,
    floating: bool,
) -> Result<ResolvedModelRef> {
    if model_id.trim().is_empty() {
        bail!("model reference `{provider_id}/` is missing a model id");
    }
    let _model_config = config
        .model_config(provider_id, model_id)
        .with_context(|| format!("model not configured: {provider_id}/{model_id}"))?;

    Ok(ResolvedModelRef {
        canonical: canonical_ref(provider_id, model_id, effort.as_deref(), floating),
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        effort,
    })
}

fn canonical_ref(
    provider_id: &str,
    model_id: &str,
    effort: Option<&str>,
    floating: bool,
) -> String {
    let base = if floating {
        format!("({provider_id})/{model_id}")
    } else {
        canonical_base(provider_id, model_id)
    };
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
        CompactionConfig, GuardrailConfig, LimitsConfig, ModelConfig, OrderedMap, ProviderConfig,
        RedactionConfig, TerminalBellConfig,
    };

    fn test_config() -> Config {
        Config {
            providers: OrderedMap::from_iter([
                (
                    "alpha".into(),
                    ProviderConfig {
                        endpoint: "https://alpha.test/v1/chat/completions".into(),
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
                                    replay_key: None,
                                },
                            ),
                            (
                                "nested/model".into(),
                                ModelConfig {
                                    context_window: Some(200),
                                    supported_efforts: None,
                                    replay_key: None,
                                },
                            ),
                            (
                                "version:latest".into(),
                                ModelConfig {
                                    context_window: Some(200),
                                    supported_efforts: None,
                                    replay_key: None,
                                },
                            ),
                            (
                                "DeepSeek-V4".into(),
                                ModelConfig {
                                    context_window: Some(300),
                                    supported_efforts: None,
                                    replay_key: None,
                                },
                            ),
                            (
                                "GLM-5".into(),
                                ModelConfig {
                                    context_window: Some(300),
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
                        endpoint: "https://beta.test/v1/responses".into(),
                        api_key_env: "BETA_KEY".into(),
                        models: OrderedMap::from_iter([(
                            "common-model".into(),
                            ModelConfig {
                                context_window: Some(300),
                                supported_efforts: Some(vec!["max".into()]),
                                replay_key: None,
                            },
                        )]),
                    },
                ),
            ]),
            output: Default::default(),
            line_wrapping: true,
            auto_resume: false,
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig {
                enabled: false,
                review_model: Some("alpha/common-model".into()),
                timeout_seconds: 120,
                max_denials_per_turn: 3,
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
    fn bare_model_floats_in_provider_order_without_effort_filtering() {
        let mut choice =
            resolve_model_choice(&test_config(), "common-model:provider-custom").unwrap();

        assert!(choice.is_floating());
        assert_eq!(choice.candidate_count(), 2);
        assert_eq!(
            choice.active_model().canonical,
            "(alpha)/common-model:provider-custom"
        );
        assert!(choice.advance());
        assert_eq!(
            choice.active_model().canonical,
            "(beta)/common-model:provider-custom"
        );
        assert!(!choice.advance());
    }

    #[test]
    fn floating_resume_uses_model_as_key_and_ignores_effort() {
        let mut previous = resolve_model_choice(&test_config(), "common-model:low").unwrap();
        assert!(previous.advance());
        let mut changed_effort = resolve_model_choice(&test_config(), "common-model:max").unwrap();
        let mut changed_model = resolve_model_choice(&test_config(), "DeepSeek-V4:max").unwrap();

        assert!(changed_effort.resume_from(&previous));
        assert_eq!(changed_effort.active_model().provider_id, "beta");
        assert!(!changed_model.resume_from(&previous));
        assert_eq!(changed_model.active_model().provider_id, "alpha");
    }

    #[test]
    fn parenthesized_choice_restores_or_resets_its_provider() {
        let restored = resolve_model_choice(&test_config(), "(beta)/common-model:high").unwrap();
        assert!(restored.is_floating());
        assert_eq!(restored.active_model().provider_id, "beta");

        let reset = resolve_model_choice(&test_config(), "(removed)/common-model:high").unwrap();
        assert_eq!(reset.active_model().provider_id, "alpha");
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
                    endpoint: "https://empty.test/chat/completions".into(),
                    api_key_env: "EMPTY_KEY".into(),
                    models: OrderedMap::default(),
                },
            ),
            (
                "alpha".into(),
                ProviderConfig {
                    endpoint: "https://alpha.test/chat/completions".into(),
                    api_key_env: "ALPHA_KEY".into(),
                    models: OrderedMap::from_iter([(
                        "first-real".into(),
                        ModelConfig {
                            context_window: None,
                            supported_efforts: None,
                            replay_key: None,
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
