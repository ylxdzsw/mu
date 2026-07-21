use std::path::Path;

use chrono::Local;

use crate::paths::Project;
use crate::skills::{
    InstructionScope, SkillMeta, format_skills_block, read_agents_md, scan_instruction_index,
};

const ROLE_PREAMBLE: &str = include_str!("system_preamble.md");

/// Preamble emitted by `mu context --export`. It tells a foreign agent (e.g.
/// Claude Code, which has a richer toolset than mu's single `bash`) that the
/// instructions and skills below were authored for mu and should be adapted in
/// intent rather than followed literally. A pointer to the `customize-mu`
/// reference is appended at runtime when that built-in file is present.
const EXPORT_PREAMBLE: &str = "\
<!-- Injected by `mu context --export`. The instructions and skills below are the
user's own mu configuration (global + project); mu's built-in skills are omitted.
They were authored for `mu`, a terminal agent whose only tool is `bash`. Adapt
their intent to your own tools — for example, read a skill file with your
file-reading tool instead of mu's shell.";

const EXPORT_PREAMBLE_CLOSE: &str = " -->";

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

    let os = os_description();
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

/// Build the portable export projection emitted by `mu context --export`.
///
/// Unlike [`build_system_prompt`], this deliberately omits mu's role preamble,
/// the `<runtime>` block, and built-in skills. It emits an explanatory preamble
/// followed by the user's own instructions (global then project `AGENTS.md`) and
/// non-built-in skills, so a foreign agent can ingest the user's mu setup without
/// inheriting mu's `bash`-only framing. Returns an empty string when the user has
/// no `AGENTS.md` and no non-built-in skills, so a `SessionStart` hook injects
/// nothing in a project with no mu configuration.
pub fn build_context(
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
) -> anyhow::Result<String> {
    let index = scan_instruction_index(global_config_dir, project_config_dir)?;
    let user_skills = index
        .skills
        .into_iter()
        .filter(|skill| skill.scope != InstructionScope::Builtin)
        .collect::<Vec<_>>();
    Ok(assemble_context(
        &user_skills,
        global_config_dir,
        project_config_dir,
        &export_preamble(),
    ))
}

/// Assemble the `--export` preamble, appending a pointer to the `customize-mu`
/// reference when that built-in file is present so a foreign agent can read
/// mu's full configuration/skill/command contract on demand.
fn export_preamble() -> String {
    let mut preamble = EXPORT_PREAMBLE.to_string();
    let customize = crate::paths::builtins_dir().join("customize-mu.md");
    if customize.is_file() {
        preamble.push_str(&format!(
            "\nTo understand mu's configuration, skills, and command contract, read {}.",
            customize.display()
        ));
    }
    preamble.push_str(EXPORT_PREAMBLE_CLOSE);
    preamble
}

fn assemble_context(
    skills: &[SkillMeta],
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
    preamble: &str,
) -> String {
    let mut parts = Vec::new();

    if let Some(global) = read_agents_md(&global_config_dir.join("AGENTS.md")) {
        parts.push(global);
    }
    if let Some(project_config_dir) = project_config_dir
        && let Some(local) = read_agents_md(&project_config_dir.join("AGENTS.md"))
    {
        parts.push(local);
    }

    let skills_block = format_skills_block(skills);
    if !skills_block.is_empty() {
        parts.push(skills_block);
    }

    // The preamble only wraps real user content; with nothing to export we emit
    // an empty string so a SessionStart hook injects nothing.
    if parts.is_empty() {
        return String::new();
    }

    parts.insert(0, preamble.to_string());
    parts.join("\n\n")
}

fn os_description() -> String {
    let os = std::env::consts::OS;
    if os != "linux" {
        return os.to_string();
    }

    ["/etc/os-release", "/usr/lib/os-release"]
        .into_iter()
        .find_map(|path| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|contents| linux_distribution(&contents))
        })
        .map_or_else(
            || os.to_string(),
            |distribution| format!("{os} ({distribution})"),
        )
}

fn linux_distribution(os_release: &str) -> Option<String> {
    ["PRETTY_NAME", "NAME", "ID"]
        .into_iter()
        .find_map(|key| os_release_value(os_release, key))
}

