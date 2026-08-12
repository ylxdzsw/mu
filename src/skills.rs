use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config::EnvMap;

const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 256;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub path: String,
    pub scope: InstructionScope,
    pub requirements: SkillRequirements,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SkillRequirements {
    pub env: Vec<String>,
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CommandMeta {
    pub name: String,
    pub path: String,
    pub scope: InstructionScope,
}

#[derive(Debug)]
pub struct CommandPrompt {
    pub text: String,
    pub model: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct MuShebang {
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum InstructionScope {
    Builtin,
    Global,
    Project,
}

#[derive(Default)]
pub struct InstructionIndex {
    pub skills: Vec<SkillMeta>,
    pub commands: Vec<CommandMeta>,
}

struct ParsedSkill {
    name: String,
    description: String,
    requirements: SkillRequirements,
}

pub fn scan_instruction_index(
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
) -> Result<InstructionIndex> {
    let env = crate::config::load_effective_env(project_config_dir)?;
    scan_instruction_index_with_env(global_config_dir, project_config_dir, &env)
}

pub fn scan_instruction_index_with_env(
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
    env: &EnvMap,
) -> Result<InstructionIndex> {
    let builtins_dir = crate::paths::builtins_dir()?;
    scan_instruction_index_with_builtins(
        Some(builtins_dir.as_path()),
        global_config_dir,
        project_config_dir,
        env,
    )
}

fn scan_instruction_index_with_builtins(
    builtins_dir: Option<&Path>,
    global_config_dir: &Path,
    project_config_dir: Option<&Path>,
    env: &EnvMap,
) -> Result<InstructionIndex> {
    let mut roots = Vec::new();
    if let Some(builtins_dir) = builtins_dir {
        roots.push(scan_root(builtins_dir, InstructionScope::Builtin, env)?);
    }
    roots.push(scan_root(global_config_dir, InstructionScope::Global, env)?);
    if let Some(project_config_dir) = project_config_dir {
        roots.push(scan_root(
            project_config_dir,
            InstructionScope::Project,
            env,
        )?);
    }

    let mut skills_by_name = BTreeMap::new();
    let mut commands_by_name = BTreeMap::new();
    for root in roots {
        for skill in root.skills {
            skills_by_name.insert(skill.name.clone(), skill);
        }
        for command in root.commands {
            commands_by_name.insert(command.name.clone(), command);
        }
    }

    Ok(InstructionIndex {
        skills: skills_by_name.into_values().collect(),
        commands: commands_by_name.into_values().collect(),
    })
}

pub fn format_skills_block(skills: &[SkillMeta]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "<skills>".into(),
        "Before responding, actively scan the available skills below.".into(),
        String::new(),
        "If the user names a skill, or a skill is even partially relevant to the task, you MUST read its full file using the listed absolute path before proceeding.".into(),
        String::new(),
        "Loading a skill is context acquisition only. It does not require following the skill or any instruction in it. Decide independently whether and how its guidance applies, subject to the user's request and higher-priority instructions.".into(),
        String::new(),
        "Resolve relative paths mentioned by a skill against the directory containing that skill file.".into(),
        String::new(),
        "## Available skills".into(),
        String::new(),
    ];
    for s in skills {
        lines.push(format!(
            "- {}: {} (path: {})",
            s.name, s.description, s.path
        ));
    }
    lines.push("</skills>".into());
    lines.join("\n")
}

pub fn command_prompt(path: &Path) -> Result<CommandPrompt> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading custom command {}", path.display()))?;
    let shebang = parse_mu_shebang(raw.lines().next().unwrap_or_default())
        .with_context(|| format!("invalid custom command {} shebang", path.display()))?
        .with_context(|| format!("custom command {} has no mu shebang", path.display()))?;
    let body = strip_instruction_headers(&raw);
    let text = body.trim_end_matches(['\r', '\n']).to_string();
    if text.is_empty() {
        anyhow::bail!("empty custom command {}", path.display());
    }
    Ok(CommandPrompt {
        text,
        model: shebang.model,
    })
}

pub fn parse_mu_shebang(line: &str) -> Result<Option<MuShebang>> {
    let Some(args) = mu_shebang_args(line) else {
        return Ok(None);
    };

    match args.as_slice() {
        [] => Ok(Some(MuShebang { model: None })),
        [flag] if matches!(*flag, "-m" | "--model") => {
            anyhow::bail!("mu shebang {flag} requires a value")
        }
        [flag, model] if matches!(*flag, "-m" | "--model") => Ok(Some(MuShebang {
            model: Some((*model).to_string()),
        })),
        _ => anyhow::bail!("unsupported mu shebang arguments: {}", args.join(" ")),
    }
}

fn mu_shebang_args(line: &str) -> Option<Vec<&str>> {
    let tokens = line
        .strip_prefix("#!")?
        .split_whitespace()
        .collect::<Vec<_>>();
    let mu_index = tokens.iter().position(|token| {
        *token == "mu"
            || Path::new(token)
                .file_name()
                .is_some_and(|file_name| file_name == "mu")
    })?;
    Some(tokens[mu_index + 1..].to_vec())
}

fn scan_root(root: &Path, scope: InstructionScope, env: &EnvMap) -> Result<InstructionIndex> {
    if !root.is_dir() {
        return Ok(InstructionIndex::default());
    }

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == "AGENTS.md" || !is_valid_instruction_name(&name) {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_file() {
            entries.push((name.into_owned(), path));
        } else if metadata.is_dir() {
            let path = path.join("SKILL.md");
            if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_file()) {
                entries.push((format!("{name}/SKILL.md"), path));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut skills = Vec::new();
    let mut commands = Vec::new();

    for (relative, path) in entries {
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                eprintln!("warning: failed to read {}: {error}", path.display());
                continue;
            }
        };
        let (frontmatter, is_command) = strip_optional_mu_shebang(&content);
        let skill = if frontmatter.starts_with("---") {
            match parse_skill_frontmatter(frontmatter) {
                Ok(skill) => Some(skill),
                Err(error) => {
                    eprintln!("warning: invalid skill {}: {error}", path.display());
                    None
                }
            }
        } else {
            None
        };
        if !is_command && skill.is_none() {
            continue;
        }
        let absolute_path = path
            .canonicalize()
            .unwrap_or_else(|_| path.clone())
            .display()
            .to_string();
        if is_command {
            commands.push(CommandMeta {
                name: relative.clone(),
                path: absolute_path.clone(),
                scope,
            });
        }
        if let Some(skill) = skill {
            let expected = if relative == "SKILL.md" {
                None
            } else if let Some(parent) = relative.strip_suffix("/SKILL.md") {
                Some(parent.to_string())
            } else {
                Path::new(&relative)
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
            };
            match expected {
                Some(expected) if expected == skill.name => {
                    if requirements_met(&skill.requirements, env) {
                        skills.push(SkillMeta {
                            name: skill.name,
                            description: skill.description,
                            path: absolute_path,
                            scope,
                            requirements: skill.requirements,
                        });
                    }
                }
                Some(expected) => eprintln!(
                    "warning: skill {} has name {}, expected {}",
                    path.display(),
                    skill.name,
                    expected
                ),
                None => eprintln!(
                    "warning: skill {} has no valid inferred name",
                    path.display()
                ),
            }
        }
    }

    Ok(InstructionIndex { skills, commands })
}

fn strip_instruction_headers(content: &str) -> &str {
    let (after_shebang, _) = strip_optional_mu_shebang(content);
    strip_closed_frontmatter(after_shebang).unwrap_or(after_shebang)
}

fn strip_optional_mu_shebang(content: &str) -> (&str, bool) {
    let first_line = content.lines().next().unwrap_or_default();
    if mu_shebang_args(first_line).is_none() {
        return (content, false);
    }
    match content.find('\n') {
        Some(idx) => (&content[idx + 1..], true),
        None => ("", true),
    }
}

fn parse_skill_frontmatter(content: &str) -> Result<ParsedSkill> {
    let content = content
        .strip_prefix("---")
        .context("missing YAML frontmatter")?;
    let end = content.find("\n---").context("unclosed frontmatter")?;
    let yaml = &content[..end];
    let mut name = None;
    let mut description = None;
    let mut requirements = SkillRequirements::default();
    for line in yaml.lines() {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().trim_matches('"');
            match key.trim() {
                "name" => name = Some(value.to_string()),
                "description" => {
                    description = Some(value.split_whitespace().collect::<Vec<_>>().join(" "))
                }
                "requires_env" => requirements.env = parse_requirement_list(value)?,
                "requires_commands" => requirements.commands = parse_requirement_list(value)?,
                _ => {}
            }
        }
    }
    let name = name.context("missing name")?;
    let description = description.context("missing description")?;
    if !valid_skill_name(&name) {
        anyhow::bail!("invalid skill name");
    }
    if description.is_empty() {
        anyhow::bail!("empty description");
    }
    if description.len() > MAX_DESCRIPTION_LEN {
        anyhow::bail!("description too long");
    }
    validate_requirements(&requirements)?;
    Ok(ParsedSkill {
        name,
        description,
        requirements,
    })
}

