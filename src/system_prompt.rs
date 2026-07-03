use std::path::Path;

use chrono::Local;

use crate::paths::Project;
use crate::skills::{SkillMeta, format_skills_block, read_agents_md, scan_instruction_index};

const ROLE_PREAMBLE: &str = include_str!("prompts/system_preamble.md");

pub fn role_preamble() -> &'static str {
    ROLE_PREAMBLE.trim_end_matches(['\r', '\n'])
}

pub fn build_system_prompt(
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
    store: Option<&crate::store::Store>,
) -> anyhow::Result<String> {
    let _ = store;
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
mod tests {
    use std::path::{Path, PathBuf};

    use super::{assemble_prompt, cwd_changed_context, initial_environment_context, role_preamble};
    use crate::paths::{Project, ProjectMarker};

    #[test]
    fn role_preamble_explicitly_limits_tools() {
        let prompt = assemble_prompt(&[], Path::new("/tmp/mu-test-global"), None);
        assert!(prompt.starts_with(role_preamble()));
        assert!(prompt.contains("Exactly one tool is available: `bash`."));
        assert!(prompt.contains(
            "Do not invent or call `read`, `write`, `edit`, `fetch`, `search`, `apply_patch`, `view_image`, or any other tool name."
        ));
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
            "session-1",
        );
        assert!(context.contains("project root: /tmp/work"));
        assert!(context.contains("git worktree: yes"));
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
