use super::*;

#[test]
fn merge_overrides_nested_values_and_keeps_base() {
    let mut base = serde_json::json!({
        "providers": {
            "alpha": {
                "base_url": "a",
                "api_key_env": "KEY",
                "models": {"one": {"context_window": 1}}
            }
        },
        "default_model": "alpha/one",
        "limits": {"max_iterations": 5, "max_lines": 10}
    });
    let overlay = serde_json::json!({
        "providers": {
            "alpha": {
                "models": {"two": {"context_window": 2}}
            },
            "beta": {
                "base_url": "b",
                "api_key_env": "BETA_KEY",
                "models": {"three": {"context_window": 3}}
            }
        },
        "default_model": "beta/three",
        "limits": {"max_lines": 20}
    });
    merge_json(&mut base, overlay);

    assert_eq!(base["default_model"], "beta/three");
    assert_eq!(
        base["providers"]["alpha"]["models"]["one"]["context_window"],
        1
    );
    assert_eq!(
        base["providers"]["alpha"]["models"]["two"]["context_window"],
        2
    );
    assert_eq!(
        base["providers"]["beta"]["models"]["three"]["context_window"],
        3
    );
    assert_eq!(base["limits"]["max_iterations"], 5);
    assert_eq!(base["limits"]["max_lines"], 20);
}

#[test]
fn api_key_reads_effective_env_for_provider() {
    let config = Config {
        providers: HashMap::from([(
            "alpha".into(),
            ProviderConfig {
                base_url: "http://localhost".into(),
                api_key_env: "TEST_KEY".into(),
                models: HashMap::new(),
            },
        )]),
        default_model: "alpha/test-model".into(),
        compaction: CompactionConfig::default(),
        limits: LimitsConfig::default(),
        guardrail: GuardrailConfig::default(),
        terminal_bell: TerminalBellConfig::default(),
        redaction: RedactionConfig::default(),
        env: HashMap::from([("TEST_KEY".into(), "secret".into())]),
    };

    assert_eq!(
        config.api_key_for_provider("alpha").unwrap(),
        Some("secret".into())
    );
}

#[test]
fn api_key_returns_none_when_env_name_is_empty() {
    let config = Config {
        providers: HashMap::from([(
            "alpha".into(),
            ProviderConfig {
                base_url: "http://localhost".into(),
                api_key_env: String::new(),
                models: HashMap::new(),
            },
        )]),
        default_model: "alpha/test-model".into(),
        compaction: CompactionConfig::default(),
        limits: LimitsConfig::default(),
        guardrail: GuardrailConfig::default(),
        terminal_bell: TerminalBellConfig::default(),
        redaction: RedactionConfig::default(),
        env: HashMap::new(),
    };

    assert_eq!(config.api_key_for_provider("alpha").unwrap(), None);
}

#[test]
fn creates_starter_config_when_global_config_is_missing() {
    let tmp = std::env::temp_dir().join(format!("mu-config-{}", uuid::Uuid::new_v4()));
    let path = tmp.join("config.jsonc");

    ensure_starter_config(&path).unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("\"providers\""));
    assert!(raw.contains("\"default_model\""));

    let _ = std::fs::remove_dir_all(tmp);
}

#[test]
fn parse_rejects_missing_providers() {
    let value = serde_json::json!({
        "default_model": "openai/gpt-4o"
    });

    let err = config_from_value(value).unwrap_err();
    assert!(err.to_string().contains("no providers configured"));
}

#[test]
fn parse_rejects_missing_default_model() {
    let value = serde_json::json!({
        "providers": {
            "openai": {
                "base_url": "http://localhost",
                "models": {"gpt-4o": {"context_window": 128000}}
            }
        }
    });

    let err = config_from_value(value).unwrap_err();
    assert!(err.to_string().contains("no default model configured"));
}
