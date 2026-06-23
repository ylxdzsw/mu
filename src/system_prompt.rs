use std::path::Path;

use chrono::Local;

use crate::skills::{format_skills_block, read_agents_md, scan_skills, SkillMeta};

pub const ROLE_PREAMBLE: &str = "You are mu, a terminal agent. You execute the user's request using the available `bash` tool, then stop. Use `bash` for local search, file reads, writes, edits, web fetches, tests, and any other CLI work. Each bash call is isolated: pass `cwd` explicitly when needed, and do not expect `cd` or environment changes to persist. Include a short `title`, an advisory `risk` label, and the `script`. Keep responses concise.";

pub fn build_system_prompt(
    config_dir: &Path,
    cwd: &Path,
    store: Option<&crate::store::Store>,
) -> anyhow::Result<String> {
    let skills = scan_skills(config_dir, store)?;
    Ok(assemble_prompt(&skills, config_dir, cwd))
}

pub fn assemble_prompt(skills: &[SkillMeta], config_dir: &Path, cwd: &Path) -> String {
    let mut parts = vec![ROLE_PREAMBLE.to_string()];

    let os = std::env::consts::OS;
    let date = Local::now().format("%Y-%m-%d").to_string();
    parts.push(format!(
        "<env>\ncwd: {}\nos: {}\ndate: {}\n</env>",
        cwd.display(),
        os,
        date
    ));

    let skills_block = format_skills_block(skills);
    if !skills_block.is_empty() {
        parts.push(skills_block);
    }

    if let Some(global) = read_agents_md(&config_dir.join("AGENTS.md")) {
        parts.push(global);
    }
    if let Some(local) = read_agents_md(&cwd.join("AGENTS.md")) {
        parts.push(local);
    }

    parts.join("\n\n")
}
