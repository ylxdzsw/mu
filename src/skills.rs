use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path};

use anyhow::{Context, Result};

use crate::config::EnvMap;

const MAX_DEPTH: usize = 4;
const MAX_FILES_PER_ROOT: usize = 512;
const MAX_SKILLS: usize = 64;
const MAX_COMMANDS: usize = 256;
const MAX_NAME_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 256;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub path: String,
    pub scope: InstructionScope,
    pub requirements: SkillRequirements,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SkillRequirements {
    pub env: Vec<String>,
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CommandMeta {
    pub name: String,
    pub path: String,
    pub scope: InstructionScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPrompt {
    pub text: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MuShebang {
    pub model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstructionScope {
    Builtin,
    Global,
    Project,
}

#[derive(Debug, Clone, Default)]
pub struct InstructionIndex {
    pub skills: Vec<SkillMeta>,
    pub commands: Vec<CommandMeta>,
}

#[derive(Debug, Clone, Copy)]
struct ScanLimits {
    max_depth: usize,
    max_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SnapshotEntry {
    path: String,
    kind: SnapshotKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SnapshotKind {
    Dir,
    File,
}

#[derive(Debug)]
struct RootIndex {
    skills: Vec<SkillMeta>,
    commands: Vec<CommandMeta>,
}

#[derive(Debug)]
struct ScanSnapshot {
    entries: Vec<SnapshotEntry>,
    truncated: bool,
}

#[derive(Debug)]
struct ParsedInstruction {
    is_command: bool,
    skill: Option<ParsedSkill>,
    skill_error: Option<anyhow::Error>,
}

#[derive(Debug)]
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
    let builtins_dir = crate::paths::builtins_dir();
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

    let mut skills = skills_by_name.into_values().collect::<Vec<_>>();
    let mut commands = commands_by_name.into_values().collect::<Vec<_>>();
    if skills.len() > MAX_SKILLS {
        skills.truncate(MAX_SKILLS);
    }
    if commands.len() > MAX_COMMANDS {
        commands.truncate(MAX_COMMANDS);
    }

    Ok(InstructionIndex { skills, commands })
}

pub fn format_skills_block(skills: &[SkillMeta]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut lines = vec!["<available_skills>".into()];
    for s in skills {
        lines.push(format!(
            "- {}: {} (path: {})",
            s.name, s.description, s.path
        ));
    }
    lines.push(
        "Relative paths inside a skill file resolve against that file's containing directory."
            .into(),
    );
    lines.push("</available_skills>".into());
    lines.join("\n")
}

pub fn read_agents_md(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
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
        ["--model"] => anyhow::bail!("mu shebang --model requires a value"),
        ["--model", model] => Ok(Some(MuShebang {
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

pub fn find_command<'a>(index: &'a InstructionIndex, name: &str) -> Option<&'a CommandMeta> {
    index.commands.iter().find(|command| command.name == name)
}

fn scan_root(root: &Path, scope: InstructionScope, env: &EnvMap) -> Result<RootIndex> {
    if !root.is_dir() {
        return Ok(RootIndex {
            skills: Vec::new(),
            commands: Vec::new(),
        });
    }

    let limits = ScanLimits {
        max_depth: MAX_DEPTH,
        max_files: MAX_FILES_PER_ROOT,
    };
    let snapshot = collect_snapshot(root, limits)?;
    let (index, _warnings) = build_root_index(root, scope, &snapshot, env)?;
    Ok(index)
}

fn collect_snapshot(root: &Path, limits: ScanLimits) -> Result<ScanSnapshot> {
    let mut entries = Vec::new();
    let mut file_count = 0;
    let mut truncated = false;
    collect_snapshot_dir(
        root,
        Path::new(""),
        0,
        limits,
        &mut entries,
        &mut file_count,
        &mut truncated,
    )?;
    entries.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| kind_name(&a.kind).cmp(kind_name(&b.kind)))
    });
    Ok(ScanSnapshot { entries, truncated })
}

fn collect_snapshot_dir(
    root: &Path,
    relative_dir: &Path,
    depth: usize,
    limits: ScanLimits,
    entries: &mut Vec<SnapshotEntry>,
    file_count: &mut usize,
    truncated: &mut bool,
) -> Result<()> {
    if depth >= limits.max_depth {
        return Ok(());
    }

    let dir = root.join(relative_dir);
    let mut children = Vec::new();
    for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if is_reserved_entry(relative_dir, &name) {
            continue;
        }
        children.push((name.to_string(), entry.path()));
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path) in children {
        let relative = relative_dir.join(&name);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.is_dir() {
            if is_valid_instruction_relative_path(&relative) {
                entries.push(SnapshotEntry {
                    path: slash_path(&relative),
                    kind: SnapshotKind::Dir,
                });
                collect_snapshot_dir(
                    root,
                    &relative,
                    depth + 1,
                    limits,
                    entries,
                    file_count,
                    truncated,
                )?;
            }
        } else if metadata.is_file() {
            if *file_count >= limits.max_files {
                *truncated = true;
                continue;
            }
            *file_count += 1;
            if !is_valid_instruction_relative_path(&relative) {
                continue;
            }
            entries.push(SnapshotEntry {
                path: slash_path(&relative),
                kind: SnapshotKind::File,
            });
        }
    }

    Ok(())
}

fn build_root_index(
    root: &Path,
    scope: InstructionScope,
    snapshot: &ScanSnapshot,
    env: &EnvMap,
) -> Result<(RootIndex, Vec<String>)> {
    let mut skills = Vec::new();
    let mut commands = Vec::new();
    let mut warnings = Vec::new();

    if snapshot.truncated {
        warnings.push(format!(
            "{} scan reached the {} file limit",
            root.display(),
            MAX_FILES_PER_ROOT
        ));
    }

    for entry in snapshot
        .entries
        .iter()
        .filter(|entry| matches!(entry.kind, SnapshotKind::File))
    {
        let relative = Path::new(&entry.path);
        let path = root.join(relative);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                warnings.push(format!("failed to read {}: {error}", path.display()));
                continue;
            }
        };
        let parsed = parse_instruction(&content);
        if let Some(error) = parsed.skill_error {
            warnings.push(format!("invalid skill {}: {error}", path.display()));
        }
        if parsed.is_command && commands.len() < MAX_COMMANDS {
            commands.push(CommandMeta {
                name: entry.path.clone(),
                path: path
                    .canonicalize()
                    .unwrap_or(path.clone())
                    .display()
                    .to_string(),
                scope,
            });
        }
        if let Some(skill) = parsed.skill {
            match expected_skill_name(relative) {
                Some(expected) if expected == skill.name => {
                    if skills.len() < MAX_SKILLS && requirements_met(&skill.requirements, env) {
                        skills.push(SkillMeta {
                            name: skill.name,
                            description: skill.description,
                            path: path
                                .canonicalize()
                                .unwrap_or(path.clone())
                                .display()
                                .to_string(),
                            scope,
                            requirements: skill.requirements,
                        });
                    }
                }
                Some(expected) => warnings.push(format!(
                    "skill {} has name {}, expected {}",
                    path.display(),
                    skill.name,
                    expected
                )),
                None => warnings.push(format!(
                    "skill {} has no valid inferred name",
                    path.display()
                )),
            }
        }
    }

    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    Ok((RootIndex { skills, commands }, warnings))
}

fn parse_instruction(content: &str) -> ParsedInstruction {
    let (after_shebang, is_command) = strip_optional_mu_shebang(content);
    let (skill, skill_error) = match parse_skill_frontmatter(after_shebang) {
        Ok(skill) => (Some(skill), None),
        Err(error) if is_requirement_error(&error) => (None, Some(error)),
        Err(_) => (None, None),
    };
    ParsedInstruction {
        is_command,
        skill,
        skill_error,
    }
}

fn is_requirement_error(error: &anyhow::Error) -> bool {
    error.to_string().contains("requirement")
}

fn strip_instruction_headers(content: &str) -> &str {
    let (after_shebang, _) = strip_optional_mu_shebang(content);
    strip_closed_frontmatter(after_shebang).unwrap_or(after_shebang)
}

fn strip_optional_mu_shebang(content: &str) -> (&str, bool) {
    let first_line = content.lines().next().unwrap_or_default();
    if !is_mu_shebang(first_line) {
        return (content, false);
    }
    match content.find('\n') {
        Some(idx) => (&content[idx + 1..], true),
        None => ("", true),
    }
}

fn is_mu_shebang(line: &str) -> bool {
    mu_shebang_args(line).is_some()
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
                "description" => description = Some(collapse_description(value)),
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
    if entries.is_empty() {
        anyhow::bail!("empty requirement list");
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

fn collapse_description(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn expected_skill_name(relative: &Path) -> Option<String> {
    let file_name = relative.file_name()?.to_string_lossy();
    if file_name == "SKILL.md" {
        return relative
            .parent()?
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
    }
    relative
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
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

fn is_reserved_entry(relative_dir: &Path, name: &str) -> bool {
    if name == "." || name == ".." {
        return true;
    }
    if relative_dir.as_os_str().is_empty() {
        return matches!(
            name,
            "cache" | "locks" | "config.jsonc" | ".env" | ".gitignore" | "AGENTS.md"
        ) || name == "sessions.db"
            || name.starts_with("sessions.db-");
    }
    false
}

fn is_valid_instruction_relative_path(path: &Path) -> bool {
    path.components().all(|component| match component {
        Component::Normal(name) => {
            let name = name.to_string_lossy();
            !name.is_empty()
                && !name.starts_with('.')
                && !name.starts_with('-')
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
        }
        _ => false,
    })
}

fn slash_path(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn kind_name(kind: &SnapshotKind) -> &'static str {
    match kind {
        SnapshotKind::Dir => "dir",
        SnapshotKind::File => "file",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn detects_permissive_mu_shebangs() {
        assert!(is_mu_shebang("#!/usr/bin/env mu"));
        assert!(is_mu_shebang("#!/usr/bin/env -S mu --output plain"));
        assert!(is_mu_shebang("#!/usr/bin/mu"));
        assert!(!is_mu_shebang("#!/usr/bin/env bash"));
        assert!(!is_mu_shebang("not a shebang"));
    }

    #[test]
    fn parses_optional_model_from_mu_shebang() {
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
        assert_eq!(parse_mu_shebang("#!/usr/bin/env bash").unwrap(), None);
    }

    #[test]
    fn rejects_other_mu_shebang_arguments() {
        for line in [
            "#!/usr/bin/env -S mu --model",
            "#!/usr/bin/env -S mu --output plain",
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
            "#!/usr/bin/env -S mu --output plain\nReview the tree.\n",
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
    fn skill_name_must_match_file_or_parent_folder() {
        assert_eq!(
            expected_skill_name(Path::new("review.md")).as_deref(),
            Some("review")
        );
        assert_eq!(
            expected_skill_name(Path::new("review/SKILL.md")).as_deref(),
            Some("review")
        );
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
    fn scans_legacy_skill_md_when_name_matches_parent() {
        let root = temp_root("legacy-skill");
        let dir = root.join("review");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("SKILL.md"),
            "---\nname: review\ndescription: Review changes.\n---\nReview it.\n",
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
            "---\nname: background-task\ndescription: Start background tasks.\n---\nUse systemd-run.\n",
        )
        .unwrap();
        fs::write(
            builtins.join("review.md"),
            "#!/usr/bin/env mu\n---\nname: review\ndescription: Review builtins.\n---\nBuiltin review.\n",
        )
        .unwrap();
        fs::write(
            builtins.join("customize-mu.md"),
            "---\nname: customize-mu\ndescription: Customize mu.\n---\nUse config files.\n",
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
        assert_eq!(index.skills[1].name, "customize-mu");
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

    #[test]
    fn repository_builtins_have_valid_skill_metadata() {
        let builtins = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("builtins");

        let root = temp_root("repository-builtins");
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("agent-browser"), "#!/bin/sh\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(bin.join("agent-browser"))
            .unwrap()
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(bin.join("agent-browser"), permissions).unwrap();

        let path = std::env::var("PATH").unwrap_or_default();
        let path = format!("{}:{path}", bin.display());
        let env = env_map(&[
            ("PATH", path.as_str()),
            ("BRAVE_API_KEY", "test-brave-key"),
            ("EXA_API_KEY", "test-exa-key"),
        ]);
        let index = scan_root(&builtins, InstructionScope::Builtin, &env).unwrap();

        let names = index
            .skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"background-task"));
        assert!(names.contains(&"agent-browser"));
        assert!(names.contains(&"brave-search"));
        assert!(names.contains(&"customize-mu"));
        assert!(names.contains(&"exa-search"));
        assert!(names.contains(&"subagent"));
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
