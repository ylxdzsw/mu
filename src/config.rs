use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::cli::OutputFormat;
use crate::paths;

pub type EnvMap = HashMap<String, String>;

pub fn load_effective_env(project_config_dir: Option<&Path>) -> Result<EnvMap> {
    let mut env: EnvMap = std::env::vars().collect();
    load_dotenv_into(&paths::global_dir().join(".env"), &mut env)?;
    if let Some(dir) = project_config_dir {
        load_dotenv_into(&dir.join(".env"), &mut env)?;
    }
    Ok(env)
}

fn load_dotenv_into(path: &Path, env: &mut EnvMap) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let contents =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let entries = parse_env_file(&contents).map_err(|error| {
        anyhow!(
            "parsing {}:{}: {}",
            path.display(),
            error.line,
            error.reason
        )
    })?;
    for (key, value) in entries {
        env.insert(key, value);
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct EnvParseError {
    line: usize,
    reason: &'static str,
}

fn parse_env_file(contents: &str) -> std::result::Result<Vec<(String, String)>, EnvParseError> {
    let mut entries = Vec::new();

    for (index, line) in contents.lines().enumerate() {
        let line_number = index + 1;
        if line.contains(['\0', '\r']) {
            return Err(env_parse_error(
                line_number,
                "unsupported control character",
            ));
        }

        let trimmed = line.trim_matches([' ', '\t']);
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(env_parse_error(
                line_number,
                "assignments cannot be indented",
            ));
        }

        let assignment = if let Some(rest) = line.strip_prefix("export") {
            if rest.starts_with([' ', '\t']) {
                rest.trim_start_matches([' ', '\t'])
            } else {
                line
            }
        } else {
            line
        };
        let Some((name, source_value)) = assignment.split_once('=') else {
            return Err(env_parse_error(line_number, "expected NAME=VALUE"));
        };
        if !valid_env_name(name) {
            return Err(env_parse_error(line_number, "invalid variable name"));
        }

        let value =
            parse_env_value(source_value).map_err(|reason| env_parse_error(line_number, reason))?;
        entries.push((name.to_owned(), value));
    }

    Ok(entries)
}

fn env_parse_error(line: usize, reason: &'static str) -> EnvParseError {
    EnvParseError { line, reason }
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn parse_env_value(source: &str) -> std::result::Result<String, &'static str> {
    if source.starts_with('\'') {
        return parse_single_quoted_env_value(source);
    }
    if source.starts_with('"') {
        return parse_double_quoted_env_value(source);
    }
    if source.bytes().all(is_safe_bare_env_byte) {
        return Ok(source.to_owned());
    }
    Err("unsupported bare value; quote values containing spaces or shell syntax")
}

fn is_safe_bare_env_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'_' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b',' | b'=' | b'-'
        )
}

fn parse_single_quoted_env_value(source: &str) -> std::result::Result<String, &'static str> {
    let Some(value) = source
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    else {
        return Err("unterminated single-quoted value");
    };
    if value.contains('\'') {
        return Err("trailing syntax after single-quoted value");
    }
    Ok(value.to_owned())
}

fn parse_double_quoted_env_value(source: &str) -> std::result::Result<String, &'static str> {
    let mut chars = source[1..].chars().peekable();
    let mut value = String::new();

    while let Some(ch) = chars.next() {
        match ch {
            '"' if chars.peek().is_none() => return Ok(value),
            '"' => return Err("trailing syntax after double-quoted value"),
            '\\' => {
                let Some(escaped) = chars.next() else {
                    return Err("unterminated escape in double-quoted value");
                };
                if matches!(escaped, '"' | '\\' | '$' | '`') {
                    value.push(escaped);
                } else {
                    return Err("unsupported escape in double-quoted value");
                }
            }
            '$' | '`' => return Err("unescaped shell expansion in double-quoted value"),
            _ => value.push(ch),
        }
    }

    Err("unterminated double-quoted value")
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: OrderedMap<ProviderConfig>,
    #[serde(default)]
    pub output: OutputFormat,
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
    pub endpoint: String,
    #[serde(default)]
    pub api_key_env: String,
    #[serde(default)]
    pub models: OrderedMap<ModelConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ModelConfig {
    pub context_window: Option<u64>,
    #[serde(default)]
    pub supported_efforts: Option<Vec<String>>,
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

#[derive(Debug, Clone)]
pub struct OrderedMap<T> {
    entries: Vec<(String, T)>,
}

impl<T> Default for OrderedMap<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
        }
    }
}

