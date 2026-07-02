use std::collections::HashMap;

use crate::config::{
    CircuitBreakerConfig, CompactionConfig, GuardrailConfig, LimitsConfig, ModelConfig,
    ProviderConfig, RedactionConfig, TerminalBellConfig,
};

use super::*;

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
fn resolves_bare_model_when_unique() {
    let resolved = resolve_model_ref(&test_config(), "nested/model").unwrap();
    assert_eq!(resolved.canonical, "alpha/nested/model");
}

#[test]
fn bare_model_errors_when_ambiguous() {
    let err = resolve_model_ref(&test_config(), "common-model").unwrap_err();
    assert!(err.to_string().contains("ambiguous model"));
}

#[test]
fn effort_without_configured_support_is_rejected() {
    let err = resolve_model_ref(&test_config(), "alpha/nested/model:high").unwrap_err();
    assert!(err.to_string().contains("supported_efforts"));
}

#[test]
fn unknown_provider_prefix_can_still_match_bare_model() {
    let mut config = test_config();
    config.providers.get_mut("alpha").unwrap().models.insert(
        "ghost/model".into(),
        ModelConfig {
            context_window: None,
            price_per_mtok: None,
            supported_efforts: None,
        },
    );

    let resolved = resolve_model_ref(&config, "ghost/model").unwrap();
    assert_eq!(resolved.canonical, "alpha/ghost/model");
}

#[test]
fn available_models_are_grouped_and_sorted() {
    let available = available_models(&test_config());
    assert_eq!(available.providers.len(), 2);
    assert_eq!(available.providers[0].id, "alpha");
    assert_eq!(available.providers[0].models[0].model_id, "common-model");
}

#[test]
fn validate_config_checks_default_and_review_models() {
    validate_config(&test_config()).unwrap();

    let mut invalid = test_config();
    invalid.default_model = "alpha/missing".into();
    assert!(validate_config(&invalid).is_err());
}
