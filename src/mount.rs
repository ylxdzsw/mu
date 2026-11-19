use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const HANDOFF_ENV: &str = "MU_MOUNT_HANDOFF";
pub type Mounts = BTreeMap<String, Mount>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mount {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
}

pub fn split_cwd(value: &str) -> Option<(&str, &str)> {
    let (id, path) = value.split_once(':')?;
    valid_id(id).then_some((id, path))
}

pub fn registration(id: &str, argv: &[String], cwd: &Path) -> Result<Mounts> {
    ensure!(
        valid_id(id),
        "mount ID must contain only ASCII letters, digits, '.', '_' or '-'"
    );
    let (program, args) = argv
        .split_first()
        .context("registration requires ID -- PROGRAM [ARG ...]")?;
    let cwd = cwd.canonicalize()?;
    let executable = |path: &Path| {
        path.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let path = if program.contains('/') {
        cwd.join(program)
    } else {
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|dir| cwd.join(dir).join(program))
            .find(|p| executable(p))
            .with_context(|| format!("executable `{program}` not found in PATH"))?
    };
    ensure!(
        executable(&path),
        "not an executable file: {}",
        path.display()
    );
    // Keep the selected executable name: multi-call programs can dispatch by argv[0].
    Ok([(
        id.to_owned(),
        Mount {
            program: path,
            args: args.to_vec(),
            cwd,
        },
    )]
    .into())
}

/// Merge registrations without changing an existing binding.
///
/// Identical registrations are idempotent. Conflicting registrations fail
/// without modifying `target`.
pub fn merge(target: &mut Mounts, incoming: &Mounts) -> Result<()> {
    for (name, mount) in incoming {
        if let Some(existing) = target.get(name)
            && existing != mount
        {
            bail!("mount `{name}` is already registered with a different binding");
        }
    }
    target.extend(
        incoming
            .iter()
            .map(|(name, mount)| (name.clone(), mount.clone())),
    );
    Ok(())
}

// One private, short-lived file per running claim. The first line describes the
// session and its committed registry; children append registrations under lock.
// The parent retains an independent file offset just past the header, so the
// common (no registrations) completion only has to check for EOF.
pub struct Handoff {
    pub path: PathBuf,
    file: File,
}

impl Handoff {
    pub fn create(session: &str, mounts: &Mounts) -> Result<Self> {
        let (file, path) =
            crate::random::create_temp_file(&crate::paths::runtime_dir()?, "mounts-", ".jsonl")?;
        let mut handoff = Self { path, file };
        serde_json::to_writer(&mut handoff.file, &(session, mounts))?;
        handoff.file.write_all(b"\n")?;
        Ok(handoff)
    }

    pub fn staged(&mut self) -> Result<Mounts> {
        read_deltas(BufReader::new(&self.file))
    }
}

impl Drop for Handoff {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn read_deltas(reader: impl BufRead) -> Result<Mounts> {
    let mut mounts = Mounts::new();
    for line in reader.lines() {
        merge(&mut mounts, &serde_json::from_str(&line?)?)?;
    }
    Ok(mounts)
}

pub fn in_call(path: &Path, session: &str, add: &Mounts) -> Result<Mounts> {
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .context("opening live mount handoff")?;
    file.lock()?;
    let mut reader = BufReader::new(&file);
    let mut header = String::new();
    reader.read_line(&mut header)?;
    let (owner, mut visible): (String, Mounts) = serde_json::from_str(&header)?;
    ensure!(
        owner == session,
        "mount handoff belongs to {owner}, not {session}"
    );
    merge(&mut visible, &read_deltas(reader)?)?;
    let before = visible.len();
    merge(&mut visible, add)?;
    if visible.len() != before {
        writeln!(file, "{}", serde_json::to_string(add)?)?;
    }
    Ok(visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_leading_mount_id_changes_local_path_semantics() {
        assert_eq!(split_cwd("deck:"), Some(("deck", "")));
        assert_eq!(split_cwd(r"deck:C:\Temp"), Some(("deck", r"C:\Temp")));
        for local in ["/tmp/a:b", "./deck:foo", "dir/file:123", "."] {
            assert_eq!(split_cwd(local), None);
        }
    }

    #[test]
    fn handoff_returns_only_new_registrations_and_rejects_rebinding() {
        let m = Mount {
            program: "/bin/echo".into(),
            args: vec![],
            cwd: "/tmp".into(),
        };
        let mut h = Handoff::create("parent", &[("old".into(), m.clone())].into()).unwrap();
        let add = [("new".into(), m.clone())].into();
        for _ in 0..2 {
            assert_eq!(in_call(&h.path, "parent", &add).unwrap().len(), 2);
        }
        assert!(in_call(&h.path, "child", &add).is_err());
        let conflict = [(
            "new".into(),
            Mount {
                args: vec!["other".into()],
                ..m
            },
        )]
        .into();
        assert!(in_call(&h.path, "parent", &conflict).is_err());
        assert_eq!(h.staged().unwrap(), add);
    }
}
