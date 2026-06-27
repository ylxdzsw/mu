use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::env::EnvMap;
use crate::models::EffortLevel;
use crate::paths;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub default_effort: Option<EffortLevel>,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub guardrail: GuardrailConfig,
    #[serde(default)]
    pub redaction: RedactionConfig,
    #[serde(skip)]
    pub env: EnvMap,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelConfig {
    pub context_window: Option<u64>,
    #[serde(default)]
    pub price_per_mtok: Option<PriceConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceConfig {
    pub input: f64,
    pub output: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompactionConfig {
    #[serde(default = "default_fraction")]
    pub fraction: f64,
    #[serde(default = "default_keep_recent")]
    pub keep_recent_turns: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
    #[serde(default = "default_max_line_bytes")]
    pub max_line_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GuardrailConfig {
    #[serde(default = "default_guardrail_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub review_model: Option<String>,
    #[serde(default = "default_guardrail_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CircuitBreakerConfig {
    #[serde(default = "default_cb_consecutive")]
    pub consecutive: u32,
    #[serde(default = "default_cb_window")]
    pub window: usize,
    #[serde(default = "default_cb_window_denials")]
    pub window_denials: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RedactionConfig {
    #[serde(default)]
    pub env: Vec<String>,
}

fn default_fraction() -> f64 {
    0.75
}
fn default_keep_recent() -> usize {
    2
}
fn default_max_iterations() -> usize {
    50
}
fn default_max_lines() -> usize {
    2000
}
fn default_max_bytes() -> usize {
    51200
}
fn default_max_line_bytes() -> usize {
    10240
}
fn default_guardrail_enabled() -> bool {
    false
}
fn default_guardrail_timeout_ms() -> u64 {
    90_000
}
fn default_cb_consecutive() -> u32 {
    3
}
fn default_cb_window() -> usize {
    50
}
fn default_cb_window_denials() -> u32 {
    10
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            fraction: default_fraction(),
            keep_recent_turns: default_keep_recent(),
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_iterations: default_max_iterations(),
            max_lines: default_max_lines(),
            max_bytes: default_max_bytes(),
            max_line_bytes: default_max_line_bytes(),
        }
    }
}

impl Default for GuardrailConfig {
    fn default() -> Self {
        Self {
            enabled: default_guardrail_enabled(),
            review_model: None,
            timeout_ms: default_guardrail_timeout_ms(),
            circuit_breaker: CircuitBreakerConfig::default(),
        }
    }
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            consecutive: default_cb_consecutive(),
            window: default_cb_window(),
            window_denials: default_cb_window_denials(),
        }
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            api_key_env: String::new(),
        }
    }
}

impl Config {
    pub fn load_for_scope(project_config_dir: Option<&Path>) -> Result<Self> {
        let global_path = paths::global_dir().join("config.jsonc");
        ensure_starter_config(&global_path)?;
        let mut value = read_config_value(&global_path)?;

        if let Some(dir) = project_config_dir {
            let project_path = dir.join("config.jsonc");
            if project_path.exists() {
                let project = read_config_value(&project_path)?;
                merge_json(&mut value, project);
            }
        }

        let mut config = config_from_value(value)?;
        config.env = crate::env::load_effective(project_config_dir)?;
        Ok(config)
    }

    pub fn try_load_for_scope(project_config_dir: Option<&Path>) -> Option<Self> {
        Self::load_for_scope(project_config_dir).ok()
    }

    pub fn api_key(&self) -> Result<Option<String>> {
        if self.provider.api_key_env.trim().is_empty() {
            return Ok(None);
        }
        let key = self
            .env
            .get(&self.provider.api_key_env)
            .cloned()
            .with_context(|| {
                format!(
                    "API key env var `{}` is not set (see config.jsonc)",
                    self.provider.api_key_env
                )
            })?;
        if key.is_empty() {
            bail!("API key env var `{}` is empty", self.provider.api_key_env);
        }
        Ok(Some(key))
    }

    pub fn context_window(&self, model: &str) -> Option<u64> {
        self.models.get(model).and_then(|m| m.context_window)
    }

    fn validate(&self) -> Result<()> {
        if self.provider.base_url.trim().is_empty() {
            bail!("no provider configured in config.jsonc: set `provider.base_url`");
        }
        if self.default_model.trim().is_empty() {
            bail!("no default model configured in config.jsonc: set `default_model`");
        }
        Ok(())
    }
}

fn ensure_starter_config(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, STARTER_CONFIG)?;
    Ok(())
}

fn config_from_value(value: serde_json::Value) -> Result<Config> {
    let config: Config = serde_json::from_value(value).context("invalid config.jsonc structure")?;
    config.validate()?;
    Ok(config)
}

