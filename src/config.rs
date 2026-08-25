use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::OutputFormat;
use crate::paths;

pub type EnvMap = HashMap<String, String>;

pub fn load_effective_env(project_config_dir: Option<&Path>) -> Result<EnvMap> {
    load_effective_env_from(&paths::global_dir(), project_config_dir)
}

fn load_effective_env_from(
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
) -> Result<EnvMap> {
    let mut env: EnvMap = std::env::vars().collect();
    load_dotenv_into(&global_config_dir.join(".env"), &mut env)?;
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
    pub providers: OrderedMap<ProviderConfig>,
    pub output: OutputFormat,
    pub trap: crate::bash::TrapLevel,
    pub auto_resume: bool,
    pub soft_interrupt: bool,
    pub compaction: CompactionConfig,
    pub limits: LimitsConfig,
    pub terminal_bell: TerminalBellConfig,
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
    #[serde(default)]
    pub replay_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompactionConfig {
    pub enabled: bool,
    pub soft_fraction: f64,
    pub hard_fraction: f64,
    pub hard_headroom_tokens: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LimitsConfig {
    pub max_lines: usize,
    pub max_bytes: usize,
    pub max_line_bytes: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TerminalBellConfig {
    pub enabled: bool,
    pub min_duration_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RedactionConfig {
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLoadMode {
    Runtime,
    Permissive,
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

impl Config {
    pub fn load_for_scope(project_config_dir: Option<&Path>, mode: ConfigLoadMode) -> Result<Self> {
        load_config(&paths::global_dir(), project_config_dir, mode)
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
        if self.compaction.hard_headroom_tokens == 0 {
            bail!("`compaction.hard_headroom_tokens` must be greater than zero");
        }
        for (name, fraction) in [
            ("soft_fraction", self.compaction.soft_fraction),
            ("hard_fraction", self.compaction.hard_fraction),
        ] {
            if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) || fraction == 0.0 {
                bail!("`compaction.{name}` must be greater than zero and at most one");
            }
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
            for (model_id, model) in &provider.models {
                if model_id.contains(':') {
                    bail!("model id `{model_id}` contains reserved `:` in config.jsonc");
                }
                if model
                    .replay_key
                    .as_deref()
                    .is_some_and(|key| key.trim().is_empty())
                {
                    bail!(
                        "model `{provider_id}/{model_id}` has an empty `replay_key` in config.jsonc"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn validate_runtime(&self) -> Result<()> {
        self.validate_structure()?;
        crate::models::validate_config(self)
    }

    pub fn replay_key(&self, provider_id: &str, model_id: &str) -> String {
        self.model_config(provider_id, model_id)
            .and_then(|model| model.replay_key.clone())
            .unwrap_or_else(|| format!("{provider_id}/{model_id}"))
    }
}

fn load_config(
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
    mode: ConfigLoadMode,
) -> Result<Config> {
    let global_path = global_config_dir.join("config.jsonc");
    if mode == ConfigLoadMode::Runtime {
        ensure_starter_config(&global_path)?;
    }
    let (mut value, global_order) = if global_path.exists() {
        read_config_file(&global_path, mode)?
    } else {
        (serde_json::json!({}), ConfigOrder::default())
    };
    let mut order = combined_config_order(&global_order, None);

    if let Some(dir) = project_config_dir {
        let project_path = dir.join("config.jsonc");
        if project_path.exists() {
            let (project, project_order) = read_config_file(&project_path, mode)?;
            merge_json(&mut value, project);
            order = combined_config_order(&global_order, Some(&project_order));
        }
    }

    let mut config = match mode {
        ConfigLoadMode::Runtime => deserialize_config(value, false)?,
        ConfigLoadMode::Permissive => deserialize_config(value, true)?,
    };
    apply_config_order(&mut config, &order);
    if mode == ConfigLoadMode::Runtime {
        config.env = load_effective_env_from(global_config_dir, project_config_dir)?;
        config.validate_runtime()?;
    }
    Ok(config)
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
    std::fs::write(path, DEFAULT_CONFIG)?;
    Ok(())
}

#[cfg(test)]
fn config_from_value(value: serde_json::Value) -> Result<Config> {
    let config = deserialize_config(value, false)?;
    config.validate_structure()?;
    Ok(config)
}

fn deserialize_config(value: serde_json::Value, discard_invalid_providers: bool) -> Result<Config> {
    let mut merged = bundled_defaults()?;
    merge_json(&mut merged, value);
    if !discard_invalid_providers {
        return serde_json::from_value(merged).context("invalid config.jsonc structure");
    }
    match serde_json::from_value(merged.clone()) {
        Ok(config) => Ok(config),
        Err(_) => {
            merged
                .as_object_mut()
                .context("config.jsonc must be an object")?
                .insert("providers".into(), serde_json::json!({}));
            serde_json::from_value(merged).context("invalid config.jsonc structure")
        }
    }
}

fn read_config_file(path: &Path, mode: ConfigLoadMode) -> Result<(serde_json::Value, ConfigOrder)> {
    if !path.exists() {
        bail!("config not found at {}", path.display());
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_config_source(&raw, &path.display().to_string(), mode)
}

fn parse_config_source(
    raw: &str,
    source: &str,
    mode: ConfigLoadMode,
) -> Result<(serde_json::Value, ConfigOrder)> {
    let value =
        jsonc_parser::parse_to_serde_value::<Option<serde_json::Value>>(raw, &Default::default())
            .map_err(|e| anyhow::anyhow!("parsing {source}: {e}"))?
            .ok_or_else(|| anyhow::anyhow!("{source} is empty"))?;
    let order = match jsonc_parser::parse_to_serde_value::<Option<ConfigOrderRaw>>(
        raw,
        &Default::default(),
    ) {
        Ok(order) => order.unwrap_or_default().into_order(),
        Err(_) if mode == ConfigLoadMode::Permissive => ConfigOrder::default(),
        Err(error) => return Err(anyhow::anyhow!("parsing {source}: {error}")),
    };
    Ok((value, order))
}

fn bundled_defaults() -> Result<serde_json::Value> {
    let (mut value, _) = parse_config_source(
        DEFAULT_CONFIG,
        "bundled default config",
        ConfigLoadMode::Runtime,
    )?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("bundled default config must be a JSON object"))?;
    object.insert("providers".into(), serde_json::json!({}));
    Ok(value)
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

const DEFAULT_CONFIG: &str = include_str!("default_config.jsonc");

#[cfg(test)]
pub(crate) fn bundled_test_default<T>(pointer: &str) -> T
where
    T: serde::de::DeserializeOwned,
{
    let (value, _) = parse_config_source(
        DEFAULT_CONFIG,
        "bundled default config",
        ConfigLoadMode::Runtime,
    )
    .expect("valid defaults");
    serde_json::from_value(
        value
            .pointer(pointer)
            .unwrap_or_else(|| panic!("missing bundled default at {pointer}"))
            .clone(),
    )
    .unwrap_or_else(|error| panic!("invalid bundled default at {pointer}: {error}"))
}

#[cfg(test)]
impl Default for CompactionConfig {
    fn default() -> Self {
        bundled_test_default("/compaction")
    }
}

#[cfg(test)]
impl Default for LimitsConfig {
    fn default() -> Self {
        bundled_test_default("/limits")
    }
}

#[cfg(test)]
impl Default for TerminalBellConfig {
    fn default() -> Self {
        bundled_test_default("/terminal_bell")
    }
}

#[cfg(test)]
impl Default for RedactionConfig {
    fn default() -> Self {
        bundled_test_default("/redaction")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn config_overlays_are_recursive_without_injecting_starter_providers() {
        let mut user = serde_json::json!({
            "providers": {
                "custom": {
                    "endpoint": "http://localhost/chat/completions",
                    "models": {"model": {"context_window": 128000}}
                }
            },
            "limits": {"max_lines": 123},
            "compaction": {"enabled": false},
            "redaction": {"env": ["*_TOKEN"]}
        });
        let project = serde_json::json!({
            "limits": {"max_bytes": 456},
            "redaction": {"env": []}
        });
        merge_json(&mut user, project);

        let config = config_from_value(user).unwrap();
        assert_eq!(
            config
                .providers
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["custom"]
        );
        assert_eq!(config.limits.max_lines, 123);
        assert_eq!(config.limits.max_bytes, 456);
        assert!(!config.compaction.enabled);
        assert!(config.redaction.env.is_empty());
    }

    #[test]
    fn numeric_limits_reject_invalid_values() {
        let provider = serde_json::json!({
            "openai": {
                "endpoint": "http://localhost/chat/completions",
                "models": {"gpt-4o": {"context_window": 128000}}
            }
        });
        for invalid in [
            serde_json::json!({"compaction": {"hard_headroom_tokens": 0}}),
            serde_json::json!({"compaction": {"soft_fraction": 0.0}}),
            serde_json::json!({"compaction": {"hard_fraction": 1.01}}),
        ] {
            let mut value = serde_json::json!({"providers": provider.clone()});
            merge_json(&mut value, invalid);
            assert!(config_from_value(value).is_err());
        }
    }

    #[test]
    fn model_ids_cannot_contain_colons() {
        assert!(
            config_from_value(serde_json::json!({
                "providers": {
                    "openai": {
                        "endpoint": "http://localhost/chat/completions",
                        "models": {"version:latest": {"context_window": 128000}}
                    }
                }
            }))
            .is_err()
        );
    }

    #[test]
    fn replay_key_is_optional_dynamic_model_metadata() {
        let config = config_from_value(serde_json::json!({
            "providers": {
                "alpha": {
                    "endpoint": "https://alpha.test/chat/completions",
                    "models": {
                        "shared": {
                            "context_window": 128000,
                            "replay_key": "compatible-family"
                        },
                        "defaulted": {
                            "context_window": 128000
                        }
                    }
                }
            }
        }))
        .unwrap();

        assert_eq!(config.replay_key("alpha", "shared"), "compatible-family");
        assert_eq!(config.replay_key("alpha", "defaulted"), "alpha/defaulted");

        assert!(
            config_from_value(serde_json::json!({
                "providers": {
                    "alpha": {
                        "endpoint": "https://alpha.test/chat/completions",
                        "models": {
                            "invalid": {
                                "context_window": 128000,
                                "replay_key": " "
                            }
                        }
                    }
                }
            }))
            .is_err()
        );
    }

    #[test]
    fn redaction_env_selector_grammar() {
        assert_eq!(redaction_suffix("GITHUB_TOKEN").unwrap(), None);
        assert_eq!(redaction_suffix("*_TOKEN").unwrap(), Some("_TOKEN"));
        for selector in ["*", "**_TOKEN", "AWS_*", "*TOKEN*"] {
            assert!(redaction_suffix(selector).is_err());
        }
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
    fn permissive_load_does_not_create_a_missing_global_config() {
        let root =
            std::env::temp_dir().join(format!("mu-config-readonly-{}", uuid::Uuid::new_v4()));

        let config = load_config(&root, None, ConfigLoadMode::Permissive).unwrap();

        assert!(config.providers.is_empty());
        assert!(!root.exists());
    }

    #[test]
    fn permissive_load_discards_malformed_providers_but_keeps_output() {
        let root =
            std::env::temp_dir().join(format!("mu-config-permissive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("config.jsonc"),
            r#"{"output":"full","providers":{"broken":{"models":[]}}}"#,
        )
        .unwrap();
        std::fs::write(root.join(".env"), "INVALID=$(ignored)\n").unwrap();

        let config = load_config(&root, None, ConfigLoadMode::Permissive).unwrap();

        assert_eq!(config.output, OutputFormat::Full);
        assert!(config.providers.is_empty());
        assert!(config.env.is_empty());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn permissive_load_keeps_model_metadata_without_provider_validation() {
        let root =
            std::env::temp_dir().join(format!("mu-config-metadata-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("config.jsonc"),
            r#"{
                "providers": {
                    "broken": {
                        "endpoint": "not a provider endpoint",
                        "models": {"model": {"context_window": 123}}
                    }
                }
            }"#,
        )
        .unwrap();

        let config = load_config(&root, None, ConfigLoadMode::Permissive).unwrap();

        assert_eq!(
            config
                .model_config("broken", "model")
                .unwrap()
                .context_window,
            Some(123)
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn env_parser_accepts_the_restricted_shell_subset() {
        let entries = parse_env_file(concat!(
            r#"
# comment
  # indented comment
BARE=abc_123-./:@%+,==
EMPTY=
SINGLE='hello world $HOME and `ticks`'
DOUBLE="say \"hello\" for \$5 at \\tmp with \`ticks\` and it's fine"
export   EXPORTED='exported value'
"#,
            "DUP=first\r\nDUP=second",
        ))
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
        assert_eq!(entries.get("DUP").map(String::as_str), Some("second"));
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
    fn env_file_errors_are_atomic_and_do_not_echo_values() {
        let tmp = std::env::temp_dir().join(format!("mu-env-error-{}", uuid::Uuid::new_v4()));
        let path = tmp.join(".env");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(&path, "GOOD=value\nSECRET=$(do-not-print)\n").unwrap();
        let baseline = EnvMap::from([("EXISTING".into(), "kept".into())]);
        let mut env = baseline.clone();

        let error = load_dotenv_into(&path, &mut env).unwrap_err().to_string();

        assert!(!error.contains("do-not-print"));
        assert_eq!(env, baseline);

        std::fs::write(&path, b"NAME=valid\nSECRET=\xff\n").unwrap();
        let mut env = baseline.clone();

        assert!(load_dotenv_into(&path, &mut env).is_err());
        assert_eq!(env, baseline);

        let _ = std::fs::remove_dir_all(tmp);
    }
}
