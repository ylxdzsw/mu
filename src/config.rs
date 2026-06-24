use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::paths;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub provider: ProviderConfig,
    pub default_model: String,
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    #[serde(default = "default_agent_mode_key")]
    pub agent_mode_key: String,
    #[serde(default)]
    pub magic_space: bool,
    #[serde(default)]
    pub compaction: CompactionConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub guardrail: GuardrailConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
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

fn default_agent_mode_key() -> String {
    "\\eM".into()
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

impl Config {
    pub fn load_for_scope(project_config_dir: Option<&Path>) -> Result<Self> {
        let global_path = paths::global_dir().join("config.jsonc");
        let mut value = read_config_value(&global_path)?;

        if let Some(dir) = project_config_dir {
            let project_path = dir.join("config.jsonc");
            if project_path.exists() {
                let project = read_config_value(&project_path)?;
                merge_json(&mut value, project);
            }
        }

        let config: Config =
            serde_json::from_value(value).context("invalid config.jsonc structure")?;
        Ok(config)
    }

    pub fn try_load_for_scope(project_config_dir: Option<&Path>) -> Option<Self> {
        Self::load_for_scope(project_config_dir).ok()
    }

    pub fn api_key(&self) -> Result<String> {
        let key = std::env::var(&self.provider.api_key_env).with_context(|| {
            format!(
                "API key env var `{}` is not set (see config.jsonc)",
                self.provider.api_key_env
            )
        })?;
        if key.is_empty() {
            bail!("API key env var `{}` is empty", self.provider.api_key_env);
        }
        Ok(key)
    }

    pub fn context_window(&self, model: &str) -> Option<u64> {
        self.models.get(model).and_then(|m| m.context_window)
    }

    pub fn starter_path() -> PathBuf {
        paths::global_dir().join("config.jsonc")
    }

    pub fn write_starter(path: &Path) -> Result<()> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, STARTER_CONFIG)?;
            return Ok(());
        }
        bail!("config already exists at {}", path.display());
    }
}

fn read_config_value(path: &Path) -> Result<serde_json::Value> {
    if !path.exists() {
        bail!(
            "config not found at {} — run `mu init` to create a starter config",
            path.display()
        );
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
  "models": {
    "gpt-4o": {
      "context_window": 128000,
      "price_per_mtok": { "input": 2.5, "output": 10.0 }
    }
  },
  "agent_mode_key": "\\eM",
  "magic_space": false,
  "compaction": { "fraction": 0.75, "keep_recent_turns": 2 },
  "limits": {
    "max_iterations": 50,
    "max_lines": 2000,
    "max_bytes": 51200,
    "max_line_bytes": 10240
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
            "models": {"one": {"context_window": 1}},
            "limits": {"max_iterations": 5, "max_lines": 10}
        });
        let overlay = serde_json::json!({
            "default_model": "two",
            "models": {"two": {"context_window": 2}},
            "limits": {"max_lines": 20}
        });
        merge_json(&mut base, overlay);

        assert_eq!(base["default_model"], "two");
        assert_eq!(base["models"]["one"]["context_window"], 1);
        assert_eq!(base["models"]["two"]["context_window"], 2);
        assert_eq!(base["limits"]["max_iterations"], 5);
        assert_eq!(base["limits"]["max_lines"], 20);
    }
}
