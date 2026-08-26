use std::path::{Path, PathBuf};

use chrono::Local;

use crate::skills::{InstructionScope, SkillMeta, format_skills_block, scan_instruction_index};

const ROLE_PREAMBLE: &str = include_str!("system_preamble.md");

/// Preamble emitted by `mu context --export`. It tells a foreign agent (e.g.
/// Claude Code, which has a richer toolset than mu's single `bash`) that the
/// instructions and skills below were authored for mu and should be adapted in
/// intent rather than followed literally. A pointer to the `mu-doc`
/// reference is appended at runtime when that built-in file is present.
const EXPORT_PREAMBLE: &str = "\
<!-- Injected by `mu context --export`. The instructions and skills below were authored
for `mu`, a terminal agent whose only tool is `bash`. Adapt their intent to your own
tools. Mu `.env` files may contain secrets needed for some skills.";

const EXPORT_PREAMBLE_CLOSE: &str = " -->";

#[derive(Debug, Clone)]
pub struct SystemPromptSource {
    global_config_dir: PathBuf,
    project_config_dir: Option<PathBuf>,
    #[cfg(test)]
    fixed: Option<String>,
}

impl SystemPromptSource {
    pub fn new(global_config_dir: &Path, project_config_dir: Option<&Path>) -> Self {
        Self {
            global_config_dir: global_config_dir.to_path_buf(),
            project_config_dir: project_config_dir.map(Path::to_path_buf),
            #[cfg(test)]
            fixed: None,
        }
    }

    pub fn build(&self) -> anyhow::Result<String> {
        #[cfg(test)]
        if let Some(prompt) = &self.fixed {
            return Ok(prompt.clone());
        }
        build_system_prompt(&self.global_config_dir, self.project_config_dir.as_deref())
    }

    #[cfg(test)]
    pub fn fixed(prompt: impl Into<String>) -> Self {
        Self {
            global_config_dir: PathBuf::new(),
            project_config_dir: None,
            fixed: Some(prompt.into()),
        }
    }
}

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
    let mut parts = vec![format!(
        "<system_preamble>\n{}\n</system_preamble>",
        role_preamble()
    )];

    let os = os_description();
    let date = Local::now().format("%Y-%m-%d").to_string();
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let uid = unsafe { libc::geteuid() };
    let mut runtime = format!("os: {os}\ndate: {date}\nuser: {user} (uid {uid})");
    if let Some(project_root) = project_config_dir.and_then(Path::parent) {
        runtime.push_str(&format!("\nmu project root: {}", project_root.display()));
    }
    parts.push(format!("<runtime>\n{runtime}\n</runtime>"));

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
    let preamble = export_preamble(&env_paths)?;
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

/// Assemble the `--export` preamble, appending a pointer to the `mu-doc`
/// reference when that built-in file is present so a foreign agent can find
/// Mu's documentation on demand.
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

