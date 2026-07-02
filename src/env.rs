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
#[path = "env_tests.rs"]
mod tests;