fn read_config_value(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        bail!("config not found at {}", path.display());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    jsonc_parser::parse_to_serde_value(&raw, &Default::default())
        .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?
        .ok_or_else(|| anyhow::anyhow!("{} is empty", path.display()))
}

fn merge_json(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_json(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

const STARTER_CONFIG: &str = r#"{
  // Provider settings — set the env var named below to your API key
  "provider": {
    "base_url": "https://api.openai.com/v1",
    "api_key_env": "OPENAI_API_KEY"
  },
  "default_model": "gpt-4o",
  // Optional default reasoning effort: null, "low", "medium", "high", "xhigh", or "max".
  "default_effort": null,
  "models": {
    "gpt-4o": {
      "context_window": 128000,
      "price_per_mtok": { "input": 2.5, "output": 10.0 }
    }
  },
  "compaction": { "fraction": 0.75, "keep_recent_turns": 2 },
  "limits": {
    "max_iterations": 50,
    "max_lines": 2000,
    "max_bytes": 51200,
    "max_line_bytes": 10240
  },
  "redaction": {
    // Provider api_key_env is always included automatically.
    "env": []
  },
  "guardrail": {
    "enabled": false,
    // "review_model": "gpt-4o-mini",  // null → same as default_model
    "timeout_ms": 90000,
    "circuit_breaker": { "consecutive": 3, "window": 50, "window_denials": 10 }
  }
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_overrides_nested_values_and_keeps_base() {
        let mut base = serde_json::json!({
            "provider": {"base_url": "a", "api_key_env": "KEY"},
            "default_model": "one",
            "default_effort": "low",
            "models": {"one": {"context_window": 1}},
            "limits": {"max_iterations": 5, "max_lines": 10}
        });
        let overlay = serde_json::json!({
            "default_model": "two",
            "default_effort": null,
            "models": {"two": {"context_window": 2}},
            "limits": {"max_lines": 20}
        });
        merge_json(&mut base, overlay);

        assert_eq!(base["default_model"], "two");
        assert!(base["default_effort"].is_null());
        assert_eq!(base["models"]["one"]["context_window"], 1);
        assert_eq!(base["models"]["two"]["context_window"], 2);
        assert_eq!(base["limits"]["max_iterations"], 5);
        assert_eq!(base["limits"]["max_lines"], 20);
    }

    #[test]
    fn api_key_reads_effective_env() {
        let config = Config {
            provider: ProviderConfig {
                base_url: "http://localhost".into(),
                api_key_env: "TEST_KEY".into(),
            },
            default_model: "test-model".into(),
            default_effort: None,
            models: HashMap::new(),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::from([("TEST_KEY".into(), "secret".into())]),
        };

        assert_eq!(config.api_key().unwrap(), Some("secret".into()));
    }

    #[test]
    fn api_key_returns_none_when_env_not_set() {
        let config = Config {
            provider: ProviderConfig {
                base_url: "http://localhost".into(),
                api_key_env: String::new(),
            },
            default_model: "test-model".into(),
            default_effort: None,
            models: HashMap::new(),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: GuardrailConfig::default(),
            redaction: RedactionConfig::default(),
            env: HashMap::new(),
        };

        assert_eq!(config.api_key().unwrap(), None);
    }

    #[test]
    fn creates_starter_config_when_global_config_is_missing() {
        let tmp = std::env::temp_dir().join(format!("mu-config-{}", uuid::Uuid::new_v4()));
        let path = tmp.join("config.jsonc");

        ensure_starter_config(&path).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"provider\""));
        assert!(raw.contains("\"default_model\""));
        assert!(raw.contains("\"default_effort\""));
    }

    #[test]
    fn default_effort_accepts_canonical_values() {
        let config = config_from_value(serde_json::json!({
            "provider": {"base_url": "http://localhost", "api_key_env": ""},
            "default_model": "gpt-4o",
            "default_effort": "high"
        }))
        .unwrap();

        assert_eq!(config.default_effort, Some(EffortLevel::High));
    }

    #[test]
    fn invalid_default_effort_fails_to_parse() {
        let err = config_from_value(serde_json::json!({
            "provider": {"base_url": "http://localhost", "api_key_env": ""},
            "default_model": "gpt-4o",
            "default_effort": "extreme"
        }))
        .unwrap_err();

        assert!(err.to_string().contains("invalid config.jsonc structure"));
    }

    #[test]
    fn missing_provider_fails_with_clear_message() {
        let err = config_from_value(serde_json::json!({
            "default_model": "gpt-4o"
        }))
        .unwrap_err();

        assert!(err.to_string().contains("no provider configured"));
    }
}