fn export_preamble(env_paths: &[std::path::PathBuf]) -> anyhow::Result<String> {
    let mut preamble = EXPORT_PREAMBLE.to_string();
    let builtins = crate::paths::builtins_dir()?;
    let mu_doc = ["mu-doc", "mu-doc.md"]
        .into_iter()
        .map(|name| builtins.join(name))
        .find(|path| path.is_file());
    if let Some(mu_doc) = mu_doc {
        preamble.push_str(&format!(
            "\nTo understand Mu, its configuration, and its CLI, read {}.",
            mu_doc.display()
        ));
    }
    if !env_paths.is_empty() {
        preamble.push_str(&format!(
            "\nSkills may need environment values from these files (JSON strings), in global-to-project precedence: [{}]. Mu parses them as restricted shell-compatible assignments: blank lines and full-line `#` comments are ignored; assignments are `NAME=VALUE` with optional `export`; values are bare `[A-Za-z0-9_./:@%+,=-]*`, single-quoted, or double-quoted with only `\\\"`, `\\\\`, `\\$`, and `\\`` escapes. Expansion and other shell syntax are errors. Parse and load them when needed, but never display the files or expose secret values in output.",
            env_paths
                .iter()
                .map(|path| json_string_for_html_comment(&path.display().to_string()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    preamble.push_str(EXPORT_PREAMBLE_CLOSE);
    Ok(preamble)
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
    let contents = std::fs::read_to_string(path).ok()?;
    let absolute_path = path.canonicalize().ok()?;
    let escaped_path = xml_escape_attribute(&absolute_path.display().to_string());
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        EXPORT_PREAMBLE, SystemPromptSource, assemble_context, assemble_prompt, build_context,
        export_preamble, json_string_for_html_comment, role_preamble,
    };
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
    fn assemble_context_emits_preamble_agents_and_skills_without_role_preamble() {
        let global = temp_dir("assemble-global");
        fs::write(global.join("AGENTS.md"), "Global mu instructions.").unwrap();
        let skills = [skill("brave-search", InstructionScope::Global)];

        let context = assemble_context(&skills, &global, None, EXPORT_PREAMBLE);
        let agents_path = global.join("AGENTS.md").canonicalize().unwrap();
        fs::remove_dir_all(&global).unwrap();

        assert!(context.starts_with(EXPORT_PREAMBLE));
        assert!(context.contains(&format!(
            "<agents_md scope=\"global\" path=\"{}\">\nGlobal mu instructions.\n</agents_md>",
            agents_path.display()
        )));
        assert!(context.contains("<skills>"));
        assert!(context.contains("\n</skills>"));
        assert!(context.contains("brave-search"));
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
        let global_path = global
            .join("AGENTS.md")
            .canonicalize()
            .unwrap()
            .display()
            .to_string()
            .replace('&', "&amp;");
        let project_path = project.join("AGENTS.md").canonicalize().unwrap();
        fs::remove_dir_all(&root).unwrap();

        let global_block = format!(
            "<agents_md scope=\"global\" path=\"{global_path}\">\nGlobal instructions.\n</agents_md>"
        );
        let project_block = format!(
            "<agents_md scope=\"project\" path=\"{}\">\nProject instructions.\n</agents_md>",
            project_path.display()
        );
        assert!(prompt.contains(&global_block));
        assert!(prompt.contains(&project_block));
        assert!(prompt.find(&global_block) < prompt.find(&project_block));
    }

    #[test]
    fn system_prompt_source_reads_latest_instructions_on_each_build() {
        let global = temp_dir("prompt-source-global");
        fs::write(global.join("AGENTS.md"), "First instructions.").unwrap();
        let source = SystemPromptSource::new(&global, None);

        assert!(source.build().unwrap().contains("First instructions."));
        fs::write(global.join("AGENTS.md"), "Second instructions.").unwrap();
        assert!(source.build().unwrap().contains("Second instructions."));

        fs::remove_dir_all(global).unwrap();
    }

    #[test]
    fn export_preamble_points_at_mu_doc_and_existing_env_files() {
        let global = temp_dir("export-env-global");
        let project = temp_dir("export-env-project");
        fs::write(global.join(".env"), "GLOBAL_KEY=secret\n").unwrap();
        fs::write(project.join(".env"), "PROJECT_KEY=secret\n").unwrap();
        let global_env = global.join(".env").canonicalize().unwrap();
        let project_env = project.join(".env").canonicalize().unwrap();

        let preamble = export_preamble(&[global_env.clone(), project_env.clone()]).unwrap();
        fs::remove_dir_all(&global).unwrap();
        fs::remove_dir_all(&project).unwrap();

        assert!(preamble.starts_with(EXPORT_PREAMBLE));
        assert!(preamble.trim_end().ends_with("-->"));
        assert!(preamble.contains(&global_env.display().to_string()));
        assert!(preamble.contains(&project_env.display().to_string()));
        // On a packaged or source checkout the built-in reference exists, so the
        // pointer is appended; otherwise the preamble is just opened and closed.
        if ["mu-doc", "mu-doc.md"]
            .into_iter()
            .map(|name| crate::paths::builtins_dir().unwrap().join(name))
            .any(|path| path.is_file())
        {
            assert!(preamble.contains("mu-doc"));
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

        let preamble = export_preamble(&[path]).unwrap();
        assert_eq!(preamble.matches("-->").count(), 1);
        assert!(!preamble.contains("injected\nname"));
    }

    #[test]
    fn build_context_reports_env_files_without_agents_or_skills() {
        let global = temp_dir("export-env-only");
        fs::write(global.join(".env"), "API_KEY=secret\n").unwrap();
        let env_path = global.join(".env").canonicalize().unwrap();

        let context = build_context(&global, None).unwrap();
        fs::remove_dir_all(&global).unwrap();

        assert!(context.starts_with(EXPORT_PREAMBLE));
        assert!(context.contains(&env_path.display().to_string()));
        assert!(!context.contains("<skills>"));
        assert!(!context.contains("<agents_md"));
    }

    #[test]
    fn build_context_excludes_builtin_skills() {
        let global = temp_dir("build-global");
        fs::write(
            global.join("mu-doc.md"),
            "---\nname: mu-doc\ndescription: Document Mu.\n---\nUse reference files.\n",
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
        assert!(context.contains(&env_path.display().to_string()));
        // `subagent` is a built-in skill; only the preamble's mu-doc
        // pointer may mention a built-in path, never the skills index.
        assert!(!context.contains("subagent"));
    }
}
