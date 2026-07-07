use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::config::{Config, LimitsConfig};
use crate::renderer::Renderer;

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub output: String,
    pub display: ToolDisplay,
}

#[derive(Debug, Clone, Default)]
pub enum ToolDisplay {
    #[default]
    None,
    Bash {
        exit_code: i32,
    },
}

pub struct ToolContext<'a> {
    pub config: &'a Config,
    /// Only sequential tools may write live output. Concurrent tools return
    /// their complete output for ordered rendering by the agent loop.
    pub renderer: Option<&'a mut Renderer>,
    pub state_dir: &'a Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Sequential,
    Concurrent,
}

pub fn bash_tool_definition() -> Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "bash",
            "description": crate::bash::description(),
            "parameters": crate::bash::parameters_schema()
        }
    })
}

pub fn tool_definitions() -> Vec<Value> {
    vec![bash_tool_definition()]
}

pub fn execution_mode(name: &str, args: &Value) -> Option<ExecutionMode> {
    (name == "bash").then(|| crate::bash::execution_mode(args))
}

pub async fn execute_bash_tool(args: Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult> {
    crate::bash::execute(args, ctx).await
}

pub fn resolve_path(path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    }
}

pub fn apply_truncation(
    output: String,
    limits: &LimitsConfig,
    prefix: &str,
    state_dir: &Path,
    use_tail: bool,
) -> Result<ToolResult> {
    let truncated = crate::truncate::truncate_output(&output, limits, prefix, state_dir, use_tail)?;
    Ok(ToolResult {
        output: truncated.text,
        display: ToolDisplay::None,
    })
}

#[derive(Debug, Deserialize)]
pub struct BashArgs {
    pub title: String,
    pub risk: BashRisk,
    pub script: String,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub stdin: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BashRisk {
    Readonly,
    Reversible,
    Destructive,
}

pub fn parse_args<T: for<'de> Deserialize<'de>>(args: &Value) -> Result<T> {
    serde_json::from_value(args.clone()).context("invalid tool arguments")
}

pub fn missing_tool_message(name: &str) -> String {
    format!("unknown tool: {name}")
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::{execution_mode, tool_definitions};
    use crate::config::{CompactionConfig, Config, LimitsConfig, OrderedMap, ProviderConfig};

    fn test_config() -> Config {
        Config {
            providers: OrderedMap::from_iter([(
                "test".into(),
                ProviderConfig {
                    base_url: "http://localhost".into(),
                    api_key_env: "MU_TEST_KEY".into(),
                    models: OrderedMap::default(),
                },
            )]),
            compaction: CompactionConfig::default(),
            limits: LimitsConfig::default(),
            guardrail: crate::config::GuardrailConfig::default(),
            terminal_bell: crate::config::TerminalBellConfig::default(),
            redaction: crate::config::RedactionConfig::default(),
            env: HashMap::new(),
        }
    }

    #[test]
    fn tool_definitions_expose_only_bash() {
        let _config = test_config();
        let definitions = tool_definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0]["function"]["name"].as_str(), Some("bash"));
        assert!(execution_mode("bash", &json!({"risk": "readonly"})).is_some());
    }

    #[test]
    fn bash_schema_requires_title_risk_and_script() {
        let schema = crate::bash::parameters_schema();
        assert_eq!(schema["required"], json!(["title", "risk", "script"]));
        assert_eq!(
            schema["properties"]["risk"]["enum"],
            json!(["readonly", "reversible", "destructive"])
        );
        assert!(schema["properties"].get("command").is_none());
        assert!(schema["properties"].get("workdir").is_none());
        assert!(schema["properties"].get("cwd").is_some());
    }
}
