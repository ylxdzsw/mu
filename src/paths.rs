use std::path::PathBuf;

pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MU_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg).join("mu");
    }
    dirs_home().join(".config/mu")
}

pub fn state_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(xdg).join("mu");
    }
    dirs_home().join(".local/state/mu")
}

pub fn runtime_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("mu");
    }
    std::env::temp_dir().join("mu")
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

pub fn ensure_dir(path: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}