fn parse_requirement_list(value: &str) -> Result<Vec<String>> {
    let mut entries = Vec::new();
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            anyhow::bail!("empty requirement entry");
        }
        entries.push(entry.to_string());
    }
    Ok(entries)
}

fn validate_requirements(requirements: &SkillRequirements) -> Result<()> {
    for name in &requirements.env {
        if !valid_env_requirement(name) {
            anyhow::bail!("invalid env requirement `{name}`");
        }
    }
    for command in &requirements.commands {
        if !valid_command_requirement(command) {
            anyhow::bail!("invalid command requirement `{command}`");
        }
    }
    Ok(())
}

fn valid_env_requirement(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn valid_command_requirement(command: &str) -> bool {
    !command.is_empty()
        && !command.contains('/')
        && command
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

fn requirements_met(requirements: &SkillRequirements, env: &EnvMap) -> bool {
    requirements
        .env
        .iter()
        .all(|name| env.get(name).is_some_and(|value| !value.is_empty()))
        && requirements
            .commands
            .iter()
            .all(|command| command_in_path(command, env))
}

fn command_in_path(command: &str, env: &EnvMap) -> bool {
    let Some(path) = env.get("PATH") else {
        return false;
    };
    std::env::split_paths(&OsString::from(path)).any(|dir| {
        let candidate = dir.join(command);
        candidate.is_file()
            && candidate
                .metadata()
                .is_ok_and(|metadata| metadata.permissions().mode() & 0o111 != 0)
    })
}

fn strip_closed_frontmatter(content: &str) -> Option<&str> {
    let rest = content.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let after_marker = &rest[end + "\n---".len()..];
    Some(after_marker.strip_prefix('\n').unwrap_or(after_marker))
}

fn valid_skill_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return false;
    }
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return false;
    }
    chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '-')
}

