use std::collections::HashMap;
use std::path::Path;

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

impl Config {
    pub fn load() -> Result<Self> {
        let path = paths::config_dir().join("config.jsonc");
        if !path.exists() {
            bail!(
                "config not found at {} — run `mu init` to create a starter config",
                path.display()
            );
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let value = jsonc_parser::parse_to_serde_value(&raw, &Default::default())
            .map_err(|e| anyhow::anyhow!("parsing config.jsonc: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("config.jsonc is empty"))?;
        let config: Config =
            serde_json::from_value(value).context("invalid config.jsonc structure")?;
        Ok(config)
    }

    pub fn try_load() -> Option<Self> {
        Self::load().ok()
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

    pub fn write_starter(path: &Path) -> Result<()> {
        if path.exists() {
            bail!("config already exists at {}", path.display());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            path,
            r#"{
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
  }
}
"#,
        )?;
        Ok(())
    }
}
