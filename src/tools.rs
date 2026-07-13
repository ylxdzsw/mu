use std::fmt;
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
    let output = truncate_output(&output, limits, prefix, state_dir, use_tail)?;
    Ok(ToolResult {
        output,
        display: ToolDisplay::None,
    })
}

fn truncate_output(
    output: &str,
    limits: &LimitsConfig,
    spill_prefix: &str,
    state_dir: &Path,
    use_tail: bool,
) -> Result<String> {
    let lines: Vec<&str> = output.lines().collect();
    let total_lines = lines.len();

    if total_lines <= limits.max_lines
        && output.len() <= limits.max_bytes
        && lines.iter().all(|line| line.len() <= limits.max_line_bytes)
    {
        return Ok(output.to_string());
    }

    crate::paths::ensure_dir(&state_dir.join("truncation"))?;
    let spill_path =
        state_dir
            .join("truncation")
            .join(format!("{}-{}.txt", spill_prefix, uuid::Uuid::new_v4()));
    std::fs::write(&spill_path, output)?;

    let preview = if use_tail {
        build_tail_preview(
            &lines,
            limits.max_lines,
            limits.max_bytes,
            limits.max_line_bytes,
        )
    } else {
        build_head_preview(
            &lines,
            limits.max_lines,
            limits.max_bytes,
            limits.max_line_bytes,
        )
    };

    let elided_lines = total_lines.saturating_sub(preview.lines().count());
    let marker = format!(
        "\n[… {elided_lines} lines elided; full output saved to {}; inspect it with `bash` if needed]",
        spill_path.display()
    );
    Ok(format!("{preview}{marker}"))
}

fn build_head_preview(
    lines: &[&str],
    max_lines: usize,
    max_bytes: usize,
    max_line_bytes: usize,
) -> String {
    let mut out = String::new();
    for (count, line) in lines.iter().enumerate() {
        if count >= max_lines {
            break;
        }
        let truncated_line = truncate_line(line, max_line_bytes);
        if out.len() + truncated_line.len() + 1 > max_bytes {
            break;
        }
        if count > 0 {
            out.push('\n');
        }
        out.push_str(&truncated_line);
    }
    out
}

fn build_tail_preview(
    lines: &[&str],
    max_lines: usize,
    max_bytes: usize,
    max_line_bytes: usize,
) -> String {
    let start = lines.len().saturating_sub(max_lines);
    let mut out = String::new();
    for line in &lines[start..] {
        let truncated_line = truncate_line(line, max_line_bytes);
        if out.len() + truncated_line.len() + 1 > max_bytes {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&truncated_line);
    }
    out
}

fn truncate_line(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_string();
    }
    let budget = max_bytes.saturating_sub(3);
    let mut end = budget.min(line.len());
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

pub fn prune_truncation_spills(state_dir: &Path, retention_days: u64) {
    let dir = state_dir.join("truncation");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(retention_days * 24 * 3600);
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata()
            && let Ok(modified) = meta.modified()
            && modified < cutoff
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct BashArgs {
    pub title: String,
    pub risk: BashRisk,
    pub command: String,
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

impl BashRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Readonly => "readonly",
            Self::Reversible => "reversible",
            Self::Destructive => "destructive",
        }
    }

    pub fn from_value(value: &Value) -> Option<Self> {
        value.get("risk")?.as_str()?.parse().ok()
    }

    pub fn from_args_json(args: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(args).ok()?;
        Self::from_value(&value)
    }
}

impl std::str::FromStr for BashRisk {
    type Err = ();

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        match value {
            "readonly" => Ok(Self::Readonly),
            "reversible" => Ok(Self::Reversible),
            "destructive" => Ok(Self::Destructive),
            _ => Err(()),
        }
    }
}

impl fmt::Display for BashRisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
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

    use super::{execution_mode, tool_definitions, truncate_line};
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
    fn bash_schema_requires_title_risk_and_command() {
        let schema = crate::bash::parameters_schema();
        assert_eq!(schema["required"], json!(["title", "risk", "command"]));
        assert_eq!(
            schema["properties"]["risk"]["enum"],
            json!(["readonly", "reversible", "destructive"])
        );
        assert!(schema["properties"].get("command").is_some());
        assert!(schema["properties"].get("script").is_none());
        assert!(schema["properties"].get("workdir").is_none());
        assert!(schema["properties"].get("cwd").is_some());
    }

    #[test]
    fn truncate_line_respects_char_boundaries() {
        let line = "héllo wörld ".repeat(20);
        let out = truncate_line(&line, 25);
        assert!(out.ends_with('…'));
        assert!(out.len() <= 25 + '…'.len_utf8());
    }
}
