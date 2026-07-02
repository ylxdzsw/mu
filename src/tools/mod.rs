use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use crate::config::{Config, LimitsConfig};
use crate::renderer::Renderer;

pub mod bash;
pub mod truncate;

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

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    fn parameters_schema(&self) -> Value;
    fn execution_mode(&self, _args: &Value) -> ExecutionMode {
        ExecutionMode::Sequential
    }
    async fn execute(&self, args: Value, ctx: &mut ToolContext<'_>) -> Result<ToolResult>;
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new(config: &Config) -> Self {
        let _ = config;
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(bash::BashTool)];
        Self { tools }
    }

    pub fn definitions(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name(),
                        "description": t.description(),
                        "parameters": t.parameters_schema()
                    }
                })
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }
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
    let truncated = truncate::truncate_output(&output, limits, prefix, state_dir, use_tail)?;
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
mod tests;
