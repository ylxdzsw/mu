use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::paths;

pub type EnvMap = HashMap<String, String>;

pub fn load_effective(project_config_dir: Option<&Path>) -> Result<EnvMap> {
    let mut env: EnvMap = std::env::vars().collect();
    load_dotenv_into(&paths::global_dir().join(".env"), &mut env)?;
    if let Some(dir) = project_config_dir {
        load_dotenv_into(&dir.join(".env"), &mut env)?;
    }
    Ok(env)
}

fn load_dotenv_into(path: &Path, env: &mut EnvMap) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let iter =
        dotenvy::from_path_iter(path).with_context(|| format!("parsing {}", path.display()))?;
    for item in iter {
        let (key, value) = item.with_context(|| format!("parsing {}", path.display()))?;
        env.insert(key, value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn dotenv_overlays_in_order() {
        let tmp = std::env::temp_dir().join(format!("mu-env-{}", uuid::Uuid::new_v4()));
        let global = tmp.join("global");
        let project = tmp.join("project/.mu");
        write(&global.join(".env"), "SAME=global\nGLOBAL_ONLY=1\n");
        write(&project.join(".env"), "SAME=project\nPROJECT_ONLY=2\n");

        let mut env = EnvMap::new();
        load_dotenv_into(&global.join(".env"), &mut env).unwrap();
        load_dotenv_into(&project.join(".env"), &mut env).unwrap();

        assert_eq!(env.get("SAME").map(String::as_str), Some("project"));
        assert_eq!(env.get("GLOBAL_ONLY").map(String::as_str), Some("1"));
        assert_eq!(env.get("PROJECT_ONLY").map(String::as_str), Some("2"));

        let _ = std::fs::remove_dir_all(tmp);
    }
}
