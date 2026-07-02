use serde_json::json;

use super::{Tool, ToolRegistry, bash::BashTool};
use crate::config::{CompactionConfig, Config, LimitsConfig, ProviderConfig};
use std::collections::HashMap;

#[test]
fn registry_exposes_only_bash() {
    let registry = ToolRegistry::new(&test_config());
    let definitions = registry.definitions();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0]["function"]["name"].as_str(), Some("bash"));
    assert!(registry.get("bash").is_some());
    for removed in ["read", "write", "edit", "fetch", "search"] {
        assert!(
            registry.get(removed).is_none(),
            "{removed} should be hidden"
        );
    }
}

#[test]
fn bash_schema_requires_title_risk_and_script() {
    let schema = BashTool.parameters_schema();
    assert_eq!(schema["required"], json!(["title", "risk", "script"]));
    assert_eq!(
        schema["properties"]["risk"]["enum"],
        json!(["readonly", "reversible", "destructive"])
    );
    assert!(schema["properties"].get("command").is_none());
    assert!(schema["properties"].get("workdir").is_none());
    assert!(schema["properties"].get("cwd").is_some());
    assert!(schema["properties"].get("stdin").is_some());
}

fn test_config() -> Config {
    Config {
        providers: HashMap::from([(
            "test".into(),
            ProviderConfig {
                base_url: "http://localhost".into(),
                api_key_env: "MU_TEST_KEY".into(),
                models: HashMap::new(),
            },
        )]),
        default_model: "test/test-model".into(),
        compaction: CompactionConfig::default(),
        limits: LimitsConfig::default(),
        guardrail: crate::config::GuardrailConfig::default(),
        terminal_bell: crate::config::TerminalBellConfig::default(),
        redaction: crate::config::RedactionConfig::default(),
        env: HashMap::new(),
    }
}
