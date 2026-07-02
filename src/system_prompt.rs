use std::path::Path;

use chrono::Local;

use crate::paths::Project;
use crate::skills::{SkillMeta, format_skills_block, read_agents_md, scan_skills};

pub const ROLE_PREAMBLE: &str = "You are mu, a terminal agent. Exactly one tool is available: `bash`. Do not invent or call `read`, `write`, `edit`, `fetch`, `search`, `apply_patch`, `view_image`, or any other tool name. If a skill or `AGENTS.md` mentions another tool, treat it as historical shorthand and accomplish the task with `bash` and ordinary CLI programs instead. Use `bash` for local search, file reads, writes, edits, web fetches, tests, and any other CLI work. Each bash call is isolated: pass `cwd` explicitly when needed, and do not expect `cd` or environment changes to persist. Include a short `title`, an advisory `risk` label, and the `script`. Keep responses concise.";

pub fn build_system_prompt(
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
    store: Option<&crate::store::Store>,
) -> anyhow::Result<String> {
    let mut skills = scan_skills(global_config_dir, store)?;
    if let Some(project_config_dir) = project_config_dir {
        skills.extend(scan_skills(project_config_dir, store)?);
    }
    Ok(assemble_prompt(
        &skills,
        global_config_dir,
        project_config_dir,
    ))
}

pub fn assemble_prompt(
    skills: &[SkillMeta],
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
) -> String {
    let mut parts = vec![ROLE_PREAMBLE.to_string()];

    let os = std::env::consts::OS;
    let date = Local::now().format("%Y-%m-%d").to_string();
    parts.push(format!("<runtime>\nos: {}\ndate: {}\n</runtime>", os, date));

    let skills_block = format_skills_block(skills);
    if !skills_block.is_empty() {
        parts.push(skills_block);
    }

    if let Some(global) = read_agents_md(&global_config_dir.join("AGENTS.md")) {
        parts.push(global);
    }
    if let Some(project_config_dir) = project_config_dir
        && let Some(local) = read_agents_md(&project_config_dir.join("AGENTS.md"))
    {
        parts.push(local);
    }

    parts.join("\n\n")
}

pub fn initial_environment_context(
    cwd: &Path,
    project: Option<&Project>,
    session_id: &str,
) -> String {
    let mut lines = vec![
        "[environment]".to_string(),
        format!("current working directory: {}", cwd.display()),
        format!("active session id: {session_id}"),
    ];

    if let Some(project) = project {
        lines.push(format!("project root: {}", project.root.display()));
        if let Some(worktree) = &project.worktree {
            lines.push(format!("git dir: {}", worktree.git_dir.display()));
            if let Some(common_dir) = &worktree.common_dir {
                lines.push(format!("git common dir: {}", common_dir.display()));
                lines.push("git worktree: yes".into());
            }
        }
    }

    lines.join("\n")
}

pub fn cwd_changed_context(cwd: &Path) -> String {
    format!(
        "<system-reminder>\ncurrent working directory changed to: {}\n</system-reminder>",
        cwd.display()
    )
}

#[cfg(test)]
#[path = "system_prompt_tests.rs"]
mod tests;
