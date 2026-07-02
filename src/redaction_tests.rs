use std::collections::HashMap;

use crate::config::{
    CompactionConfig, Config, GuardrailConfig, LimitsConfig, ProviderConfig, RedactionConfig,
};

use super::*;

fn config(env: &[(&str, &str)], redaction_env: &[&str]) -> Config {
    Config {
        providers: HashMap::from([(
            "test".into(),
            ProviderConfig {
                base_url: "https://example.test".into(),
                api_key_env: "OPENAI_API_KEY".into(),
                models: HashMap::new(),
            },
        )]),
        default_model: "test/model".into(),
        compaction: CompactionConfig::default(),
        limits: LimitsConfig::default(),
        guardrail: GuardrailConfig::default(),
        terminal_bell: crate::config::TerminalBellConfig::default(),
        redaction: RedactionConfig {
            env: redaction_env.iter().map(|name| name.to_string()).collect(),
        },
        env: env
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    }
}

#[test]
fn provider_key_is_implicitly_redacted_across_chunks() {
    let cfg = config(&[("OPENAI_API_KEY", "secret-value")], &[]);
    let mut redactor = SecretRedactor::from_config(&cfg);

    let mut out = String::new();
    out.push_str(&redactor.redact_chunk("before secret"));
    out.push_str(&redactor.redact_chunk("-value after"));
    out.push_str(&redactor.finish());

    assert_eq!(out, "before [redacted:OPENAI_API_KEY] after");
    assert!(redactor.did_redact());
}

#[test]
fn empty_values_warn_and_are_ignored() {
    let cfg = config(&[("OPENAI_API_KEY", ""), ("TOKEN", "abc")], &["TOKEN"]);
    let mut redactor = SecretRedactor::from_config(&cfg);

    assert!(
        redactor
            .warnings()
            .iter()
            .any(|warning| warning.contains("OPENAI_API_KEY") && warning.contains("empty"))
    );
    assert!(
        redactor
            .warnings()
            .iter()
            .any(|warning| warning.contains("TOKEN") && warning.contains("short"))
    );

    let mut out = String::new();
    out.push_str(&redactor.redact_chunk("empty is  and token is abc"));
    out.push_str(&redactor.finish());
    assert_eq!(out, "empty is  and token is [redacted:TOKEN]");
}
