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
file-reading tool instead of mu's shell. Mu `.env` files may contain API keys or
other secrets.";

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
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());
    parts.push(format!(
        "<runtime>\nos: {}\ndate: {}\nuser: {}\n</runtime>",
        os, date, user
    ));

    let skills_block = format_skills_block(skills);
    if !skills_block.is_empty() {
        parts.push(skills_block);
    }

    if let Some(global) = agents_md_block(&global_config_dir.join("AGENTS.md"), "global") {
        parts.push(global);
    }
    if let Some(project_config_dir) = project_config_dir
        && let Some(local) = agents_md_block(&project_config_dir.join("AGENTS.md"), "project")
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
/// no `AGENTS.md`, non-built-in skills, or `.env` files, so a `SessionStart` hook
/// injects nothing in a project with no mu configuration.
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
    let env_paths = existing_env_paths(global_config_dir, project_config_dir);
    let preamble = export_preamble(&env_paths);
    let context = assemble_context(
        &user_skills,
        global_config_dir,
        project_config_dir,
        &preamble,
    );
    Ok(if context.is_empty() && !env_paths.is_empty() {
        preamble
    } else {
        context
    })
}

/// Assemble the `--export` preamble, appending a pointer to the `customize-mu`
/// reference when that built-in file is present so a foreign agent can read
/// mu's full configuration/skill/command contract on demand.
fn existing_env_paths(
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
) -> Vec<std::path::PathBuf> {
    std::iter::once(global_config_dir.join(".env"))
        .chain(project_config_dir.map(|dir| dir.join(".env")))
        .filter(|path| path.is_file())
        .filter_map(|path| path.canonicalize().ok())
        .collect()
}

