use std::fmt;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::config::{Config, PriceConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum EffortLevel {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl EffortLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

impl fmt::Display for EffortLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EffortLevel {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            other => bail!("unsupported effort level `{other}`"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModelRef {
    pub canonical: String,
    pub provider_id: String,
    pub model_id: String,
    pub effort: Option<EffortLevel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestOptions {
    pub model: ResolvedModelRef,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedModelInfo {
    pub context_window: Option<u64>,
    pub supported_effort_levels: Vec<EffortLevel>,
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
    pub supported_efforts: Vec<EffortLevel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_per_mtok: Option<PriceConfig>,
}

pub fn validate_config(config: &Config) -> Result<()> {
    resolve_model_ref(config, &config.default_model)
        .with_context(|| "invalid `default_model` in config.jsonc")?;
    if let Some(review_model) = config.guardrail.review_model.as_deref() {
        resolve_model_ref(config, review_model)
            .with_context(|| "invalid `guardrail.review_model` in config.jsonc")?;
    }
    Ok(())
}

pub fn resolve_model_ref(config: &Config, raw: &str) -> Result<ResolvedModelRef> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty model reference");
    }

    let (base, effort) = split_effort(raw);

    if let Some((provider_id, model_id)) = explicit_provider(config, base) {
        return resolve_exact_model(config, provider_id, model_id, effort);
    }

    resolve_implicit_model(config, base, effort)
}

pub fn resolve_model_info(config: &Config, model: &ResolvedModelRef) -> ResolvedModelInfo {
    let cfg = config.model_config(&model.provider_id, &model.model_id);
    ResolvedModelInfo {
        context_window: cfg.and_then(|item| item.context_window),
        supported_effort_levels: cfg
            .and_then(|item| item.supported_efforts.clone())
            .unwrap_or_default(),
    }
}

pub fn validate_model_effort(config: &Config, model: &ResolvedModelRef) -> Result<()> {
    let Some(effort) = model.effort else {
        return Ok(());
    };

    let cfg = config
        .model_config(&model.provider_id, &model.model_id)
        .ok_or_else(|| anyhow::anyhow!("model not configured: {}", model.canonical))?;
    let Some(levels) = cfg.supported_efforts.as_ref() else {
        bail!(
            "reasoning effort `{effort}` is not supported by model `{}`; no `supported_efforts` configured",
            canonical_base(&model.provider_id, &model.model_id)
        );
    };
    if levels.contains(&effort) {
        return Ok(());
    }

    let supported = if levels.is_empty() {
        "(none)".to_string()
    } else {
        levels
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    bail!(
        "reasoning effort `{effort}` is not supported by model `{}`; supported levels: {supported}",
        canonical_base(&model.provider_id, &model.model_id)
    );
}

pub fn available_models(config: &Config) -> AvailableModelsPayload {
    let mut providers = config
        .providers
        .iter()
        .map(|(provider_id, provider)| {
            let mut models = provider
                .models
                .iter()
                .map(|(model_id, model)| AvailableModel {
                    id: canonical_base(provider_id, model_id),
                    model_id: model_id.clone(),
                    supported_efforts: model.supported_efforts.clone().unwrap_or_default(),
                    context_window: model.context_window,
                    price_per_mtok: model.price_per_mtok.clone(),
                })
                .collect::<Vec<_>>();
            models.sort_by(|left, right| left.model_id.cmp(&right.model_id));
            AvailableProvider {
                id: provider_id.clone(),
                models,
            }
        })
        .collect::<Vec<_>>();
    providers.sort_by(|left, right| left.id.cmp(&right.id));
    AvailableModelsPayload { providers }
}

fn split_effort(raw: &str) -> (&str, Option<EffortLevel>) {
    let Some((base, suffix)) = raw.rsplit_once(':') else {
        return (raw, None);
    };
    match <EffortLevel as FromStr>::from_str(suffix) {
        Ok(level) if !base.is_empty() => (base, Some(level)),
        _ => (raw, None),
    }
}

fn explicit_provider<'a>(config: &'a Config, base: &'a str) -> Option<(&'a str, &'a str)> {
    let (provider_id, model_id) = base.split_once('/')?;
    config
        .providers
        .contains_key(provider_id)
        .then_some((provider_id, model_id))
}

fn resolve_exact_model(
    config: &Config,
    provider_id: &str,
    model_id: &str,
    effort: Option<EffortLevel>,
) -> Result<ResolvedModelRef> {
    if model_id.trim().is_empty() {
        bail!("model reference `{provider_id}/` is missing a model id");
    }
    config
        .model_config(provider_id, model_id)
        .with_context(|| format!("model not configured: {provider_id}/{model_id}"))?;

    let resolved = ResolvedModelRef {
        canonical: canonical_ref(provider_id, model_id, effort),
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
        effort,
    };
    validate_model_effort(config, &resolved)?;
    Ok(resolved)
}

fn resolve_implicit_model(
    config: &Config,
    model_id: &str,
    effort: Option<EffortLevel>,
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

fn canonical_ref(provider_id: &str, model_id: &str, effort: Option<EffortLevel>) -> String {
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
        ProviderConfig, RedactionConfig, TerminalBellConfig,
    };

    fn test_config() -> Config {
        Config {
            providers: HashMap::from([
                (
                    "alpha".into(),
                    ProviderConfig {
                        base_url: "https://alpha.test/v1".into(),
                        api_key_env: "ALPHA_KEY".into(),
                        models: HashMap::from([
                            (
                                "common-model".into(),
                                ModelConfig {
                                    context_window: Some(100),
                                    price_per_mtok: None,
                                    supported_efforts: Some(vec![
                                        EffortLevel::Low,
                                        EffortLevel::Medium,
                                        EffortLevel::High,
                                    ]),
                                },
                            ),
                            (
                                "nested/model".into(),
                                ModelConfig {
                                    context_window: Some(200),
                                    price_per_mtok: None,
                                    supported_efforts: None,
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
                        models: HashMap::from([(
                            "common-model".into(),
                            ModelConfig {
                                context_window: Some(300),
                                price_per_mtok: None,
                                supported_efforts: Some(vec![EffortLevel::Max]),
                            },
                        )]),
                    },
                ),
            ]),
            default_model: "alpha/common-model:medium".into(),
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
        assert_eq!(resolved.effort, Some(EffortLevel::High));
        assert_eq!(resolved.canonical, "alpha/common-model:high");
    }

    #[test]
    fn bare_model_errors_when_ambiguous() {
        let err = resolve_model_ref(&test_config(), "common-model").unwrap_err();
        assert!(err.to_string().contains("ambiguous model"));
    }

    #[test]
    fn validate_config_checks_default_and_review_models() {
        validate_config(&test_config()).unwrap();

        let mut invalid = test_config();
        invalid.default_model = "alpha/missing".into();
        assert!(validate_config(&invalid).is_err());
    }
}
