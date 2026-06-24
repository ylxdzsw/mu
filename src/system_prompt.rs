use std::path::Path;

use chrono::Local;

use crate::paths::Project;
use crate::skills::{format_skills_block, read_agents_md, scan_skills, SkillMeta};

pub const ROLE_PREAMBLE: &str = "You are mu, a terminal agent. You execute the user's request using the available `bash` tool, then stop. Use `bash` for local search, file reads, writes, edits, web fetches, tests, and any other CLI work. Each bash call is isolated: pass `cwd` explicitly when needed, and do not expect `cd` or environment changes to persist. Include a short `title`, an advisory `risk` label, and the `script`. Keep responses concise.";

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
    if let Some(project_config_dir) = project_config_dir {
        if let Some(local) = read_agents_md(&project_config_dir.join("AGENTS.md")) {
            parts.push(local);
        }
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

    match project {
        Some(project) => {
            lines.push("project active: yes".into());
            lines.push(format!("project root: {}", project.root.display()));
            if let Some(worktree) = &project.worktree {
                lines.push(format!("git dir: {}", worktree.git_dir.display()));
                if let Some(common_dir) = &worktree.common_dir {
                    lines.push(format!("git common dir: {}", common_dir.display()));
                    lines.push("git worktree: yes".into());
                }
            }
        }
        None => lines.push("project active: no".into()),
    }

    lines.join("\n")
}

pub fn cwd_changed_context(cwd: &Path) -> String {
    format!(
        "[environment update]\ncurrent working directory changed to: {}",
        cwd.display()
    )
}
