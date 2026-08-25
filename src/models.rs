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
    first_model_choice(config)?;
    Ok(())
}

pub fn first_model_choice(config: &Config) -> Result<ResolvedModelChoice> {
    for (provider_id, provider) in config.providers.iter() {
        if let Some((model_id, _)) = provider.models.iter().next() {
            return resolve_model(config, provider_id, model_id, None, false)
                .map(ResolvedModelChoice::fixed);
        }
    }
    bail!("no models configured in config.jsonc")
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
        let (model_id, effort) = split_model_effort(rest);
        return resolve_floating_model(config, model_id, effort, Some(provider_id));
    }

    if let Some((provider_id, model_id)) = explicit_provider(config, raw) {
        let (model_id, effort) = split_model_effort(model_id);
        return resolve_model(config, provider_id, model_id, effort, false)
            .map(ResolvedModelChoice::fixed);
    }

    if raw.contains('/') {
        bail!("model not configured: {raw}");
    }

    let (model_id, effort) = split_model_effort(raw);
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

fn split_model_effort(raw: &str) -> (&str, Option<String>) {
    raw.split_once(':')
        .filter(|(model_id, effort)| !model_id.is_empty() && !effort.is_empty())
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
        CompactionConfig, LimitsConfig, ModelConfig, OrderedMap, ProviderConfig, RedactionConfig,
        TerminalBellConfig,
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
                                "DeepSeek-V4".into(),
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
            trap: crate::bash::TrapLevel::Off,
            auto_resume: false,
            soft_interrupt: crate::config::bundled_test_default("/soft_interrupt"),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
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
    fn bare_model_floats_in_provider_order_without_effort_filtering() {
        let mut choice =
            resolve_model_choice(&test_config(), "common-model:provider-custom").unwrap();

        assert!(choice.is_floating());
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
    fn parenthesized_choice_restores_or_resets_its_provider() {
        let restored = resolve_model_choice(&test_config(), "(beta)/common-model:high").unwrap();
        assert!(restored.is_floating());
        assert_eq!(restored.active_model().provider_id, "beta");

        let reset = resolve_model_choice(&test_config(), "(removed)/common-model:high").unwrap();
        assert_eq!(reset.active_model().provider_id, "alpha");
    }

    #[test]
    fn first_model_skips_empty_providers_and_uses_configured_order() {
        let mut config = test_config();
        let providers = std::iter::once((
            "empty".into(),
            ProviderConfig {
                endpoint: "https://empty.test/chat/completions".into(),
                api_key_env: "EMPTY_KEY".into(),
                models: OrderedMap::default(),
            },
        ))
        .chain(
            config
                .providers
                .iter()
                .map(|(id, provider)| (id.clone(), provider.clone())),
        )
        .collect();
        config.providers = providers;

        let choice = first_model_choice(&config).unwrap();
        let resolved = choice.active_model();
        assert_eq!(resolved.canonical, "alpha/common-model");
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