fn is_valid_instruction_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.starts_with('-')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn detects_and_parses_mu_shebangs() {
        assert!(mu_shebang_args("#!/usr/bin/env mu").is_some());
        assert!(mu_shebang_args("#!/usr/bin/env -S mu --output detail").is_some());
        assert!(mu_shebang_args("#!/usr/bin/mu").is_some());
        assert!(mu_shebang_args("#!/usr/bin/env bash").is_none());
        assert_eq!(
            parse_mu_shebang("#!/usr/bin/env mu").unwrap(),
            Some(MuShebang { model: None })
        );
        assert_eq!(
            parse_mu_shebang("#!/usr/bin/env -S mu --model openai/gpt-5:high").unwrap(),
            Some(MuShebang {
                model: Some("openai/gpt-5:high".into())
            })
        );
        assert_eq!(
            parse_mu_shebang("#!/usr/bin/env -S mu -m openai/gpt-5:high").unwrap(),
            Some(MuShebang {
                model: Some("openai/gpt-5:high".into())
            })
        );
        assert_eq!(parse_mu_shebang("#!/usr/bin/env bash").unwrap(), None);
    }

    #[test]
    fn rejects_other_mu_shebang_arguments() {
        for line in [
            "#!/usr/bin/env -S mu --model",
            "#!/usr/bin/env -S mu -m",
            "#!/usr/bin/env -S mu --output detail",
            "#!/usr/bin/env -S mu --model=openai/gpt-5",
            "#!/usr/bin/env -S mu --model openai/gpt-5 extra",
            "#!/usr/bin/env -S mu --model one --model two",
        ] {
            assert!(parse_mu_shebang(line).is_err(), "accepted {line}");
        }
    }

    #[test]
    fn command_prompt_rejects_other_mu_shebang_arguments() {
        let root = temp_root("invalid-command-shebang");
        fs::create_dir_all(&root).unwrap();
        let path = root.join("review.md");
        fs::write(
            &path,
            "#!/usr/bin/env -S mu --output detail\nReview the tree.\n",
        )
        .unwrap();

        let error = command_prompt(&path).unwrap_err();
        fs::remove_dir_all(root).unwrap();
        assert!(error.to_string().contains("invalid custom command"));
        assert!(format!("{error:#}").contains("unsupported mu shebang arguments"));
    }

    #[test]
    fn command_prompt_strips_shebang_and_frontmatter() {
        let body = strip_instruction_headers(
            "#!/usr/bin/env mu\n---\nname: review\n---\nReview the tree.\n",
        );
        assert_eq!(body, "Review the tree.\n");
    }

    #[test]
    fn scans_flat_command_skill_files() {
        let root = temp_root("flat-command-skill");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("review.md"),
            "#!/usr/bin/env mu\n---\nname: review\ndescription: Review changes.\n---\nReview it.\n",
        )
        .unwrap();

        let env = env_map(&[]);
        let index = scan_instruction_index_with_builtins(None, &root, None, &env).unwrap();

        assert_eq!(index.commands.len(), 1);
        assert_eq!(index.commands[0].name, "review.md");
        assert_eq!(index.skills.len(), 1);
        assert_eq!(index.skills[0].name, "review");
        assert_eq!(index.skills[0].description, "Review changes.");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scans_folder_skill_md_when_name_matches_parent() {
        let root = temp_root("folder-skill");
        let dir = root.join("review");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: review\ndescription: Review changes.\n---\nReview it.\n",
        )
        .unwrap();
        fs::write(
            dir.join("helper"),
            "#!/usr/bin/env mu\nThis supporting file is not a command.\n",
        )
        .unwrap();

        let env = env_map(&[]);
        let index = scan_instruction_index_with_builtins(None, &root, None, &env).unwrap();

        assert!(index.commands.is_empty());
        assert_eq!(index.skills.len(), 1);
        assert_eq!(index.skills[0].name, "review");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_skill_name_that_does_not_match_file() {
        let root = temp_root("skill-name-mismatch");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("review.md"),
            "---\nname: other\ndescription: Review changes.\n---\nReview it.\n",
        )
        .unwrap();

        let env = env_map(&[]);
        let index = scan_instruction_index_with_builtins(None, &root, None, &env).unwrap();

        assert!(index.skills.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn skill_requirements_parse_comma_separated_env_and_commands() {
        let skill = parse_skill_frontmatter(
            "---\nname: review\ndescription: Review changes.\nrequires_env: TOKEN, OTHER_TOKEN\nrequires_commands: gh, jq\n---\nReview it.\n",
        )
        .unwrap();

        assert_eq!(skill.requirements.env, ["TOKEN", "OTHER_TOKEN"]);
        assert_eq!(skill.requirements.commands, ["gh", "jq"]);
    }

    #[test]
    fn env_requirements_gate_skill_activation() {
        let root = temp_root("env-requirements");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("review.md"),
            "---\nname: review\ndescription: Review changes.\nrequires_env: TOKEN, OTHER_TOKEN\n---\nReview it.\n",
        )
        .unwrap();

        let missing = env_map(&[("TOKEN", "set")]);
        let index = scan_instruction_index_with_builtins(None, &root, None, &missing).unwrap();
        assert!(index.skills.is_empty());

        let present = env_map(&[("TOKEN", "set"), ("OTHER_TOKEN", "set")]);
        let index = scan_instruction_index_with_builtins(None, &root, None, &present).unwrap();
        assert_eq!(index.skills.len(), 1);
        assert_eq!(index.skills[0].requirements.env, ["TOKEN", "OTHER_TOKEN"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn command_requirements_gate_skill_activation() {
        let root = temp_root("command-requirements");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("gh"), "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(bin.join("gh")).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(bin.join("gh"), permissions).unwrap();
        fs::write(
            root.join("review.md"),
            "---\nname: review\ndescription: Review changes.\nrequires_commands: gh, jq\n---\nReview it.\n",
        )
        .unwrap();

        let missing = env_map(&[("PATH", &bin.display().to_string())]);
        let index = scan_instruction_index_with_builtins(None, &root, None, &missing).unwrap();
        assert!(index.skills.is_empty());

        fs::write(bin.join("jq"), "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(bin.join("jq")).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(bin.join("jq"), permissions).unwrap();
        let present = env_map(&[("PATH", &bin.display().to_string())]);
        let index = scan_instruction_index_with_builtins(None, &root, None, &present).unwrap();
        assert_eq!(index.skills.len(), 1);
        assert_eq!(index.skills[0].requirements.commands, ["gh", "jq"]);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_instructions_shadow_global_and_builtin_instructions() {
        let builtins = temp_root("builtins");
        let global = temp_root("global");
        let project = temp_root("project");
        fs::create_dir_all(&builtins).unwrap();
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            builtins.join("background-task.md"),
            "---\nname: background-task\ndescription: Start background tasks.\n---\nUse setsid.\n",
        )
        .unwrap();
        fs::write(
            builtins.join("review.md"),
            "#!/usr/bin/env mu\n---\nname: review\ndescription: Review builtins.\n---\nBuiltin review.\n",
        )
        .unwrap();
        fs::write(
            builtins.join("mu-doc.md"),
            "---\nname: mu-doc\ndescription: Document Mu.\n---\nUse reference files.\n",
        )
        .unwrap();
        fs::write(
            global.join("review.md"),
            "#!/usr/bin/env mu\n---\nname: review\ndescription: Review globally.\n---\nGlobal review.\n",
        )
        .unwrap();
        fs::write(
            project.join("review.md"),
            "#!/usr/bin/env mu\n---\nname: review\ndescription: Review locally.\n---\nLocal review.\n",
        )
        .unwrap();

        let env = env_map(&[]);
        let index =
            scan_instruction_index_with_builtins(Some(&builtins), &global, Some(&project), &env)
                .unwrap();

        assert_eq!(index.skills.len(), 3);
        assert_eq!(index.skills[0].name, "background-task");
        assert_eq!(index.skills[1].name, "mu-doc");
        let review = index
            .skills
            .iter()
            .find(|skill| skill.name == "review")
            .unwrap();
        assert_eq!(review.description, "Review locally.");
        let review_command = index
            .commands
            .iter()
            .find(|command| command.name == "review.md")
            .unwrap();
        assert_eq!(review_command.scope, InstructionScope::Project);
        assert_eq!(
            review_command.path,
            project.join("review.md").display().to_string()
        );
        fs::remove_dir_all(builtins).unwrap();
        fs::remove_dir_all(global).unwrap();
        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn repository_reference_files_are_not_indexed_as_instructions() {
        let builtins = Path::new(env!("CARGO_MANIFEST_DIR")).join("builtins");
        let global = temp_root("repository-builtins-global");
        fs::create_dir_all(&global).unwrap();

        let env = env_map(&[("PATH", "/usr/bin:/bin")]);
        let index =
            scan_instruction_index_with_builtins(Some(&builtins), &global, None, &env).unwrap();

        assert!(index.skills.iter().any(|skill| skill.name == "mu-doc"));
        assert!(!index.skills.iter().any(|skill| skill.name == "config"));
        assert!(!index.skills.iter().any(|skill| skill.name == "cli"));
        assert!(!index.skills.iter().any(|skill| skill.name == "goal"));
        assert!(
            !index
                .commands
                .iter()
                .any(|command| { matches!(command.name.as_str(), "config.md" | "cli.md") })
        );
        let goal = index
            .commands
            .iter()
            .find(|command| command.name == "goal")
            .expect("built-in goal command");
        assert_eq!(goal.scope, InstructionScope::Builtin);
        let prompt = command_prompt(Path::new(&goal.path)).unwrap();
        assert!(prompt.text.contains("error: /goal requires a goal"));
        assert!(
            prompt
                .text
                .contains("includes the original goal verbatim again")
        );
        fs::remove_dir_all(global).unwrap();
    }

    #[test]
    fn inactive_project_skill_does_not_shadow_active_global_skill() {
        let global = temp_root("global-shadow");
        let project = temp_root("project-shadow");
        fs::create_dir_all(&global).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            global.join("review.md"),
            "---\nname: review\ndescription: Review globally.\n---\nGlobal review.\n",
        )
        .unwrap();
        fs::write(
            project.join("review.md"),
            "---\nname: review\ndescription: Review locally.\nrequires_env: PROJECT_ONLY\n---\nLocal review.\n",
        )
        .unwrap();

        let env = env_map(&[]);
        let index =
            scan_instruction_index_with_builtins(None, &global, Some(&project), &env).unwrap();

        assert_eq!(index.skills.len(), 1);
        assert_eq!(index.skills[0].name, "review");
        assert_eq!(index.skills[0].description, "Review globally.");
        assert_eq!(index.skills[0].scope, InstructionScope::Global);
        fs::remove_dir_all(global).unwrap();
        fs::remove_dir_all(project).unwrap();
    }

    #[test]
    fn inactive_skill_is_excluded_from_prompt_but_command_remains_available() {
        let root = temp_root("inactive-command-skill");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("review.md"),
            "#!/usr/bin/env mu\n---\nname: review\ndescription: Review changes.\nrequires_env: TOKEN\n---\nReview it.\n",
        )
        .unwrap();

        let env = env_map(&[]);
        let index = scan_instruction_index_with_builtins(None, &root, None, &env).unwrap();

        assert!(index.skills.is_empty());
        assert_eq!(index.commands.len(), 1);
        assert!(format_skills_block(&index.skills).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    fn temp_root(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("mu-{name}-{}-{nanos}", std::process::id()))
    }

    fn env_map(entries: &[(&str, &str)]) -> EnvMap {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }
}
