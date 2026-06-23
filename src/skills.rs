use std::path::Path;

use anyhow::Result;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub path: String,
}

pub fn scan_skills(
    config_dir: &Path,
    store: Option<&crate::store::Store>,
) -> Result<Vec<SkillMeta>> {
    let skills_dir = config_dir.join("skills");
    if !skills_dir.exists() {
        return Ok(vec![]);
    }

    let mtime = dir_mtime(&skills_dir)?;

    if let Some(store) = store {
        if let Ok(Some(cached)) = store.get_skill_cache(mtime) {
            if let Ok(skills) = serde_json::from_str(&cached) {
                return Ok(skills);
            }
        }
    }

    let mut skills = Vec::new();
    for entry in std::fs::read_dir(&skills_dir)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let skill_md = entry.path().join("SKILL.md");
        if !skill_md.exists() {
            eprintln!(
                "warning: skill dir {} has no SKILL.md",
                entry.path().display()
            );
            continue;
        }
        match parse_skill_frontmatter(&skill_md) {
            Ok((name, description)) => {
                if name.len() > 64 {
                    eprintln!("warning: skill name too long in {}", skill_md.display());
                    continue;
                }
                if description.len() > 256 {
                    eprintln!(
                        "warning: skill description too long in {}",
                        skill_md.display()
                    );
                    continue;
                }
                skills.push(SkillMeta {
                    name,
                    description,
                    path: skill_md.canonicalize()?.display().to_string(),
                });
            }
            Err(e) => {
                eprintln!("warning: malformed skill {}: {e}", skill_md.display());
            }
        }
    }

    if let Some(store) = store {
        if let Ok(json) = serde_json::to_string(&skills) {
            let _ = store.set_skill_cache(mtime, &json);
        }
    }

    Ok(skills)
}

fn dir_mtime(path: &Path) -> Result<i64> {
    let mut max_mtime = file_mtime(path)?;

    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let skill_md = entry.path().join("SKILL.md");
        if skill_md.exists() {
            let m = file_mtime(&skill_md)?;
            if m > max_mtime {
                max_mtime = m;
            }
        }
    }
    Ok(max_mtime)
}

fn file_mtime(path: &Path) -> Result<i64> {
    let meta = std::fs::metadata(path)?;
    let modified = meta.modified()?;
    Ok(modified
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64)
}

fn parse_skill_frontmatter(path: &Path) -> Result<(String, String)> {
    let content = std::fs::read_to_string(path)?;
    if !content.starts_with("---") {
        anyhow::bail!("missing YAML frontmatter");
    }
    let rest = &content[3..];
    let end = rest
        .find("\n---")
        .ok_or_else(|| anyhow::anyhow!("unclosed frontmatter"))?;
    let yaml = &rest[..end];
    let mut name = None;
    let mut description = None;
    for line in yaml.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim();
            let val = v.trim().trim_matches('"');
            match key {
                "name" => name = Some(val.to_string()),
                "description" => description = Some(val.to_string()),
                _ => {}
            }
        }
    }
    Ok((
        name.ok_or_else(|| anyhow::anyhow!("missing name"))?,
        description.ok_or_else(|| anyhow::anyhow!("missing description"))?,
    ))
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
    lines.push("Relative paths inside SKILL.md resolve against the skill's directory.".into());
    lines.push("</available_skills>".into());
    lines.join("\n")
}

pub fn read_agents_md(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}
