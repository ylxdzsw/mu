use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

use crate::config::{Config, LimitsConfig};
use crate::provider::ToolArtifact;
use crate::renderer::Renderer;

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub output: String,
    pub exit_code: i32,
    pub artifacts: Vec<ToolArtifact>,
}

pub struct ToolContext<'a> {
    pub config: &'a Config,
    pub renderer: &'a mut Renderer,
    pub state_dir: &'a Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionMode {
    Sequential,
    Concurrent,
}

/// How long truncation spill files are kept before `prune_truncation_spills`
/// removes them. Surfaced in the truncation marker so the agent knows a spill
/// path from an old, resumed session may no longer exist.
pub const SPILL_RETENTION_DAYS: u64 = 7;

pub fn tool_definitions() -> Vec<Value> {
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "bash",
            "description": crate::bash::description(),
            "parameters": crate::bash::parameters_schema(),
            "strict": false
        }
    })]
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
) -> String {
    truncate_output(&output, limits, prefix, state_dir, use_tail)
}

fn truncate_output(
    output: &str,
    limits: &LimitsConfig,
    spill_prefix: &str,
    state_dir: &Path,
    use_tail: bool,
) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let total_lines = lines.len();

    if total_lines <= limits.max_lines
        && output.len() <= limits.max_bytes
        && lines.iter().all(|line| line.len() <= limits.max_line_bytes)
    {
        return output.to_string();
    }

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

    // The spill is best-effort: by this point the command has already run, so
    // a failure to save the full output (read-only state dir, disk full) must
    // degrade to a preview-only note, never fail the tool result.
    let spill_note = match write_spill(output, spill_prefix, state_dir) {
        Ok(spill_path) => format!(
            "full output saved for {SPILL_RETENTION_DAYS} days to {}; inspect it with `bash` if it still exists",
            spill_path.display()
        ),
        Err(error) => {
            format!("full output could not be saved ({error}); only this preview is available")
        }
    };

    let elided_lines = total_lines.saturating_sub(preview.lines().count());
    format!("{preview}\n[… {elided_lines} lines elided; {spill_note}]")
}

fn write_spill(output: &str, spill_prefix: &str, state_dir: &Path) -> Result<PathBuf> {
    let dir = state_dir.join("truncation");
    crate::paths::ensure_dir(&dir)?;
    let spill_path = dir.join(format!("{}-{}.txt", spill_prefix, uuid::Uuid::new_v4()));
    std::fs::write(&spill_path, output)?;
    Ok(spill_path)
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
    let line_cap = max_line_bytes.min(max_bytes);
    let mut selected = Vec::new();
    let mut used_bytes = 0;
    for line in lines[start..].iter().rev() {
        let truncated_line = truncate_line(line, line_cap);
        let separator_bytes = usize::from(!selected.is_empty());
        if used_bytes + separator_bytes + truncated_line.len() > max_bytes {
            break;
        }
        used_bytes += separator_bytes + truncated_line.len();
        selected.push(truncated_line);
    }
    selected.reverse();
    selected.join("\n")
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{build_tail_preview, tool_definitions, truncate_line};

    #[test]
    fn tool_definitions_expose_only_bash() {
        let definitions = tool_definitions();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0]["function"]["name"].as_str(), Some("bash"));
        assert_eq!(definitions[0]["function"]["strict"], json!(false));
    }

    #[test]
    fn bash_schema_requires_title_risk_and_command() {
        let schema = crate::bash::parameters_schema();
        assert_eq!(schema["required"], json!(["title", "risk", "command"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["properties"]["risk"]["enum"],
            json!(["readonly", "reversible", "destructive"])
        );
        assert!(schema["properties"].get("command").is_some());
        assert!(schema["properties"].get("script").is_none());
        assert!(schema["properties"].get("workdir").is_none());
        assert!(schema["properties"].get("cwd").is_some());
        assert!(
            schema["properties"]["stdin"]["description"]
                .as_str()
                .is_some_and(|description| description.contains("omit unless"))
        );
    }

    #[test]
    fn truncate_line_respects_char_boundaries() {
        let line = "héllo wörld ".repeat(20);
        let out = truncate_line(&line, 25);
        assert!(out.ends_with('…'));
        assert!(out.len() <= 25 + '…'.len_utf8());
    }

    #[test]
    fn tail_preview_preserves_actual_tail_when_byte_limited() {
        let lines = vec![
            "first output line",
            "second output line",
            "third output line",
            "[exit code: 7]",
        ];

        let preview = build_tail_preview(&lines, 10, 32, 1024);

        assert!(preview.ends_with("[exit code: 7]"));
        assert!(!preview.contains("first output line"));
        assert!(preview.len() <= 32);
    }

    #[test]
    fn tail_preview_preserves_actual_tail_when_line_limited() {
        let lines = vec!["one", "two", "three", "[exit code: 0]"];

        let preview = build_tail_preview(&lines, 2, 1024, 1024);

        assert_eq!(preview, "three\n[exit code: 0]");
    }

    fn tight_limits() -> crate::config::LimitsConfig {
        crate::config::LimitsConfig {
            max_iterations: 50,
            max_lines: 2,
            max_bytes: 10_000,
            max_line_bytes: 10_000,
        }
    }

    #[test]
    fn truncation_spills_full_output_and_names_the_retention_window() {
        let tmp = std::env::temp_dir().join(format!("mu-trunc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();

        let clamped = super::apply_truncation(
            "one\ntwo\nthree\nfour".into(),
            &tight_limits(),
            "bash",
            &tmp,
            true,
        );

        assert!(clamped.contains("lines elided"));
        assert!(clamped.contains(&format!(
            "full output saved for {} days",
            super::SPILL_RETENTION_DAYS
        )));
        let spilled: Vec<_> = std::fs::read_dir(tmp.join("truncation")).unwrap().collect();
        assert_eq!(spilled.len(), 1);
        let _ = std::fs::remove_dir_all(tmp);
    }

    #[test]
    fn truncation_survives_an_unwritable_spill_directory() {
        let tmp = std::env::temp_dir().join(format!("mu-trunc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&tmp).unwrap();
        // Occupy the spill directory's path with a regular file so it cannot
        // be created: the command already ran, so the clamped preview must
        // come back anyway instead of an error.
        std::fs::write(tmp.join("truncation"), b"not a directory").unwrap();

        let clamped = super::apply_truncation(
            "one\ntwo\nthree\nfour".into(),
            &tight_limits(),
            "bash",
            &tmp,
            true,
        );

        assert!(clamped.contains("three\nfour"));
        assert!(clamped.contains("full output could not be saved"));
        let _ = std::fs::remove_dir_all(tmp);
    }
}
