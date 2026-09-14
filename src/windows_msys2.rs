use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

struct Msys2Programs {
    bash: PathBuf,
    cygpath: PathBuf,
}

/// Require the environment in which the Windows build is supported.
pub fn validate_environment() -> Result<()> {
    require_ucrt64()?;
    let _ = programs()?;
    Ok(())
}

/// Resolve the Bash executable and its matching `cygpath.exe` from one MSYS2
/// installation.  Looking up the pair together avoids running a shell from
/// one installation with path conversion tools from another.
pub fn bash_program() -> Result<PathBuf> {
    require_ucrt64()?;
    Ok(programs()?.bash)
}

pub fn native_path(value: &str) -> Result<PathBuf> {
    let path = PathBuf::from(value);
    if is_msys_absolute(value) {
        return cygpath("-aw", path.as_os_str()).map(PathBuf::from);
    }
    if path.is_absolute() {
        return Ok(without_verbatim_prefix(&path));
    }
    Ok(std::env::current_dir()?.join(path))
}

pub fn shell_path(path: &Path) -> Result<String> {
    let path = without_verbatim_prefix(path);
    let text = path.as_os_str().to_string_lossy();
    if is_msys_absolute(&text) {
        return Ok(text.into_owned());
    }
    cygpath("-au", path.as_os_str())
}

/// Convert a path for user-facing shell output.  The fixed return type is part
/// of the display API, so conversion failures are reported on stderr before
/// retaining a useful native spelling instead of failing silently.
pub fn display_path(path: &Path) -> String {
    let path = without_verbatim_prefix(path);
    match shell_path(&path) {
        Ok(path) => path,
        Err(error) => {
            eprintln!(
                "mu: could not convert path {} for display: {error:#}",
                path.display()
            );
            path.display().to_string()
        }
    }
}

pub fn canonical_path(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::canonicalize(path).map(|path| without_verbatim_prefix(&path))
}

/// Convert one environment variable containing a path.  Callers that can
/// report an error should prefer [`native_env_path_result`].
pub fn native_env_path(value: &OsStr) -> PathBuf {
    match native_env_path_result(value) {
        Ok(path) => path,
        Err(error) => {
            eprintln!(
                "mu: could not convert environment path {}: {error:#}",
                value.to_string_lossy()
            );
            PathBuf::from(value)
        }
    }
}

pub fn native_env_path_result(value: &OsStr) -> Result<PathBuf> {
    let text = value.to_string_lossy();
    if is_msys_absolute(&text) {
        return cygpath("-aw", value).map(PathBuf::from);
    }
    Ok(PathBuf::from(value))
}

fn require_ucrt64() -> Result<()> {
    let msystem = std::env::var("MSYSTEM").unwrap_or_default();
    if !msystem.eq_ignore_ascii_case("UCRT64") {
        bail!(
            "Mu requires an MSYS2 UCRT64 shell (expected MSYSTEM=UCRT64, found {})",
            if msystem.is_empty() {
                "unset"
            } else {
                &msystem
            }
        );
    }
    Ok(())
}

fn programs() -> Result<Msys2Programs> {
    let path = std::env::var_os("PATH").context("PATH is not set")?;
    let directories = path_entries(&path);
    for directory in &directories {
        let bash = directory.join("bash.exe");
        let cygpath = directory.join("cygpath.exe");
        if bash.is_file() && cygpath.is_file() && same_directory(&bash, &cygpath) {
            return Ok(Msys2Programs { bash, cygpath });
        }
    }

    bail!("bash.exe and cygpath.exe from the same MSYS2 installation are not both on PATH")
}

fn path_entries(path: &OsStr) -> Vec<PathBuf> {
    let raw = path.to_string_lossy();
    if raw.contains(';') {
        return std::env::split_paths(path).collect();
    }

    let mut entries = Vec::new();
    let mut start = 0;
    for (index, component) in raw.char_indices() {
        if component == ':' && !(index == 1 && raw.as_bytes()[0].is_ascii_alphabetic()) {
            if index > start {
                entries.push(PathBuf::from(&raw[start..index]));
            }
            start = index + component.len_utf8();
        }
    }
    if start < raw.len() {
        entries.push(PathBuf::from(&raw[start..]));
    }
    if entries.is_empty() {
        std::env::split_paths(path).collect()
    } else {
        entries
    }
}

fn same_directory(left: &Path, right: &Path) -> bool {
    let left = left
        .parent()
        .and_then(|path| std::fs::canonicalize(path).ok());
    let right = right
        .parent()
        .and_then(|path| std::fs::canonicalize(path).ok());
    match (left, right) {
        (Some(left), Some(right)) => without_verbatim_prefix(&left)
            .to_string_lossy()
            .eq_ignore_ascii_case(&without_verbatim_prefix(&right).to_string_lossy()),
        _ => false,
    }
}

fn is_msys_absolute(value: &str) -> bool {
    value.starts_with('/')
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

fn cygpath(mode: &str, value: &OsStr) -> Result<String> {
    let output = Command::new(programs()?.cygpath)
        .arg(mode)
        .arg("--")
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
    let converted = converted.trim_end_matches(['\r', '\n']).to_string();
    if converted.is_empty() {
        bail!(
            "cygpath returned an empty path for {}",
            value.to_string_lossy()
        );
    }
    Ok(converted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_windows_verbatim_prefixes() {
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\C:\work\mu")),
            PathBuf::from(r"C:\work\mu")
        );
        assert_eq!(
            without_verbatim_prefix(Path::new(r"\\?\UNC\server\share\mu")),
            PathBuf::from(r"\\server\share\mu")
        );
    }

    #[test]
    fn splits_native_and_msysis_path_lists() {
        assert_eq!(
            path_entries(OsStr::new(r"C:\msys64\usr\bin;C:\msys64\bin")),
            [
                PathBuf::from(r"C:\msys64\usr\bin"),
                PathBuf::from(r"C:\msys64\bin")
            ]
        );
        assert_eq!(
            path_entries(OsStr::new("/ucrt64/bin:/usr/bin")),
            [PathBuf::from("/ucrt64/bin"), PathBuf::from("/usr/bin")]
        );
    }
}
