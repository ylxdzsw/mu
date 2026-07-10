use std::path::Path;

use chrono::Local;

use crate::paths::Project;
use crate::skills::{SkillMeta, format_skills_block, read_agents_md, scan_instruction_index};

const ROLE_PREAMBLE: &str = include_str!("system_preamble.md");

pub fn role_preamble() -> &'static str {
    ROLE_PREAMBLE.trim_end_matches(['\r', '\n'])
}

pub fn build_system_prompt(
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
) -> anyhow::Result<String> {
    let index = scan_instruction_index(global_config_dir, project_config_dir)?;
    Ok(assemble_prompt(
        &index.skills,
        global_config_dir,
        project_config_dir,
    ))
}

pub fn assemble_prompt(
    skills: &[SkillMeta],
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
) -> String {
    let mut parts = vec![role_preamble().to_string()];

    let os = std::env::consts::OS;
    let date = Local::now().format("%Y-%m-%d").to_string();
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let uid = unsafe { libc::geteuid() };
    parts.push(format!(
        "<runtime>\nos: {}\ndate: {}\nuser: {} (uid {})\n</runtime>",
        os, date, user, uid
    ));

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

pub fn initial_environment_context(cwd: &Path, project: Option<&Project>) -> String {
    let mut lines = vec!["[environment]".to_string()];

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

    lines.push(format!("current working directory: {}", cwd.display()));

    lines.join("\n")
}

pub fn cwd_changed_context(cwd: &Path) -> String {
    format!(
        "<system-reminder>\ncurrent working directory changed to: {}\n</system-reminder>",
        cwd.display()
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{assemble_prompt, cwd_changed_context, initial_environment_context, role_preamble};
    use crate::paths::{Project, ProjectMarker};

    #[test]
    fn role_preamble_explicitly_limits_tools() {
        let prompt = assemble_prompt(&[], Path::new("/tmp/mu-test-global"), None);
        assert!(prompt.starts_with(role_preamble()));
        assert!(prompt.contains("Exactly one tool is available: `bash`."));
        assert!(prompt.contains("Do not invent or call other tool name."));
        assert!(prompt.contains("\nuser: "));
        assert!(prompt.contains(" (uid "));
    }

    #[test]
    fn initial_environment_context_reports_project_and_worktree_metadata() {
        let context = initial_environment_context(
            Path::new("/tmp/work/subdir"),
            Some(&Project {
                root: PathBuf::from("/tmp/work"),
                marker: ProjectMarker::Git,
                worktree: Some(crate::paths::GitWorktreeInfo {
                    git_dir: PathBuf::from("/tmp/repo/.git/worktrees/feature"),
                    common_dir: Some(PathBuf::from("/tmp/repo/.git")),
                }),
            }),
        );
        assert!(context.contains("project root: /tmp/work"));
        assert!(context.contains("git worktree: yes"));
        assert!(context.contains("current working directory: /tmp/work/subdir"));
        assert!(!context.contains("active session id"));
    }

    #[test]
    fn cwd_changed_context_is_wrapped_in_system_reminder() {
        let context = cwd_changed_context(Path::new("/tmp/next"));
        assert_eq!(
            context,
            "<system-reminder>\ncurrent working directory changed to: /tmp/next\n</system-reminder>"
        );
    }
}
