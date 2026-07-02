use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::env::EnvMap;
use crate::models::EffortLevel;
use crate::paths;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub default_model: String,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub guardrail: GuardrailConfig,
    #[serde(default)]
    pub terminal_bell: TerminalBellConfig,
    #[serde(default)]
    pub redaction: RedactionConfig,
    #[serde(skip)]
    pub env: EnvMap,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderConfig {
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelConfig {
    pub context_window: Option<u64>,
    #[serde(default)]
    pub price_per_mtok: Option<PriceConfig>,
    #[serde(default)]
    pub supported_efforts: Option<Vec<EffortLevel>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize)]
pub struct TerminalBellConfig {
    #[serde(default = "default_terminal_bell_enabled")]
    pub enabled: bool,
    #[serde(default = "default_terminal_bell_min_duration_ms")]
    pub min_duration_ms: u64,
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
fn default_terminal_bell_enabled() -> bool {
    true
}
fn default_terminal_bell_min_duration_ms() -> u64 {
    10_000
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

impl Default for TerminalBellConfig {
    fn default() -> Self {
        Self {
            enabled: default_terminal_bell_enabled(),
            min_duration_ms: default_terminal_bell_min_duration_ms(),
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
        config.validate_runtime()?;
        Ok(config)
    }

    pub fn provider(&self, provider_id: &str) -> Result<&ProviderConfig> {
        self.providers
            .get(provider_id)
            .with_context(|| format!("unknown provider `{provider_id}` in config.jsonc"))
    }

    pub fn model_config(&self, provider_id: &str, model_id: &str) -> Option<&ModelConfig> {
        self.providers
            .get(provider_id)
            .and_then(|provider| provider.models.get(model_id))
    }

    pub fn api_key_for_provider(&self, provider_id: &str) -> Result<Option<String>> {
        let provider = self.provider(provider_id)?;
        if provider.api_key_env.trim().is_empty() {
            return Ok(None);
        }
        let key = self
            .env
            .get(&provider.api_key_env)
            .cloned()
            .with_context(|| {
                format!(
                    "API key env var `{}` is not set (provider `{provider_id}` in config.jsonc)",
                    provider.api_key_env
                )
            })?;
        if key.is_empty() {
            bail!("API key env var `{}` is empty", provider.api_key_env);
        }
        Ok(Some(key))
    }

    pub fn validate_structure(&self) -> Result<()> {
        if self.providers.is_empty() {
            bail!("no providers configured in config.jsonc: set `providers`");
        }
        if self.default_model.trim().is_empty() {
            bail!("no default model configured in config.jsonc: set `default_model`");
        }
        for (provider_id, provider) in &self.providers {
            if provider.base_url.trim().is_empty() {
                bail!("provider `{provider_id}` is missing `base_url` in config.jsonc");
            }
        }
        Ok(())
    }

    pub fn validate_runtime(&self) -> Result<()> {
        self.validate_structure()?;
        crate::models::validate_config(self)
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
    config.validate_structure()?;
    Ok(config)
}

fn read_config_value(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        bail!("config not found at {}", path.display());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    jsonc_parser::parse_to_serde_value::<Option<serde_json::Value>>(&raw, &Default::default())
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
  "providers": {
    "openai": {
      "base_url": "https://api.openai.com/v1",
      "api_key_env": "OPENAI_API_KEY",
      "models": {
        "gpt-4o": {
          "context_window": 128000,
          "price_per_mtok": { "input": 2.5, "output": 10.0 },
          "supported_efforts": ["low", "medium", "high"]
        }
      }
    }
  },
  "default_model": "openai/gpt-4o",
  "terminal_bell": {
    "enabled": true,
    "min_duration_ms": 10000
  },
  "compaction": { "fraction": 0.75, "keep_recent_turns": 2 },
  "limits": {
    "max_iterations": 50,
    "max_lines": 2000,
    "max_bytes": 51200,
    "max_line_bytes": 10240
  },
  "redaction": {
    "env": []
  },
  "guardrail": {
    "enabled": false,
    "timeout_ms": 90000,
    "circuit_breaker": { "consecutive": 3, "window": 50, "window_denials": 10 }
  }
}
"#;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

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
}