impl<T> OrderedMap<T> {
    pub fn get(&self, key: &str) -> Option<&T> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value)
    }

    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &T)> {
        self.entries.iter().map(|(key, value)| (key, value))
    }

    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&String, &mut T)> {
        self.entries.iter_mut().map(|(key, value)| (&*key, value))
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().map(|(_, value)| value)
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.entries.iter().map(|(key, _)| key)
    }

    pub fn reorder_by_keys(&mut self, order: &[String]) {
        let mut remaining = std::mem::take(&mut self.entries);
        let mut ordered = Vec::with_capacity(remaining.len());
        for key in order {
            if let Some(index) = remaining.iter().position(|(candidate, _)| candidate == key) {
                ordered.push(remaining.remove(index));
            }
        }
        ordered.extend(remaining);
        self.entries = ordered;
    }
}

impl<'a, T> IntoIterator for &'a OrderedMap<T> {
    type Item = (&'a String, &'a T);
    type IntoIter =
        std::iter::Map<std::slice::Iter<'a, (String, T)>, fn(&(String, T)) -> (&String, &T)>;

    fn into_iter(self) -> Self::IntoIter {
        fn as_refs<T>((key, value): &(String, T)) -> (&String, &T) {
            (key, value)
        }
        self.entries.iter().map(as_refs::<T>)
    }
}

impl<T> FromIterator<(String, T)> for OrderedMap<T> {
    fn from_iter<I: IntoIterator<Item = (String, T)>>(iter: I) -> Self {
        let mut entries = Vec::new();
        for (key, value) in iter {
            if let Some((_, existing)) = entries.iter_mut().find(|(candidate, _)| candidate == &key)
            {
                *existing = value;
            } else {
                entries.push((key, value));
            }
        }
        Self { entries }
    }
}

impl<'de, T> Deserialize<'de> for OrderedMap<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OrderedMapVisitor<T> {
            marker: std::marker::PhantomData<T>,
        }

        impl<'de, T> Visitor<'de> for OrderedMapVisitor<T>
        where
            T: Deserialize<'de>,
        {
            type Value = OrderedMap<T>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an object")
            }

            fn visit_map<A>(self, mut access: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::with_capacity(access.size_hint().unwrap_or(0));
                while let Some((key, value)) = access.next_entry::<String, T>()? {
                    if let Some((_, existing)) =
                        entries.iter_mut().find(|(candidate, _)| candidate == &key)
                    {
                        *existing = value;
                    } else {
                        entries.push((key, value));
                    }
                }
                Ok(OrderedMap { entries })
            }
        }

        deserializer.deserialize_map(OrderedMapVisitor {
            marker: std::marker::PhantomData,
        })
    }
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
    true
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
        let (mut value, global_order) = read_config_file(&global_path)?;
        let mut order = combined_config_order(&global_order, None);

        if let Some(dir) = project_config_dir {
            let project_path = dir.join("config.jsonc");
            if project_path.exists() {
                let (project, project_order) = read_config_file(&project_path)?;
                merge_json(&mut value, project);
                order = combined_config_order(&global_order, Some(&project_order));
            }
        }

        let mut config = config_from_value(value)?;
        apply_config_order(&mut config, &order);
        config.env = load_effective_env(project_config_dir)?;
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
        for selector in &self.redaction.env {
            redaction_suffix(selector)?;
        }
        for (provider_id, provider) in &self.providers {
            if provider.endpoint.trim().is_empty() {
                bail!(
                    "provider `{provider_id}` is missing `endpoint` in config.jsonc; configure the complete URL ending in `/chat/completions` or `/responses`"
                );
            }
            crate::provider::classify_endpoint(&provider.endpoint).map_err(|error| {
                anyhow::anyhow!("invalid provider `{provider_id}` in config.jsonc: {error}")
            })?;
        }
        Ok(())
    }

    pub fn validate_runtime(&self) -> Result<()> {
        self.validate_structure()?;
        crate::models::validate_config(self)
    }
}