fn export_preamble(env_paths: &[std::path::PathBuf]) -> String {
    let mut preamble = EXPORT_PREAMBLE.to_string();
    let customize = crate::paths::builtins_dir().join("customize-mu.md");
    if customize.is_file() {
        preamble.push_str(&format!(
            "\nTo understand mu's configuration, skills, and command contract, read {}.",
            crate::windows_msys2::display_path(&customize)
        ));
    }
    if !env_paths.is_empty() {
        preamble.push_str(&format!(
            "\nSkills may need environment values from these files (JSON strings), in global-to-project precedence: [{}]. Mu parses them as restricted shell-compatible assignments: blank lines and full-line `#` comments are ignored; assignments are `NAME=VALUE` with optional `export`; values are bare `[A-Za-z0-9_./:@%+,=-]*`, single-quoted, or double-quoted with only `\\\"`, `\\\\`, `\\$`, and `\\`` escapes. Expansion and other shell syntax are errors. Parse and load them when needed, but never display the files or expose secret values in output.",
            env_paths
                .iter()
                .map(|path| {
                    json_string_for_html_comment(&crate::windows_msys2::display_path(path))
                })
                .collect::<Vec<_>>()
                .join(", ")
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

    if let Some(global) = agents_md_block(&global_config_dir.join("AGENTS.md"), "global") {
        parts.push(global);
    }
    if let Some(project_config_dir) = project_config_dir
        && let Some(local) = agents_md_block(&project_config_dir.join("AGENTS.md"), "project")
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

fn agents_md_block(path: &Path, scope: &str) -> Option<String> {
    let contents = read_agents_md(path)?;
    let absolute_path = path.canonicalize().ok()?;
    let escaped_path = xml_escape_attribute(&crate::windows_msys2::display_path(&absolute_path));
    let mut block = format!("<agents_md scope=\"{scope}\" path=\"{escaped_path}\">\n");
    block.push_str(&contents);
    if !contents.ends_with('\n') {
        block.push('\n');
    }
    block.push_str("</agents_md>");
    Some(block)
}

fn xml_escape_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
        .replace('\t', "&#9;")
}

fn json_string_for_html_comment(value: &str) -> String {
    serde_json::to_string(value)
        .expect("serializing a path string cannot fail")
        .replace("--", "\\u002d\\u002d")
}

fn os_description() -> String {
    "windows (MSYS2 UCRT64)".to_string()
}

pub fn initial_environment_context(cwd: &Path, project: Option<&Project>) -> String {
    let mut lines = vec!["[environment]".to_string()];

    if let Some(project) = project {
        lines.push(format!(
            "mu project root: {}",
            crate::windows_msys2::display_path(&project.root)
        ));
        if let Some(worktree) = &project.worktree {
            if let Some(main_root) = worktree.main_worktree_root() {
                lines.push(format!(
                    "git worktree root: {}",
                    crate::windows_msys2::display_path(&worktree.root)
                ));
                lines.push(format!(
                    "git main worktree root: {}",
                    crate::windows_msys2::display_path(main_root)
                ));
            } else {
                lines.push(format!(
                    "git root: {}",
                    crate::windows_msys2::display_path(&worktree.root)
                ));
            }
        }
    }

    lines.push(format!(
        "current working directory: {}",
        crate::windows_msys2::display_path(cwd)
    ));

    lines.join("\n")
}

pub fn cwd_changed_context(cwd: &Path) -> String {
    format!(
        "<system-reminder>\ncurrent working directory changed to: {}\n</system-reminder>",
        crate::windows_msys2::display_path(cwd)
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        EXPORT_PREAMBLE, assemble_context, assemble_prompt, build_context, cwd_changed_context,
        export_preamble, initial_environment_context, json_string_for_html_comment, role_preamble,
        xml_escape_attribute,
    };
    use crate::paths::{Project, ProjectMarker};
    use crate::skills::{InstructionScope, SkillMeta, SkillRequirements};

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("mu-context-{name}-{}-{nanos}", std::process::id()));
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
        assert!(prompt.contains("os: windows (MSYS2 UCRT64)"));
        assert!(prompt.contains("\nuser: "));
        assert!(!prompt.contains("uid"));
    }

    #[test]
    fn initial_environment_context_reports_project_and_worktree_metadata() {
        let context = initial_environment_context(
            Path::new("/tmp/work/subdir"),
            Some(&Project {
                root: PathBuf::from("/tmp/work"),
                marker: ProjectMarker::Git,
                worktree: Some(crate::paths::GitWorktreeInfo {
                    root: PathBuf::from("/tmp/worktree"),
                    git_dir: PathBuf::from("/tmp/repo/.git/worktrees/feature"),
                    common_dir: Some(PathBuf::from("/tmp/repo/.git")),
                }),
            }),
        );
        assert_eq!(
            context,
            "[environment]\nmu project root: /tmp/work\ngit worktree root: /tmp/worktree\ngit main worktree root: /tmp/repo\ncurrent working directory: /tmp/work/subdir"
        );
    }

    #[test]
    fn initial_environment_context_uses_git_root_for_a_regular_checkout() {
        let context = initial_environment_context(
            Path::new("/tmp/repo/src"),
            Some(&Project {
                root: PathBuf::from("/tmp/repo"),
                marker: ProjectMarker::Git,
                worktree: Some(crate::paths::GitWorktreeInfo {
                    root: PathBuf::from("/tmp/repo"),
                    git_dir: PathBuf::from("/tmp/repo/.git"),
                    common_dir: None,
                }),
            }),
        );
        assert_eq!(
            context,
            "[environment]\nmu project root: /tmp/repo\ngit root: /tmp/repo\ncurrent working directory: /tmp/repo/src"
        );
    }

    #[test]
    fn initial_environment_context_omits_git_for_a_non_git_project() {
        let context = initial_environment_context(
            Path::new("/tmp/project/src"),
            Some(&Project {
                root: PathBuf::from("/tmp/project"),
                marker: ProjectMarker::Mu,
                worktree: None,
            }),
        );
        assert_eq!(
            context,
            "[environment]\nmu project root: /tmp/project\ncurrent working directory: /tmp/project/src"
        );
    }

    #[test]
    fn assemble_context_emits_preamble_agents_and_skills_without_role_preamble() {
        let global = temp_dir("assemble-global");
        fs::write(global.join("AGENTS.md"), "Global mu instructions.").unwrap();
        let skills = [skill("brave-search", InstructionScope::Global)];

        let context = assemble_context(&skills, &global, None, EXPORT_PREAMBLE);
        let agents_path = global.join("AGENTS.md").canonicalize().unwrap();
        fs::remove_dir_all(&global).unwrap();

        assert!(context.starts_with(EXPORT_PREAMBLE));
        assert!(context.contains("Mu `.env` files may contain API keys"));
        assert!(context.contains(&format!(
            "<agents_md scope=\"global\" path=\"{}\">\nGlobal mu instructions.\n</agents_md>",
            crate::windows_msys2::display_path(&agents_path)
        )));
        assert!(context.contains("<available_skills>"));
        assert!(context.contains("brave-search"));
        assert!(!context.contains("Relative paths inside a skill file"));
        assert!(!context.contains(role_preamble()));
        assert!(!context.contains("<runtime>"));
    }

    #[test]
    fn assemble_prompt_wraps_agents_files_with_scope_and_absolute_path() {
        let root = temp_dir("agents-wrappers");
        let global = root.join("global & user");
        let project = root.join("project");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(global.join("AGENTS.md"), "Global instructions.\n").unwrap();
        fs::write(project.join("AGENTS.md"), "Project instructions.").unwrap();

        let prompt = assemble_prompt(&[], &global, Some(&project));
        let global_path =
            crate::windows_msys2::display_path(&global.join("AGENTS.md").canonicalize().unwrap())
                .replace('&', "&amp;");
        let project_path = project.join("AGENTS.md").canonicalize().unwrap();
        fs::remove_dir_all(&root).unwrap();

        let global_block = format!(
            "<agents_md scope=\"global\" path=\"{global_path}\">\nGlobal instructions.\n</agents_md>"
        );
        let project_block = format!(
            "<agents_md scope=\"project\" path=\"{}\">\nProject instructions.\n</agents_md>",
            crate::windows_msys2::display_path(&project_path)
        );
        assert!(prompt.contains(&global_block));
        assert!(prompt.contains(&project_block));
        assert!(prompt.find(&global_block) < prompt.find(&project_block));
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
    fn export_preamble_points_at_customize_mu_and_existing_env_files() {
        let global = temp_dir("export-env-global");
        let project = temp_dir("export-env-project");
        fs::write(global.join(".env"), "GLOBAL_KEY=secret\n").unwrap();
        fs::write(project.join(".env"), "PROJECT_KEY=secret\n").unwrap();
        let global_env = global.join(".env").canonicalize().unwrap();
        let project_env = project.join(".env").canonicalize().unwrap();

        let preamble = export_preamble(&[global_env.clone(), project_env.clone()]);
        fs::remove_dir_all(&global).unwrap();
        fs::remove_dir_all(&project).unwrap();

        assert!(preamble.starts_with(EXPORT_PREAMBLE));
        assert!(preamble.trim_end().ends_with("-->"));
        assert!(preamble.contains(&crate::windows_msys2::display_path(&global_env)));
        assert!(preamble.contains(&crate::windows_msys2::display_path(&project_env)));
        assert!(preamble.contains("JSON strings"));
        assert!(preamble.contains("in global-to-project precedence"));
        assert!(preamble.contains("assignments are `NAME=VALUE` with optional `export`"));
        assert!(preamble.contains("bare `[A-Za-z0-9_./:@%+,=-]*`"));
        assert!(preamble.contains("single-quoted, or double-quoted"));
        assert!(preamble.contains("never display the files or expose secret values"));
        // On a packaged or source checkout the built-in reference exists, so the
        // pointer is appended; otherwise the preamble is just opened and closed.
        if crate::paths::builtins_dir()
            .join("customize-mu.md")
            .is_file()
        {
            assert!(preamble.contains("customize-mu.md"));
            assert!(preamble.contains("mu's configuration, skills, and command contract"));
        }
    }

    #[test]
    fn export_preamble_encodes_paths_without_closing_its_html_comment() {
        let path = std::path::PathBuf::from("/tmp/project-->injected\nname/.env");
        let encoded = json_string_for_html_comment(&path.display().to_string());

        assert_eq!(
            serde_json::from_str::<String>(&encoded).unwrap(),
            path.display().to_string()
        );
        assert!(!encoded.contains("--"));

        let preamble = export_preamble(&[path]);
        assert_eq!(preamble.matches("-->").count(), 1);
        assert!(!preamble.contains("injected\nname"));
    }

    #[test]
    fn agents_path_attribute_escapes_xml_markup_and_control_whitespace() {
        assert_eq!(
            xml_escape_attribute("a&\"<>\n\r\tb"),
            "a&amp;&quot;&lt;&gt;&#10;&#13;&#9;b"
        );
    }

    #[test]
    fn build_context_reports_env_files_without_agents_or_skills() {
        let global = temp_dir("export-env-only");
        fs::write(global.join(".env"), "API_KEY=secret\n").unwrap();
        let env_path = global.join(".env").canonicalize().unwrap();

        let context = build_context(&global, None).unwrap();
        fs::remove_dir_all(&global).unwrap();

        assert!(context.starts_with(EXPORT_PREAMBLE));
        assert!(context.contains(&crate::windows_msys2::display_path(&env_path)));
        assert!(!context.contains("<available_skills>"));
        assert!(!context.contains("<agents_md"));
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
        fs::write(global.join(".env"), "BRAVE_API_KEY=secret\n").unwrap();

        let env_path = global.join(".env").canonicalize().unwrap();

        // build_context scans real builtins; the user's global skill must appear
        // and no built-in skill (e.g. subagent) may leak into the skills index.
        let context = build_context(&global, None).unwrap();
        fs::remove_dir_all(&global).unwrap();

        assert!(context.contains("(path: "));
        assert!(context.contains("brave-search"));
        assert!(context.contains(&crate::windows_msys2::display_path(&env_path)));
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