fn os_release_value(os_release: &str, key: &str) -> Option<String> {
    os_release.lines().find_map(|line| {
        let (candidate, value) = line.split_once('=')?;
        if candidate != key {
            return None;
        }

        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .or_else(|| {
                value
                    .strip_prefix('\'')
                    .and_then(|value| value.strip_suffix('\''))
            })
            .unwrap_or(value);
        (!value.is_empty()).then(|| value.replace("\\\"", "\"").replace("\\\\", "\\"))
    })
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

    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        EXPORT_PREAMBLE, assemble_context, assemble_prompt, build_context, cwd_changed_context,
        export_preamble, initial_environment_context, linux_distribution, role_preamble,
    };
    use crate::paths::{Project, ProjectMarker};
    use crate::skills::{InstructionScope, SkillMeta, SkillRequirements};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mu-context-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn skill(name: &str, scope: InstructionScope) -> SkillMeta {
        SkillMeta {
            name: name.to_string(),
            description: format!("{name} description"),
            path: format!("/abs/{name}.md"),
            scope,
            requirements: SkillRequirements::default(),
        }
    }

    #[test]
    fn role_preamble_explicitly_limits_tools() {
        let prompt = assemble_prompt(&[], Path::new("/tmp/mu-test-global"), None);
        assert!(prompt.starts_with(role_preamble()));
        assert!(prompt.contains("Exactly one tool is available: `bash`"));
        assert!(prompt.contains("\nuser: "));
        assert!(prompt.contains(" (uid "));
    }

    #[test]
    fn linux_distribution_prefers_pretty_name_with_fallbacks() {
        assert_eq!(
            linux_distribution("NAME=Ubuntu\nPRETTY_NAME=\"Ubuntu 24.04.2 LTS\"\nID=ubuntu"),
            Some("Ubuntu 24.04.2 LTS".into())
        );
        assert_eq!(
            linux_distribution("NAME='Alpine Linux'\nID=alpine"),
            Some("Alpine Linux".into())
        );
        assert_eq!(linux_distribution("ID=arch"), Some("arch".into()));
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
    fn assemble_context_emits_preamble_agents_and_skills_without_role_preamble() {
        let global = temp_dir("assemble-global");
        fs::write(global.join("AGENTS.md"), "Global mu instructions.").unwrap();
        let skills = [skill("brave-search", InstructionScope::Global)];

        let context = assemble_context(&skills, &global, None, EXPORT_PREAMBLE);
        fs::remove_dir_all(&global).unwrap();

        assert!(context.starts_with(EXPORT_PREAMBLE));
        assert!(context.contains("Global mu instructions."));
        assert!(context.contains("<available_skills>"));
        assert!(context.contains("brave-search"));
        assert!(!context.contains(role_preamble()));
        assert!(!context.contains("<runtime>"));
    }

    #[test]
    fn assemble_context_is_empty_without_agents_or_skills() {
        let global = temp_dir("assemble-empty");

        // Nothing is emitted when there is no user content, so a SessionStart
        // hook injects nothing — not even the preamble.
        assert!(assemble_context(&[], &global, None, EXPORT_PREAMBLE).is_empty());
        fs::remove_dir_all(&global).unwrap();
    }

    #[test]
    fn export_preamble_points_at_customize_mu_when_present() {
        let preamble = export_preamble();
        assert!(preamble.starts_with(EXPORT_PREAMBLE));
        assert!(preamble.trim_end().ends_with("-->"));
        // On a packaged or source checkout the built-in reference exists, so the
        // pointer is appended; otherwise the preamble is just opened and closed.
        if crate::paths::builtins_dir().join("customize-mu.md").is_file() {
            assert!(preamble.contains("customize-mu.md"));
            assert!(preamble.contains("mu's configuration, skills, and command contract"));
        }
    }

    #[test]
    fn build_context_excludes_builtin_skills() {
        let global = temp_dir("build-global");
        fs::write(
            global.join("customize-mu.md"),
            "---\nname: customize-mu\ndescription: Customize mu.\n---\nUse config files.\n",
        )
        .unwrap();
        fs::write(
            global.join("brave-search.md"),
            "---\nname: brave-search\ndescription: Web search.\n---\nSearch it.\n",
        )
        .unwrap();

        // build_context scans real builtins; the user's global skill must appear
        // and no built-in skill (e.g. subagent) may leak into the skills index.
        let context = build_context(&global, None).unwrap();
        fs::remove_dir_all(&global).unwrap();

        assert!(context.contains("(path: "));
        assert!(context.contains("brave-search"));
        // `subagent` is a built-in skill; only the preamble's customize-mu
        // pointer may mention a built-in path, never the skills index.
        assert!(!context.contains("subagent"));
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