pub(crate) fn redaction_suffix(selector: &str) -> Result<Option<&str>> {
    let Some(suffix) = selector.strip_prefix('*') else {
        if selector.contains('*') {
            bail!(
                "invalid redaction env selector `{selector}`: `*` is only allowed as the first character"
            );
        }
        return Ok(None);
    };
    if suffix.is_empty() {
        bail!(
            "invalid redaction env selector `{selector}`: `*` must be followed by a literal suffix"
        );
    }
    if suffix.contains('*') {
        bail!("invalid redaction env selector `{selector}`: exactly one `*` is allowed");
    }
    Ok(Some(suffix))
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

fn read_config_file(path: &Path) -> Result<(serde_json::Value, ConfigOrder)> {
    if !path.exists() {
        bail!("config not found at {}", path.display());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let value =
        jsonc_parser::parse_to_serde_value::<Option<serde_json::Value>>(&raw, &Default::default())
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?
            .ok_or_else(|| anyhow::anyhow!("{} is empty", path.display()))?;
    let order =
        jsonc_parser::parse_to_serde_value::<Option<ConfigOrderRaw>>(&raw, &Default::default())
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?
            .unwrap_or_default()
            .into_order();
    Ok((value, order))
}

#[derive(Debug, Clone, Default)]
struct ConfigOrder {
    providers: Vec<String>,
    models: HashMap<String, Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct ConfigOrderRaw {
    #[serde(default)]
    providers: OrderedMap<ProviderOrderRaw>,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderOrderRaw {
    #[serde(default)]
    models: OrderedMap<serde::de::IgnoredAny>,
}

impl ConfigOrderRaw {
    fn into_order(self) -> ConfigOrder {
        let providers = self.providers.keys().cloned().collect::<Vec<_>>();
        let models = self
            .providers
            .iter()
            .map(|(provider_id, provider)| {
                (
                    provider_id.clone(),
                    provider.models.keys().cloned().collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();
        ConfigOrder { providers, models }
    }
}

fn combined_config_order(global: &ConfigOrder, project: Option<&ConfigOrder>) -> ConfigOrder {
    let Some(project) = project else {
        return global.clone();
    };

    let mut providers = project.providers.clone();
    providers.extend(
        global
            .providers
            .iter()
            .filter(|provider_id| !project.providers.contains(provider_id))
            .cloned(),
    );

    let mut models = HashMap::new();
    for provider_id in &providers {
        let project_models = project.models.get(provider_id).cloned().unwrap_or_default();
        let global_models = global.models.get(provider_id).cloned().unwrap_or_default();
        let mut merged_models = project_models.clone();
        merged_models.extend(
            global_models
                .into_iter()
                .filter(|model_id| !project_models.contains(model_id)),
        );
        models.insert(provider_id.clone(), merged_models);
    }

    ConfigOrder { providers, models }
}

fn apply_config_order(config: &mut Config, order: &ConfigOrder) {
    config.providers.reorder_by_keys(&order.providers);
    for (provider_id, provider) in config.providers.iter_mut() {
        if let Some(model_order) = order.models.get(provider_id) {
            provider.models.reorder_by_keys(model_order);
        }
    }
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
  "output": "detail",
  "providers": {
    // Default provider: OpenCode Zen's free DeepSeek model. It needs no API
    // key, so mu works out of the box. Other free models include
    // "big-pickle", "mimo-v2.5-free", and "north-mini-code-free".
    "opencode": {
      "endpoint": "https://opencode.ai/zen/v1/chat/completions",
      "api_key_env": "",
      "models": {
        "deepseek-v4-flash-free": {
          "context_window": 128000
        }
      }
    },
    // Example keyed provider. Add OPENAI_API_KEY to ~/.mu/.env, then select it
    // with `mu --model openai/gpt-4o` or reorder providers to make it default.
    "openai": {
      "endpoint": "https://api.openai.com/v1/chat/completions",
      "api_key_env": "OPENAI_API_KEY",
      "models": {
        "gpt-4o": {
          "context_window": 128000,
          "supported_efforts": ["low", "medium", "high"]
        }
      }
    }
  },
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
    "enabled": true,
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
                    "endpoint": "https://a.test/chat/completions",
                    "api_key_env": "KEY",
                    "models": {"one": {"context_window": 1}}
                }
            },
            "limits": {"max_iterations": 5, "max_lines": 10}
        });
        let overlay = serde_json::json!({
            "providers": {
                "alpha": {
                    "models": {"two": {"context_window": 2}}
                },
                "beta": {
                    "endpoint": "https://b.test/chat/completions",
                    "api_key_env": "BETA_KEY",
                    "models": {"three": {"context_window": 3}}
                }
            },
            "limits": {"max_lines": 20}
        });
        merge_json(&mut base, overlay);

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
            providers: OrderedMap::from_iter([(
                "alpha".into(),
                ProviderConfig {
                    endpoint: "http://localhost/chat/completions".into(),
                    api_key_env: "TEST_KEY".into(),
                    models: OrderedMap::default(),
                },
            )]),
            output: OutputFormat::Detail,
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
    fn parse_accepts_without_explicit_default() {
        let value = serde_json::json!({
            "providers": {
                "openai": {
                    "endpoint": "http://localhost/chat/completions",
                    "models": {"gpt-4o": {"context_window": 128000}}
                }
            }
        });

        let config = config_from_value(value).unwrap();
        assert_eq!(config.output, OutputFormat::Detail);
    }

    #[test]
    fn parse_accepts_configured_output() {
        let value = serde_json::json!({
            "output": "concise",
            "providers": {
                "openai": {
                    "endpoint": "http://localhost/chat/completions",
                    "models": {"gpt-4o": {"context_window": 128000}}
                }
            }
        });

        let config = config_from_value(value).unwrap();
        assert_eq!(config.output, OutputFormat::Concise);
    }

    #[test]
    fn parse_accepts_provider_defined_effort_strings() {
        let value = serde_json::json!({
            "providers": {
                "openai": {
                    "endpoint": "http://localhost/chat/completions",
                    "models": {
                        "custom": {
                            "context_window": 128000,
                            "supported_efforts": ["none", "minimal", "provider-custom"]
                        }
                    }
                }
            }
        });

        let config = config_from_value(value).unwrap();
        let efforts = config
            .model_config("openai", "custom")
            .unwrap()
            .supported_efforts
            .as_ref()
            .unwrap();
        assert_eq!(efforts, &["none", "minimal", "provider-custom"]);
    }

    #[test]
    fn redaction_env_accepts_exact_names_and_leading_wildcard_suffixes() {
        let config = config_from_value(serde_json::json!({
            "providers": {
                "openai": {
                    "endpoint": "http://localhost/chat/completions",
                    "models": {"gpt": {}}
                }
            },
            "redaction": {"env": ["GITHUB_TOKEN", "*_TOKEN"]}
        }))
        .unwrap();

        assert_eq!(config.redaction.env, ["GITHUB_TOKEN", "*_TOKEN"]);
    }

    #[test]
    fn redaction_env_rejects_unsupported_wildcards() {
        for (selector, expected) in [
            ("*", "must be followed by a literal suffix"),
            ("**_TOKEN", "exactly one `*` is allowed"),
            ("AWS_*", "only allowed as the first character"),
            ("*TOKEN*", "exactly one `*` is allowed"),
        ] {
            let error = config_from_value(serde_json::json!({
                "providers": {
                    "openai": {
                        "endpoint": "http://localhost/chat/completions",
                        "models": {"gpt": {}}
                    }
                },
                "redaction": {"env": [selector]}
            }))
            .unwrap_err();

            let message = error.to_string();
            assert!(message.contains(selector), "{message}");
            assert!(message.contains(expected), "{message}");
        }
    }

    #[test]
    fn rejects_unsupported_endpoint_paths_before_runtime() {
        let error = config_from_value(serde_json::json!({
            "providers": {
                "openai": {
                    "endpoint": "https://api.openai.com/v1",
                    "models": {"gpt": {}}
                }
            }
        }))
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("must end in `/chat/completions` or `/responses`")
        );
    }

    #[test]
    fn legacy_base_url_fails_with_endpoint_migration_hint() {
        let error = config_from_value(serde_json::json!({
            "providers": {
                "openai": {
                    "base_url": "https://api.openai.com/v1",
                    "models": {"gpt": {}}
                }
            }
        }))
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("missing `endpoint`"));
        assert!(message.contains("complete URL"));
    }

    #[test]
    fn project_order_takes_precedence_over_global_order() {
        let global = ConfigOrder {
            providers: vec!["global-first".into(), "shared".into()],
            models: HashMap::from([
                ("global-first".into(), vec!["g1".into()]),
                (
                    "shared".into(),
                    vec!["global-model".into(), "shared-model".into()],
                ),
            ]),
        };
        let project = ConfigOrder {
            providers: vec!["shared".into(), "project-only".into()],
            models: HashMap::from([
                (
                    "shared".into(),
                    vec!["project-model".into(), "shared-model".into()],
                ),
                ("project-only".into(), vec!["p1".into()]),
            ]),
        };

        let order = combined_config_order(&global, Some(&project));

        assert_eq!(
            order.providers,
            vec!["shared", "project-only", "global-first"]
        );
        assert_eq!(
            order.models["shared"],
            vec!["project-model", "shared-model", "global-model"]
        );
    }

    #[test]
    fn starter_config_is_the_full_global_template() {
        let root = std::env::temp_dir().join(format!("mu-config-{}", uuid::Uuid::new_v4()));
        let config = root.join("config.jsonc");

        ensure_starter_config(&config).unwrap();

        assert_eq!(std::fs::read_to_string(&config).unwrap(), STARTER_CONFIG);
        assert!(!root.join(".gitignore").exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn env_parser_accepts_the_restricted_shell_subset() {
        let entries = parse_env_file(
            r#"
# comment
  # indented comment
BARE=abc_123-./:@%+,==
EMPTY=
SINGLE='hello world $HOME and `ticks`'
DOUBLE="say \"hello\" for \$5 at \\tmp with \`ticks\` and it's fine"
export   EXPORTED='exported value'
"#,
        )
        .unwrap()
        .into_iter()
        .collect::<EnvMap>();

        assert_eq!(
            entries.get("BARE").map(String::as_str),
            Some("abc_123-./:@%+,==")
        );
        assert_eq!(entries.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(
            entries.get("SINGLE").map(String::as_str),
            Some("hello world $HOME and `ticks`")
        );
        assert_eq!(
            entries.get("DOUBLE").map(String::as_str),
            Some("say \"hello\" for $5 at \\tmp with `ticks` and it's fine")
        );
        assert_eq!(
            entries.get("EXPORTED").map(String::as_str),
            Some("exported value")
        );
    }

    #[test]
    fn env_parser_rejects_unsupported_shell_syntax() {
        let cases = [
            ("indented assignment", " NAME=value"),
            ("whitespace before equals", "NAME =value"),
            ("whitespace after equals", "NAME= value"),
            ("trailing whitespace", "NAME=value "),
            ("bare spaces", "NAME=hello world"),
            ("inline comment", "NAME=value # comment"),
            ("mixed quoting", "NAME='one'\"two\""),
            ("parameter expansion", r#"NAME="$HOME""#),
            ("command substitution", r#"NAME="$(id)""#),
            ("backtick substitution", "NAME=\"`id`\""),
            ("unsupported escape", r#"NAME="bad\q""#),
            ("shell escape", r#"NAME=hello\ world"#),
            ("ansi-c quoting", r#"NAME=$'value'"#),
            ("tilde expansion", "NAME=~/bin"),
            ("shell operator", "NAME=value;id"),
            ("unterminated single quote", "NAME='value"),
            ("unterminated double quote", "NAME=\"value"),
            ("multiline value", "NAME='one\ntwo'"),
            ("invalid name", "9NAME=value"),
            ("missing assignment", "export NAME"),
            ("lone carriage return", "NAME=value\r"),
            ("nul", "NAME=value\0suffix"),
        ];

        for (case, source) in cases {
            let error = parse_env_file(source).expect_err(case);
            assert_eq!(error.line, 1, "{case}");
        }
    }

    #[test]
    fn env_parser_accepts_crlf_and_last_duplicate_wins() {
        let entries = parse_env_file("NAME=first\r\nNAME=second")
            .unwrap()
            .into_iter()
            .collect::<EnvMap>();

        assert_eq!(entries.get("NAME").map(String::as_str), Some("second"));
    }

    #[test]
    fn env_file_errors_are_atomic_and_do_not_echo_values() {
        let tmp = std::env::temp_dir().join(format!("mu-env-error-{}", uuid::Uuid::new_v4()));
        let path = tmp.join(".env");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&path, "GOOD=value\nSECRET=$(do-not-print)\n").unwrap();
        let mut env = EnvMap::from([("EXISTING".into(), "kept".into())]);

        let error = load_dotenv_into(&path, &mut env).unwrap_err().to_string();

        assert!(error.contains(&format!("{}:2", path.display())));
        assert!(!error.contains("do-not-print"));
        assert_eq!(env, EnvMap::from([("EXISTING".into(), "kept".into())]));

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn env_file_rejects_invalid_utf8_without_mutation() {
        let tmp = std::env::temp_dir().join(format!("mu-env-utf8-{}", uuid::Uuid::new_v4()));
        let path = tmp.join(".env");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&path, b"NAME=valid\nSECRET=\xff\n").unwrap();
        let mut env = EnvMap::from([("EXISTING".into(), "kept".into())]);

        let error = load_dotenv_into(&path, &mut env).unwrap_err().to_string();

        assert!(error.contains(&format!("reading {}", path.display())));
        assert_eq!(env, EnvMap::from([("EXISTING".into(), "kept".into())]));

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn env_file_values_match_bash_sourcing() {
        let tmp = std::env::temp_dir().join(format!("mu-env-source-{}", uuid::Uuid::new_v4()));
        let path = tmp.join(".env");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(
            &path,
            r#"# source-compatible fixture
MU_TEST_BARE=abc_123-./:@%+,==
MU_TEST_SINGLE='hello world $HOME and `ticks`'
MU_TEST_DOUBLE="say \"hello\" for \$5 at \\tmp with \`ticks\` and it's fine"
export MU_TEST_EXPORTED='exported value'
MU_TEST_EMPTY=
"#,
        )
        .unwrap();

        let mut env = EnvMap::new();
        load_dotenv_into(&path, &mut env).unwrap();
        let names = [
            "MU_TEST_BARE",
            "MU_TEST_SINGLE",
            "MU_TEST_DOUBLE",
            "MU_TEST_EXPORTED",
            "MU_TEST_EMPTY",
        ];
        let output = std::process::Command::new("bash")
            .args([
                "--noprofile",
                "--norc",
                "-c",
                "set -a\n. \"$1\"\nprintf '%s\\0' \"$MU_TEST_BARE\" \"$MU_TEST_SINGLE\" \"$MU_TEST_DOUBLE\" \"$MU_TEST_EXPORTED\" \"$MU_TEST_EMPTY\"",
                "bash",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "bash failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let mut expected = Vec::new();
        for name in names {
            expected.extend_from_slice(env[name].as_bytes());
            expected.push(0);
        }
        assert_eq!(output.stdout, expected);

        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn env_files_overlay_in_order() {
        let tmp = std::env::temp_dir().join(format!("mu-env-{}", uuid::Uuid::new_v4()));
        let global = tmp.join("global");
        let project = tmp.join("project/.mu");
        std::fs::create_dir_all(&global).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(global.join(".env"), "SAME=global\nGLOBAL_ONLY=1\n").unwrap();
        std::fs::write(project.join(".env"), "SAME=project\nPROJECT_ONLY=2\n").unwrap();

        let mut env = EnvMap::new();
        load_dotenv_into(&global.join(".env"), &mut env).unwrap();
        load_dotenv_into(&project.join(".env"), &mut env).unwrap();

        assert_eq!(env.get("SAME").map(String::as_str), Some("project"));
        assert_eq!(env.get("GLOBAL_ONLY").map(String::as_str), Some("1"));
        assert_eq!(env.get("PROJECT_ONLY").map(String::as_str), Some("2"));

        let _ = std::fs::remove_dir_all(tmp);
    }
}
