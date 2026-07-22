use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn validate_environment() -> Result<()> {
    let msystem = std::env::var("MSYSTEM").unwrap_or_default();
    if !msystem.eq_ignore_ascii_case("UCRT64") {
        bail!(
            "the msys2 branch requires an MSYS2 UCRT64 shell (expected MSYSTEM=UCRT64, found {})",
            if msystem.is_empty() {
                "unset"
            } else {
                &msystem
            }
        );
    }
    bash_program()?;
    cygpath_program()?;
    Ok(())
}

pub fn bash_program() -> Result<PathBuf> {
    program_in_path("bash.exe").context("finding MSYS2 bash.exe on PATH")
}

fn cygpath_program() -> Result<PathBuf> {
    program_in_path("cygpath.exe").context("finding MSYS2 cygpath.exe on PATH")
}

fn program_in_path(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("{name} is not installed"))
}

pub fn native_path(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return Ok(path);
    }
    if !value.starts_with('/') {
        return Ok(std::env::current_dir()?.join(path));
    }
    let converted = cygpath("-aw", OsStr::new(value))?;
    Ok(PathBuf::from(converted))
}

pub fn native_env_path(value: &OsStr) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        return path;
    }
    let text = value.to_string_lossy();
    if text.starts_with('/')
        && let Ok(converted) = cygpath("-aw", value)
    {
        return PathBuf::from(converted);
    }
    path
}

pub fn shell_path(path: &Path) -> Result<String> {
    let text = path.as_os_str().to_string_lossy();
    if text.starts_with('/') {
        return Ok(text.into_owned());
    }
    cygpath("-au", path.as_os_str())
}

pub fn display_path(path: &Path) -> String {
    let path = without_verbatim_prefix(path);
    shell_path(&path).unwrap_or_else(|_| path.display().to_string())
}

pub fn canonical_path(path: &Path) -> std::io::Result<PathBuf> {
    path.canonicalize()
        .map(|canonical| without_verbatim_prefix(&canonical))
}

fn without_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.as_os_str().to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

pub fn builtins_dir() -> PathBuf {
    let installed = install_prefix().join("share/mu");
    if installed.is_dir() {
        return installed;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("builtins")
}

pub fn libexec_dir() -> PathBuf {
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
        && parent.join("apply_patch.exe").is_file()
    {
        return parent.to_path_buf();
    }
    install_prefix().join("libexec/mu")
}

pub fn libexec_shell_path() -> Result<String> {
    let dir = libexec_dir();
    if dir == install_prefix().join("libexec/mu") {
        return Ok("/ucrt64/libexec/mu".to_string());
    }
    shell_path(&dir)
}

fn install_prefix() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().and_then(Path::parent).map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from(r"C:\msys64\ucrt64"))
}

fn cygpath(mode: &str, value: &OsStr) -> Result<String> {
    let output = Command::new(cygpath_program()?)
        .arg(mode)
        .arg(value)
        .output()
        .context("running MSYS2 cygpath")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "cygpath failed for {}: {}",
            value.to_string_lossy(),
            stderr.trim()
        );
    }
    let converted = String::from_utf8(output.stdout).context("decoding cygpath output")?;
    Ok(converted.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_native_paths_to_shell_paths_and_back() {
        let native = std::env::current_dir().unwrap();
        let shell = shell_path(&native).unwrap();
        assert!(shell.starts_with('/'));
        assert_eq!(
            native_path(&shell).unwrap().canonicalize().unwrap(),
            native.canonicalize().unwrap()
        );
    }
}
