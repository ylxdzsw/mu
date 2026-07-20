use std::ffi::OsStr;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applet {
    ApplyPatch,
    Edit,
    ViewImage,
}

pub fn from_argv0(argv0: &OsStr) -> Option<Applet> {
    match Path::new(argv0).file_name().and_then(OsStr::to_str) {
        Some("apply_patch") => Some(Applet::ApplyPatch),
        Some("edit") => Some(Applet::Edit),
        Some("view_image") => Some(Applet::ViewImage),
        _ => None,
    }
}

pub fn dispatch(applet: Applet) -> i32 {
    match applet {
        Applet::ApplyPatch => apply_patch::main(),
        Applet::Edit => edit::main(),
        Applet::ViewImage => view_image::main(),
    }
}

mod apply_patch {
    use std::collections::HashSet;
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Write};
    use std::path::{Component, Path, PathBuf};

    use anyhow::{Context, Result, bail};

    #[derive(Debug)]
    enum Operation {
        Add {
            path: PathBuf,
            content: String,
        },
        Delete {
            path: PathBuf,
        },
        Update {
            path: PathBuf,
            move_to: Option<PathBuf>,
            chunks: Vec<Chunk>,
        },
    }

    #[derive(Debug)]
    struct Chunk {
        locator: Option<String>,
        lines: Vec<HunkLine>,
        end_of_file: bool,
    }

    #[derive(Debug)]
    enum HunkLine {
        Context(String),
        Remove(String),
        Add(String),
    }

    #[derive(Debug)]
    enum PlannedChange {
        Add {
            path: PathBuf,
            content: String,
        },
        Delete {
            path: PathBuf,
        },
        Update {
            path: PathBuf,
            reported_path: PathBuf,
            content: String,
            permissions: fs::Permissions,
        },
        Move {
            from: PathBuf,
            to: PathBuf,
            content: String,
            permissions: fs::Permissions,
        },
        MoveSymlink {
            from: PathBuf,
            to: PathBuf,
            target_update: Option<(PathBuf, String, fs::Permissions)>,
        },
    }

    pub fn main() -> i32 {
        if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--help"))
            && std::env::args_os().nth(2).is_none()
        {
            println!(
                "Usage: apply_patch 'PATCH'\n       apply_patch < patch.txt\n\nApply a Mu/Codex-style *** Begin Patch envelope. Relative paths resolve from the current directory; absolute paths are used as written."
            );
            return 0;
        }
        match read_patch().and_then(|patch| run(&patch)) {
            Ok(summary) => {
                print!("{summary}");
                let _ = std::io::stdout().flush();
                0
            }
            Err(error) => {
                eprintln!("apply_patch: {error:#}");
                1
            }
        }
    }

    fn read_patch() -> Result<String> {
        let mut args = std::env::args_os();
        let _ = args.next();
        let patch = match args.next() {
            Some(value) => value
                .into_string()
                .map_err(|_| anyhow::anyhow!("patch argument must be UTF-8"))?,
            None => {
                let mut patch = String::new();
                std::io::stdin()
                    .read_to_string(&mut patch)
                    .context("reading patch from stdin")?;
                if patch.is_empty() {
                    bail!("expected one PATCH argument or patch text on stdin");
                }
                patch
            }
        };
        if args.next().is_some() {
            bail!("accepts exactly one PATCH argument");
        }
        Ok(patch)
    }

    fn run(patch: &str) -> Result<String> {
        let operations = parse_patch(patch)?;
        let cwd = std::env::current_dir().context("determining current directory")?;
        let changes = preflight(&cwd, operations)?;
        commit(&changes)?;
        Ok(format_summary(&changes, &cwd))
    }

    fn parse_patch(patch: &str) -> Result<Vec<Operation>> {
        let normalized = patch.replace("\r\n", "\n");
        let mut lines = normalized.lines().peekable();
        if lines.next() != Some("*** Begin Patch") {
            bail!("patch must begin with `*** Begin Patch`");
        }
        let mut operations = Vec::new();
        loop {
            let Some(line) = lines.next() else {
                bail!("patch is missing `*** End Patch`");
            };
            if line == "*** End Patch" {
                if lines.next().is_some() {
                    bail!("unexpected content after `*** End Patch`");
                }
                break;
            }
            if let Some(path) = line.strip_prefix("*** Add File: ") {
                let path = parse_path(path)?;
                let mut content = Vec::new();
                while let Some(next) = lines.peek() {
                    if next.starts_with("*** ") {
                        break;
                    }
                    let next = lines.next().expect("peeked line");
                    let Some(added) = next.strip_prefix('+') else {
                        bail!("add-file content lines must begin with `+`");
                    };
                    content.push(added.to_string());
                }
                let content = if content.is_empty() {
                    String::new()
                } else {
                    format!("{}\n", content.join("\n"))
                };
                operations.push(Operation::Add { path, content });
                continue;
            }
            if let Some(path) = line.strip_prefix("*** Delete File: ") {
                operations.push(Operation::Delete {
                    path: parse_path(path)?,
                });
                continue;
            }
            if let Some(path) = line.strip_prefix("*** Update File: ") {
                let path = parse_path(path)?;
                let move_to = lines
                    .peek()
                    .and_then(|line| line.strip_prefix("*** Move to: "))
                    .map(parse_path)
                    .transpose()?;
                if move_to.is_some() {
                    lines.next();
                }
                let mut chunks = Vec::new();
                while lines.peek().is_some_and(|line| line.starts_with("@@")) {
                    let header = lines.next().expect("peeked hunk header");
                    let locator = header
                        .strip_prefix("@@")
                        .expect("checked prefix")
                        .trim()
                        .to_string();
                    let locator = (!locator.is_empty()).then_some(locator);
                    let mut hunk_lines = Vec::new();
                    let mut end_of_file = false;
                    while let Some(next) = lines.peek() {
                        if next.starts_with("@@") || next.starts_with("*** ") {
                            if *next == "*** End of File" {
                                lines.next();
                                end_of_file = true;
                            }
                            break;
                        }
                        let next = lines.next().expect("peeked hunk line");
                        let (prefix, text) = next.split_at_checked(1).ok_or_else(|| {
                            anyhow::anyhow!("empty hunk line must be written as a single space")
                        })?;
                        let line = match prefix {
                            " " => HunkLine::Context(text.to_string()),
                            "-" => HunkLine::Remove(text.to_string()),
                            "+" => HunkLine::Add(text.to_string()),
                            _ => bail!("hunk lines must begin with space, `-`, or `+`"),
                        };
                        hunk_lines.push(line);
                    }
                    if hunk_lines.is_empty() {
                        bail!("update hunk must contain at least one line");
                    }
                    chunks.push(Chunk {
                        locator,
                        lines: hunk_lines,
                        end_of_file,
                    });
                }
                if chunks.is_empty() && move_to.is_none() {
                    bail!("update-file operation needs a hunk or move destination");
                }
                operations.push(Operation::Update {
                    path,
                    move_to,
                    chunks,
                });
                continue;
            }
            bail!("unrecognized patch line `{line}`");
        }
        if operations.is_empty() {
            bail!("patch contains no file operations");
        }
        Ok(operations)
    }

    fn parse_path(path: &str) -> Result<PathBuf> {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            bail!("patch path cannot be empty");
        }
        Ok(path)
    }

    fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    }

    fn preflight(cwd: &Path, operations: Vec<Operation>) -> Result<Vec<PlannedChange>> {
        let mut touched = HashSet::new();
        let mut changes = Vec::new();
        for operation in operations {
            match operation {
                Operation::Add { path, content } => {
                    let full = resolve_path(cwd, &path);
                    claim_path(&mut touched, &full, &path)?;
                    if fs::symlink_metadata(&full).is_ok() {
                        bail!(
                            "add destination already exists: {}; inspect it first, then use bash to move or remove the existing file if necessary before retrying apply_patch",
                            path.display()
                        );
                    }
                    changes.push(PlannedChange::Add {
                        path: full,
                        content,
                    });
                }
                Operation::Delete { path } => {
                    let full = resolve_path(cwd, &path);
                    claim_path(&mut touched, &full, &path)?;
                    file_or_symlink_metadata(&full, "delete")?;
                    changes.push(PlannedChange::Delete { path: full });
                }
                Operation::Update {
                    path,
                    move_to,
                    chunks,
                } => {
                    let full = resolve_path(cwd, &path);
                    claim_path(&mut touched, &full, &path)?;
                    let destination_full = move_to
                        .as_ref()
                        .map(|destination| resolve_path(cwd, destination));
                    if let (Some(destination), Some(destination_full)) =
                        (&move_to, &destination_full)
                    {
                        claim_path(&mut touched, destination_full, destination)?;
                        if fs::symlink_metadata(destination_full).is_ok() {
                            bail!(
                                "move destination already exists: {}; inspect it first, then use bash to move or remove the existing file if necessary before retrying apply_patch",
                                destination.display()
                            );
                        }
                    }

                    let entry_metadata = fs::symlink_metadata(&full).with_context(|| {
                        format!("cannot update missing file {}", path.display())
                    })?;
                    if entry_metadata.file_type().is_symlink() {
                        if chunks.is_empty() {
                            let destination = destination_full
                                .expect("an update without chunks must have a move destination");
                            changes.push(PlannedChange::MoveSymlink {
                                from: full,
                                to: destination,
                                target_update: None,
                            });
                            continue;
                        }
                        let target = fs::canonicalize(&full).with_context(|| {
                            format!("resolving symlink to update {}", path.display())
                        })?;
                        claim_path(&mut touched, &target, &path)?;
                        let metadata = regular_file_metadata(&target, "update symlink target")?;
                        let original = fs::read_to_string(&target).with_context(|| {
                            format!("reading symlink target to update {}", path.display())
                        })?;
                        let content = apply_chunks(&original, &path, &chunks)?;
                        if let Some(destination) = destination_full {
                            changes.push(PlannedChange::MoveSymlink {
                                from: full,
                                to: destination,
                                target_update: Some((target, content, metadata.permissions())),
                            });
                        } else {
                            changes.push(PlannedChange::Update {
                                path: target,
                                reported_path: full,
                                content,
                                permissions: metadata.permissions(),
                            });
                        }
                        continue;
                    }
                    if !entry_metadata.is_file() {
                        bail!("cannot update non-regular file {}", path.display());
                    }
                    let original = fs::read_to_string(&full)
                        .with_context(|| format!("reading file to update {}", path.display()))?;
                    let content = if chunks.is_empty() {
                        original
                    } else {
                        apply_chunks(&original, &path, &chunks)?
                    };
                    if let Some(destination_full) = destination_full {
                        changes.push(PlannedChange::Move {
                            from: full,
                            to: destination_full,
                            content,
                            permissions: entry_metadata.permissions(),
                        });
                    } else {
                        changes.push(PlannedChange::Update {
                            reported_path: full.clone(),
                            path: full,
                            content,
                            permissions: entry_metadata.permissions(),
                        });
                    }
                }
            }
        }
        Ok(changes)
    }

    fn claim_path(touched: &mut HashSet<PathBuf>, resolved: &Path, reported: &Path) -> Result<()> {
        if !touched.insert(normalize_path(resolved)) {
            bail!(
                "patch contains conflicting operations for {}",
                reported.display()
            );
        }
        Ok(())
    }

    fn normalize_path(path: &Path) -> PathBuf {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => normalized.push(part),
                Component::ParentDir => {
                    if normalized.file_name() == Some(std::ffi::OsStr::new("..")) {
                        normalized.push("..");
                    } else {
                        let _ = normalized.pop();
                    }
                }
                Component::RootDir => normalized.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
                Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            }
        }
        normalized
    }

    fn regular_file_metadata(path: &Path, action: &str) -> Result<fs::Metadata> {
        let metadata = fs::metadata(path)
            .with_context(|| format!("cannot {action} missing file {}", path.display()))?;
        if !metadata.is_file() {
            bail!("cannot {action} non-regular file {}", path.display());
        }
        Ok(metadata)
    }

    fn file_or_symlink_metadata(path: &Path, action: &str) -> Result<fs::Metadata> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("cannot {action} missing file {}", path.display()))?;
        if !metadata.is_file() && !metadata.file_type().is_symlink() {
            bail!("cannot {action} non-file {}", path.display());
        }
        Ok(metadata)
    }

    fn apply_chunks(original: &str, path: &Path, chunks: &[Chunk]) -> Result<String> {
        let mut lines = original.split('\n').map(str::to_string).collect::<Vec<_>>();
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        let mut cursor = 0usize;
        for chunk in chunks {
            if let Some(locator) = &chunk.locator {
                let index = seek_sequence(&lines, std::slice::from_ref(locator), cursor, false)
                    .with_context(|| {
                        format!("failed to find context `{locator}` in {}", path.display())
                    })?;
                cursor = index + 1;
            }
            let old = chunk
                .lines
                .iter()
                .filter_map(|line| match line {
                    HunkLine::Context(text) | HunkLine::Remove(text) => Some(text.clone()),
                    HunkLine::Add(_) => None,
                })
                .collect::<Vec<_>>();
            let new = chunk
                .lines
                .iter()
                .filter_map(|line| match line {
                    HunkLine::Context(text) | HunkLine::Add(text) => Some(text.clone()),
                    HunkLine::Remove(_) => None,
                })
                .collect::<Vec<_>>();
            let index = if old.is_empty() {
                if chunk.end_of_file {
                    lines.len()
                } else {
                    cursor
                }
            } else {
                seek_sequence(&lines, &old, cursor, chunk.end_of_file).with_context(|| {
                    format!(
                        "failed to find expected lines in {}:\n{}",
                        path.display(),
                        old.join("\n")
                    )
                })?
            };
            lines.splice(index..index + old.len(), new.iter().cloned());
            cursor = index + new.len();
        }
        Ok(format!("{}\n", lines.join("\n")))
    }

    fn seek_sequence(
        lines: &[String],
        pattern: &[String],
        start: usize,
        eof: bool,
    ) -> Option<usize> {
        if pattern.is_empty() {
            return Some(start.min(lines.len()));
        }
        if pattern.len() > lines.len() {
            return None;
        }
        let preferred = if eof {
            lines.len().saturating_sub(pattern.len())
        } else {
            start.min(lines.len())
        };
        for matcher in [
            exact_lines as fn(&[String], &[String]) -> bool,
            trim_end_lines,
            trim_lines,
            normalize_lines,
        ] {
            if eof && matcher(&lines[preferred..preferred + pattern.len()], pattern) {
                return Some(preferred);
            }
            for index in start.min(lines.len())..=lines.len() - pattern.len() {
                if matcher(&lines[index..index + pattern.len()], pattern) {
                    return Some(index);
                }
            }
        }
        None
    }

    fn exact_lines(actual: &[String], expected: &[String]) -> bool {
        actual == expected
    }

    fn trim_end_lines(actual: &[String], expected: &[String]) -> bool {
        actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.trim_end() == expected.trim_end())
    }

    fn trim_lines(actual: &[String], expected: &[String]) -> bool {
        actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| actual.trim() == expected.trim())
    }

    fn normalize_lines(actual: &[String], expected: &[String]) -> bool {
        actual
            .iter()
            .zip(expected)
            .all(|(actual, expected)| normalize_line(actual) == normalize_line(expected))
    }

    fn normalize_line(line: &str) -> String {
        line.trim()
            .chars()
            .map(|character| match character {
                '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
                | '\u{2212}' => '-',
                '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => '\'',
                '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => '"',
                '\u{00a0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
                | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200a}' | '\u{202f}' | '\u{205f}'
                | '\u{3000}' => ' ',
                other => other,
            })
            .collect()
    }

    fn commit(changes: &[PlannedChange]) -> Result<()> {
        let mut completed = Vec::new();
        for change in changes {
            let result = match change {
                PlannedChange::Add { path, content } => atomic_write(path, content, None, false),
                PlannedChange::Delete { path } => {
                    fs::remove_file(path).with_context(|| format!("deleting {}", path.display()))
                }
                PlannedChange::Update {
                    path,
                    reported_path: _,
                    content,
                    permissions,
                } => atomic_write(path, content, Some(permissions.clone()), true),
                PlannedChange::Move {
                    from,
                    to,
                    content,
                    permissions,
                } => atomic_write(to, content, Some(permissions.clone()), false).and_then(|()| {
                    fs::remove_file(from).with_context(|| format!("removing {}", from.display()))
                }),
                PlannedChange::MoveSymlink {
                    from,
                    to,
                    target_update,
                } => (|| -> Result<()> {
                    let parent = to.parent().unwrap_or_else(|| Path::new("."));
                    fs::create_dir_all(parent).with_context(|| {
                        format!("creating parent directory {}", parent.display())
                    })?;
                    fs::rename(from, to).with_context(|| {
                        format!("moving symlink {} to {}", from.display(), to.display())
                    })?;
                    if let Some((target, content, permissions)) = target_update
                        && let Err(error) =
                            atomic_write(target, content, Some(permissions.clone()), true)
                    {
                        return match fs::rename(to, from) {
                            Ok(()) => Err(error.context(format!(
                                "updating symlink target after moving {}; move rolled back",
                                target.display()
                            ))),
                            Err(rollback_error) => Err(error.context(format!(
                                "updating symlink target after moving {}; also failed to roll back {} to {}: {rollback_error}",
                                target.display(),
                                to.display(),
                                from.display()
                            ))),
                        };
                    }
                    Ok(())
                })(),
            };
            if let Err(error) = result {
                let completed = if completed.is_empty() {
                    "none".to_string()
                } else {
                    completed.join(", ")
                };
                return Err(error.context(format!("completed changes before failure: {completed}")));
            }
            completed.push(change_label(change));
        }
        Ok(())
    }

    pub(super) fn atomic_write(
        path: &Path,
        content: &str,
        permissions: Option<fs::Permissions>,
        replace: bool,
    ) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)
            .with_context(|| format!("creating parent directory {}", parent.display()))?;
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let temporary = parent.join(format!(".{filename}.mu-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .with_context(|| format!("creating temporary file {}", temporary.display()))?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            if let Some(permissions) = permissions {
                file.set_permissions(permissions)?;
            }
            drop(file);
            if replace {
                fs::rename(&temporary, path)
                    .with_context(|| format!("replacing {}", path.display()))?;
            } else {
                fs::hard_link(&temporary, path)
                    .with_context(|| format!("creating {} without overwriting", path.display()))?;
                fs::remove_file(&temporary)?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn format_summary(changes: &[PlannedChange], cwd: &Path) -> String {
        let mut summary = String::from("Done!\n");
        for change in changes {
            summary.push_str(&change_label_relative(change, cwd));
            summary.push('\n');
        }
        summary
    }

    fn change_label_relative(change: &PlannedChange, cwd: &Path) -> String {
        let relative = |path: &Path| path.strip_prefix(cwd).unwrap_or(path).display().to_string();
        match change {
            PlannedChange::Add { path, .. } => format!("A {}", relative(path)),
            PlannedChange::Delete { path } => format!("D {}", relative(path)),
            PlannedChange::Update { reported_path, .. } => {
                format!("M {}", relative(reported_path))
            }
            PlannedChange::Move { from, to, .. } => {
                format!("R {} -> {}", relative(from), relative(to))
            }
            PlannedChange::MoveSymlink { from, to, .. } => {
                format!("R {} -> {}", relative(from), relative(to))
            }
        }
    }

    fn change_label(change: &PlannedChange) -> String {
        match change {
            PlannedChange::Add { path, .. } => format!("A {}", path.display()),
            PlannedChange::Delete { path } => format!("D {}", path.display()),
            PlannedChange::Update { reported_path, .. } => {
                format!("M {}", reported_path.display())
            }
            PlannedChange::Move { from, to, .. } => {
                format!("R {} -> {}", from.display(), to.display())
            }
            PlannedChange::MoveSymlink { from, to, .. } => {
                format!("R {} -> {}", from.display(), to.display())
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn temp_dir() -> PathBuf {
            let path =
                std::env::temp_dir().join(format!("mu-apply-patch-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            path
        }

        #[test]
        fn parses_and_applies_add_update_move_delete() {
            let dir = temp_dir();
            fs::write(dir.join("old.txt"), "one\ntwo\n").unwrap();
            fs::write(dir.join("delete.txt"), "bye\n").unwrap();
            let patch = "*** Begin Patch\n*** Add File: added.txt\n+hello\n*** Update File: old.txt\n*** Move to: moved.txt\n@@\n one\n-two\n+three\n*** Delete File: delete.txt\n*** End Patch\n";
            let changes = preflight(&dir, parse_patch(patch).unwrap()).unwrap();
            commit(&changes).unwrap();
            assert_eq!(
                fs::read_to_string(dir.join("added.txt")).unwrap(),
                "hello\n"
            );
            assert_eq!(
                fs::read_to_string(dir.join("moved.txt")).unwrap(),
                "one\nthree\n"
            );
            assert!(!dir.join("old.txt").exists());
            assert!(!dir.join("delete.txt").exists());
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn failed_preflight_leaves_every_file_unchanged() {
            let dir = temp_dir();
            fs::write(dir.join("one.txt"), "old\n").unwrap();
            fs::write(dir.join("exists.txt"), "keep\n").unwrap();
            let patch = "*** Begin Patch\n*** Update File: one.txt\n@@\n-old\n+new\n*** Add File: exists.txt\n+replace\n*** End Patch\n";
            let error = preflight(&dir, parse_patch(patch).unwrap()).unwrap_err();
            assert!(error.to_string().contains("inspect it first"));
            assert!(error.to_string().contains("use bash to move or remove"));
            assert_eq!(fs::read_to_string(dir.join("one.txt")).unwrap(), "old\n");
            assert_eq!(
                fs::read_to_string(dir.join("exists.txt")).unwrap(),
                "keep\n"
            );
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn existing_move_destination_error_is_actionable() {
            let dir = temp_dir();
            fs::write(dir.join("source.txt"), "source\n").unwrap();
            fs::write(dir.join("destination.txt"), "destination\n").unwrap();
            let patch = "*** Begin Patch\n*** Update File: source.txt\n*** Move to: destination.txt\n*** End Patch\n";
            let error = preflight(&dir, parse_patch(patch).unwrap()).unwrap_err();
            let message = error.to_string();
            assert!(message.contains("move destination already exists"));
            assert!(message.contains("inspect it first"));
            assert!(message.contains("use bash to move or remove"));
            assert_eq!(
                fs::read_to_string(dir.join("destination.txt")).unwrap(),
                "destination\n"
            );
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn fuzzy_context_matches_trailing_whitespace() {
            let original = "fn main() {   \n    old();\n}\n";
            let chunks = vec![Chunk {
                locator: None,
                lines: vec![
                    HunkLine::Context("fn main() {".into()),
                    HunkLine::Remove("    old();".into()),
                    HunkLine::Add("    new();".into()),
                    HunkLine::Context("}".into()),
                ],
                end_of_file: false,
            }];
            assert_eq!(
                apply_chunks(original, Path::new("main.rs"), &chunks).unwrap(),
                "fn main() {\n    new();\n}\n"
            );
        }

        #[test]
        fn accepts_absolute_paths() {
            let dir = temp_dir();
            let path = dir.join("absolute.txt");
            let patch = format!(
                "*** Begin Patch\n*** Add File: {}\n+absolute\n*** End Patch\n",
                path.display()
            );
            let changes = preflight(&dir, parse_patch(&patch).unwrap()).unwrap();
            commit(&changes).unwrap();
            assert_eq!(fs::read_to_string(&path).unwrap(), "absolute\n");
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn rejects_lexically_aliased_operations() {
            let dir = temp_dir();
            let patch = "*** Begin Patch\n*** Add File: sub/../same.txt\n+one\n*** Add File: same.txt\n+two\n*** End Patch\n";
            let error = preflight(&dir, parse_patch(patch).unwrap()).unwrap_err();
            assert!(error.to_string().contains("conflicting operations"));
            fs::remove_dir_all(dir).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn update_through_symlink_preserves_link_and_updates_target() {
            use std::os::unix::fs::symlink;

            let dir = temp_dir();
            fs::write(dir.join("target.txt"), "old\n").unwrap();
            symlink("target.txt", dir.join("link.txt")).unwrap();
            let patch =
                "*** Begin Patch\n*** Update File: link.txt\n@@\n-old\n+new\n*** End Patch\n";
            let changes = preflight(&dir, parse_patch(patch).unwrap()).unwrap();
            commit(&changes).unwrap();
            assert!(
                fs::symlink_metadata(dir.join("link.txt"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read_to_string(dir.join("target.txt")).unwrap(), "new\n");
            fs::remove_dir_all(dir).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn delete_symlink_removes_only_the_link() {
            use std::os::unix::fs::symlink;

            let dir = temp_dir();
            fs::write(dir.join("target.txt"), "keep\n").unwrap();
            symlink("target.txt", dir.join("link.txt")).unwrap();
            let patch = "*** Begin Patch\n*** Delete File: link.txt\n*** End Patch\n";
            let changes = preflight(&dir, parse_patch(patch).unwrap()).unwrap();
            commit(&changes).unwrap();
            assert!(fs::symlink_metadata(dir.join("link.txt")).is_err());
            assert_eq!(
                fs::read_to_string(dir.join("target.txt")).unwrap(),
                "keep\n"
            );
            fs::remove_dir_all(dir).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn pure_move_renames_symlink_without_touching_target() {
            use std::os::unix::fs::symlink;

            let dir = temp_dir();
            fs::write(dir.join("target.txt"), "keep\n").unwrap();
            symlink("target.txt", dir.join("link.txt")).unwrap();
            let patch = "*** Begin Patch\n*** Update File: link.txt\n*** Move to: moved.txt\n*** End Patch\n";
            let changes = preflight(&dir, parse_patch(patch).unwrap()).unwrap();
            commit(&changes).unwrap();
            assert!(fs::symlink_metadata(dir.join("link.txt")).is_err());
            assert!(
                fs::symlink_metadata(dir.join("moved.txt"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(
                fs::read_to_string(dir.join("target.txt")).unwrap(),
                "keep\n"
            );
            fs::remove_dir_all(dir).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn move_with_update_edits_target_and_renames_symlink() {
            use std::os::unix::fs::symlink;

            let dir = temp_dir();
            fs::write(dir.join("target.txt"), "old\n").unwrap();
            symlink("target.txt", dir.join("link.txt")).unwrap();
            let patch = "*** Begin Patch\n*** Update File: link.txt\n*** Move to: moved.txt\n@@\n-old\n+new\n*** End Patch\n";
            let changes = preflight(&dir, parse_patch(patch).unwrap()).unwrap();
            commit(&changes).unwrap();
            assert!(fs::symlink_metadata(dir.join("link.txt")).is_err());
            assert!(
                fs::symlink_metadata(dir.join("moved.txt"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read_to_string(dir.join("target.txt")).unwrap(), "new\n");
            fs::remove_dir_all(dir).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn failed_symlink_move_does_not_update_target() {
            use std::os::unix::fs::symlink;

            let dir = temp_dir();
            fs::write(dir.join("target.txt"), "old\n").unwrap();
            fs::write(dir.join("not-a-directory"), "blocking file\n").unwrap();
            symlink("target.txt", dir.join("link.txt")).unwrap();
            let patch = "*** Begin Patch\n*** Update File: link.txt\n*** Move to: not-a-directory/moved.txt\n@@\n-old\n+new\n*** End Patch\n";
            let changes = preflight(&dir, parse_patch(patch).unwrap()).unwrap();
            commit(&changes).unwrap_err();
            assert!(
                fs::symlink_metadata(dir.join("link.txt"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read_to_string(dir.join("target.txt")).unwrap(), "old\n");
            fs::remove_dir_all(dir).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn rejects_updates_through_two_symlinks_to_the_same_target() {
            use std::os::unix::fs::symlink;

            let dir = temp_dir();
            fs::write(dir.join("target.txt"), "old\n").unwrap();
            symlink("target.txt", dir.join("one.txt")).unwrap();
            symlink("target.txt", dir.join("two.txt")).unwrap();
            let patch = "*** Begin Patch\n*** Update File: one.txt\n@@\n-old\n+one\n*** Update File: two.txt\n@@\n-old\n+two\n*** End Patch\n";
            let error = preflight(&dir, parse_patch(patch).unwrap()).unwrap_err();
            assert!(error.to_string().contains("conflicting operations"));
            assert_eq!(fs::read_to_string(dir.join("target.txt")).unwrap(), "old\n");
            fs::remove_dir_all(dir).unwrap();
        }
    }
}

mod edit {
    use std::fs;
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};

    use anyhow::{Context, Result, bail};
    use clap::Parser;

    #[derive(Debug, Parser)]
    #[command(
        name = "edit",
        about = "Replace exact text in an existing file",
        after_help = "Pass one or more SEARCH/REPLACE blocks on stdin:\n\n<<<<<<< SEARCH\nexact existing text\n=======\nreplacement text\n>>>>>>> REPLACE"
    )]
    struct Args {
        /// Replace every occurrence of each SEARCH block
        #[arg(long)]
        all: bool,
        /// Existing file to edit
        file: PathBuf,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct Block {
        search: String,
        replacement: String,
    }

    #[derive(Debug)]
    struct Line {
        number: usize,
        start: usize,
        content_end: usize,
    }

    #[derive(Debug)]
    struct Match {
        start: usize,
        end: usize,
        block: usize,
    }

    #[derive(Debug)]
    struct PlannedEdit {
        target: PathBuf,
        reported_path: PathBuf,
        content: String,
        permissions: fs::Permissions,
        blocks: usize,
        replacements: usize,
    }

    pub fn main() -> i32 {
        let args = match Args::try_parse() {
            Ok(args) => args,
            Err(error) => {
                let _ = error.print();
                return error.exit_code();
            }
        };
        match read_document().and_then(|document| run(args, &document)) {
            Ok(summary) => {
                print!("{summary}");
                let _ = std::io::stdout().flush();
                0
            }
            Err(error) => {
                eprintln!("edit: {error:#}");
                1
            }
        }
    }

    fn read_document() -> Result<String> {
        let mut document = String::new();
        std::io::stdin()
            .read_to_string(&mut document)
            .context("reading edit document from stdin")?;
        if document.is_empty() {
            bail!("expected one or more SEARCH/REPLACE blocks on stdin");
        }
        Ok(document)
    }

    fn run(args: Args, document: &str) -> Result<String> {
        let blocks = parse_document(document)?;
        let cwd = std::env::current_dir().context("determining current directory")?;
        let planned = preflight(&cwd, args.file, blocks, args.all)?;
        super::apply_patch::atomic_write(
            &planned.target,
            &planned.content,
            Some(planned.permissions.clone()),
            true,
        )?;
        Ok(format_summary(&planned, &cwd))
    }

    fn parse_document(document: &str) -> Result<Vec<Block>> {
        let lines = scan_lines(document);
        let mut blocks = Vec::new();
        let mut index = 0;

        while index < lines.len() {
            if index + 1 == lines.len() && line_text(document, &lines[index]).is_empty() {
                break;
            }
            let block_number = blocks.len() + 1;
            expect_marker(document, &lines[index], "<<<<<<< SEARCH", block_number)?;
            index += 1;

            let search_start = index;
            while index < lines.len() && line_text(document, &lines[index]) != "=======" {
                if is_marker(line_text(document, &lines[index])) {
                    bail!(
                        "block {block_number} expected `=======` at line {}, found `{}`",
                        lines[index].number,
                        line_text(document, &lines[index])
                    );
                }
                index += 1;
            }
            if index == lines.len() {
                bail!("block {block_number} is missing `=======`");
            }
            let search = body_text(document, &lines[search_start..index]).to_string();
            if search.is_empty() {
                bail!("block {block_number} has an empty SEARCH section");
            }
            index += 1;

            let replacement_start = index;
            while index < lines.len() && line_text(document, &lines[index]) != ">>>>>>> REPLACE" {
                if is_marker(line_text(document, &lines[index])) {
                    bail!(
                        "block {block_number} expected `>>>>>>> REPLACE` at line {}, found `{}`",
                        lines[index].number,
                        line_text(document, &lines[index])
                    );
                }
                index += 1;
            }
            if index == lines.len() {
                bail!("block {block_number} is missing `>>>>>>> REPLACE`");
            }
            let replacement = body_text(document, &lines[replacement_start..index]).to_string();
            blocks.push(Block {
                search,
                replacement,
            });
            index += 1;
        }

        if blocks.is_empty() {
            bail!("edit document contains no SEARCH/REPLACE blocks");
        }
        Ok(blocks)
    }

    fn scan_lines(text: &str) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut start = 0;
        for (number, newline) in text.match_indices('\n').enumerate() {
            let raw_content_end = newline.0;
            let content_end =
                if text.as_bytes().get(raw_content_end.wrapping_sub(1)) == Some(&b'\r') {
                    raw_content_end - 1
                } else {
                    raw_content_end
                };
            lines.push(Line {
                number: number + 1,
                start,
                content_end,
            });
            start = raw_content_end + 1;
        }
        lines.push(Line {
            number: lines.len() + 1,
            start,
            content_end: text.len(),
        });
        lines
    }

    fn line_text<'a>(document: &'a str, line: &Line) -> &'a str {
        &document[line.start..line.content_end]
    }

    fn body_text<'a>(document: &'a str, lines: &[Line]) -> &'a str {
        match (lines.first(), lines.last()) {
            (Some(first), Some(last)) => &document[first.start..last.content_end],
            _ => "",
        }
    }

    fn expect_marker(document: &str, line: &Line, marker: &str, block: usize) -> Result<()> {
        let actual = line_text(document, line);
        if actual != marker {
            bail!(
                "expected `<<<<<<< SEARCH` for block {block} at line {}, found `{actual}`",
                line.number
            );
        }
        Ok(())
    }

    fn is_marker(line: &str) -> bool {
        matches!(line, "<<<<<<< SEARCH" | "=======" | ">>>>>>> REPLACE")
    }

    fn resolve_path(cwd: &Path, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    }

    fn preflight(
        cwd: &Path,
        reported_path: PathBuf,
        blocks: Vec<Block>,
        replace_all: bool,
    ) -> Result<PlannedEdit> {
        let path = resolve_path(cwd, &reported_path);
        let entry_metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("cannot edit missing file {}", reported_path.display()))?;
        let target = if entry_metadata.file_type().is_symlink() {
            fs::canonicalize(&path)
                .with_context(|| format!("resolving symlink to edit {}", reported_path.display()))?
        } else {
            path
        };
        let metadata = fs::metadata(&target)
            .with_context(|| format!("cannot edit missing file {}", reported_path.display()))?;
        if !metadata.is_file() {
            bail!("cannot edit non-regular file {}", reported_path.display());
        }
        let original = fs::read_to_string(&target)
            .with_context(|| format!("reading file to edit {}", reported_path.display()))?;

        let mut matches = Vec::new();
        for (block_index, block) in blocks.iter().enumerate() {
            let locations = find_all(&original, &block.search);
            let block_number = block_index + 1;
            if locations.is_empty() {
                bail!(
                    "block {block_number} did not match {}; re-read the file and copy the exact current text",
                    reported_path.display()
                );
            }
            if !replace_all && locations.len() != 1 {
                bail!(
                    "block {block_number} matched {} locations in {}; add surrounding context or retry with --all",
                    locations.len(),
                    reported_path.display()
                );
            }
            for start in locations {
                matches.push(Match {
                    start,
                    end: start + block.search.len(),
                    block: block_index,
                });
                if !replace_all {
                    break;
                }
            }
        }

        matches.sort_by_key(|found| (found.start, found.end, found.block));
        for pair in matches.windows(2) {
            if pair[1].start < pair[0].end {
                let first = pair[0].block + 1;
                let second = pair[1].block + 1;
                if first == second {
                    bail!(
                        "block {first} has overlapping matches in {}; add surrounding context or edit in separate steps",
                        reported_path.display()
                    );
                }
                bail!(
                    "blocks {first} and {second} overlap in {}; combine them into one block",
                    reported_path.display()
                );
            }
        }

        let replacements = matches.len();
        let mut content = original;
        for found in matches.iter().rev() {
            content.replace_range(found.start..found.end, &blocks[found.block].replacement);
        }
        Ok(PlannedEdit {
            target,
            reported_path,
            content,
            permissions: metadata.permissions(),
            blocks: blocks.len(),
            replacements,
        })
    }

    fn find_all(text: &str, needle: &str) -> Vec<usize> {
        text.as_bytes()
            .windows(needle.len())
            .enumerate()
            .filter_map(|(index, candidate)| (candidate == needle.as_bytes()).then_some(index))
            .collect()
    }

    fn format_summary(edit: &PlannedEdit, cwd: &Path) -> String {
        let path = edit
            .reported_path
            .strip_prefix(cwd)
            .unwrap_or(&edit.reported_path);
        let block_label = if edit.blocks == 1 { "block" } else { "blocks" };
        let replacement_label = if edit.replacements == 1 {
            "replacement"
        } else {
            "replacements"
        };
        format!(
            "Done!\nM {}\nApplied {} {block_label}, {} {replacement_label}.\n",
            path.display(),
            edit.blocks,
            edit.replacements
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn temp_dir() -> PathBuf {
            let path = std::env::temp_dir().join(format!("mu-edit-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            path
        }

        fn block(search: &str, replacement: &str) -> String {
            format!("<<<<<<< SEARCH\n{search}\n=======\n{replacement}\n>>>>>>> REPLACE\n")
        }

        #[test]
        fn parses_file_and_replace_all_flag() {
            let args = Args::try_parse_from(["edit", "--all", "src/main.rs"]).unwrap();
            assert!(args.all);
            assert_eq!(args.file, PathBuf::from("src/main.rs"));
        }

        #[test]
        fn parses_multiple_blocks_and_empty_replacement() {
            let document = concat!(
                "<<<<<<< SEARCH\none\n=======\ntwo\n>>>>>>> REPLACE\n",
                "<<<<<<< SEARCH\nremove me\n=======\n>>>>>>> REPLACE\n"
            );
            assert_eq!(
                parse_document(document).unwrap(),
                vec![
                    Block {
                        search: "one".into(),
                        replacement: "two".into()
                    },
                    Block {
                        search: "remove me".into(),
                        replacement: String::new()
                    }
                ]
            );
        }

        #[test]
        fn framing_newlines_are_not_part_of_the_bodies() {
            let blocks = parse_document(
                "<<<<<<< SEARCH\nfirst\nsecond\n\n=======\nreplacement\n>>>>>>> REPLACE\n",
            )
            .unwrap();
            assert_eq!(blocks[0].search, "first\nsecond\n");
            assert_eq!(blocks[0].replacement, "replacement");
        }

        #[test]
        fn preserves_internal_crlf_line_endings() {
            let blocks = parse_document(
                "<<<<<<< SEARCH\r\nfirst\r\nsecond\r\n=======\r\nnew\r\n>>>>>>> REPLACE\r\n",
            )
            .unwrap();
            assert_eq!(blocks[0].search, "first\r\nsecond");
            assert_eq!(blocks[0].replacement, "new");
        }

        #[test]
        fn rejects_empty_search_and_malformed_documents() {
            let error =
                parse_document("<<<<<<< SEARCH\n=======\nx\n>>>>>>> REPLACE\n").unwrap_err();
            assert!(error.to_string().contains("empty SEARCH"));

            let error = parse_document("<<<<<<< SEARCH\nx\n>>>>>>> REPLACE\n").unwrap_err();
            assert!(error.to_string().contains("expected `=======`"));

            let error = parse_document("not a block\n").unwrap_err();
            assert!(error.to_string().contains("expected `<<<<<<< SEARCH`"));
        }

        #[test]
        fn applies_multiple_blocks_against_the_original_snapshot() {
            let dir = temp_dir();
            fs::write(dir.join("file.txt"), "alpha beta gamma\n").unwrap();
            let blocks = vec![
                Block {
                    search: "alpha".into(),
                    replacement: "A".into(),
                },
                Block {
                    search: "gamma".into(),
                    replacement: "G".into(),
                },
            ];
            let planned = preflight(&dir, PathBuf::from("file.txt"), blocks, false).unwrap();
            assert_eq!(planned.content, "A beta G\n");
            assert_eq!(planned.replacements, 2);
            super::super::apply_patch::atomic_write(
                &planned.target,
                &planned.content,
                Some(planned.permissions.clone()),
                true,
            )
            .unwrap();
            assert_eq!(
                fs::read_to_string(dir.join("file.txt")).unwrap(),
                "A beta G\n"
            );
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn all_replaces_every_non_overlapping_occurrence() {
            let dir = temp_dir();
            fs::write(dir.join("file.txt"), "x x x").unwrap();
            let blocks = parse_document(&block("x", "y")).unwrap();
            let planned = preflight(&dir, PathBuf::from("file.txt"), blocks, true).unwrap();
            assert_eq!(planned.content, "y y y");
            assert_eq!(planned.replacements, 3);
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn empty_replacement_deletes_the_match() {
            let dir = temp_dir();
            fs::write(dir.join("file.txt"), "keep remove keep").unwrap();
            let blocks = parse_document(&block(" remove", "")).unwrap();
            let planned = preflight(&dir, PathBuf::from("file.txt"), blocks, false).unwrap();
            assert_eq!(planned.content, "keep keep");
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn reports_no_match_and_ambiguity_without_writing() {
            let dir = temp_dir();
            let path = dir.join("file.txt");
            fs::write(&path, "same same").unwrap();

            let error = preflight(
                &dir,
                PathBuf::from("file.txt"),
                parse_document(&block("missing", "new")).unwrap(),
                false,
            )
            .unwrap_err();
            assert!(error.to_string().contains("did not match"));

            let error = preflight(
                &dir,
                PathBuf::from("file.txt"),
                parse_document(&block("same", "new")).unwrap(),
                false,
            )
            .unwrap_err();
            assert!(error.to_string().contains("matched 2 locations"));
            assert_eq!(fs::read_to_string(&path).unwrap(), "same same");
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn accepts_absolute_paths_and_rejects_missing_files() {
            let dir = temp_dir();
            let path = dir.join("file.txt");
            fs::write(&path, "old").unwrap();
            let planned = preflight(
                &dir,
                path.clone(),
                parse_document(&block("old", "new")).unwrap(),
                false,
            )
            .unwrap();
            assert_eq!(planned.target, path);
            assert_eq!(planned.content, "new");

            let missing = dir.join("missing.txt");
            let error = preflight(
                &dir,
                missing.clone(),
                parse_document(&block("old", "new")).unwrap(),
                false,
            )
            .unwrap_err();
            assert!(error.to_string().contains("cannot edit missing file"));
            assert!(!missing.exists());
            fs::remove_dir_all(dir).unwrap();
        }

        #[test]
        fn rejects_overlapping_blocks_and_overlapping_all_matches() {
            let dir = temp_dir();
            fs::write(dir.join("file.txt"), "abc").unwrap();
            let blocks = vec![
                Block {
                    search: "ab".into(),
                    replacement: "x".into(),
                },
                Block {
                    search: "bc".into(),
                    replacement: "y".into(),
                },
            ];
            let error = preflight(&dir, PathBuf::from("file.txt"), blocks, false).unwrap_err();
            assert!(error.to_string().contains("blocks 1 and 2 overlap"));

            fs::write(dir.join("file.txt"), "aaa").unwrap();
            let blocks = parse_document(&block("aa", "x")).unwrap();
            let error = preflight(&dir, PathBuf::from("file.txt"), blocks, true).unwrap_err();
            assert!(error.to_string().contains("overlapping matches"));
            fs::remove_dir_all(dir).unwrap();
        }

        #[cfg(unix)]
        #[test]
        fn preserves_permissions_and_updates_through_symlink() {
            use std::os::unix::fs::{PermissionsExt, symlink};

            let dir = temp_dir();
            let target = dir.join("target.txt");
            fs::write(&target, "old").unwrap();
            fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
            symlink("target.txt", dir.join("link.txt")).unwrap();
            let blocks = parse_document(&block("old", "new")).unwrap();
            let planned = preflight(&dir, PathBuf::from("link.txt"), blocks, false).unwrap();
            super::super::apply_patch::atomic_write(
                &planned.target,
                &planned.content,
                Some(planned.permissions.clone()),
                true,
            )
            .unwrap();

            assert!(
                fs::symlink_metadata(dir.join("link.txt"))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
            assert_eq!(fs::read_to_string(&target).unwrap(), "new");
            assert_eq!(
                fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                0o640
            );
            fs::remove_dir_all(dir).unwrap();
        }
    }
}

mod view_image {
    use std::path::PathBuf;

    use clap::Parser;

    use crate::artifact::write_image_artifact;
    use crate::attachment::load_attachment;
    use crate::provider::ImageDetail;

    #[derive(Debug, Parser)]
    #[command(
        name = "view_image",
        about = "Load an image into the current Mu tool result"
    )]
    struct Args {
        #[arg(long, value_enum, default_value_t = ImageDetail::Auto)]
        detail: ImageDetail,
        path: PathBuf,
    }

    pub fn main() -> i32 {
        let args = match Args::try_parse() {
            Ok(args) => args,
            Err(error) => {
                let _ = error.print();
                return error.exit_code();
            }
        };
        match run(args) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("view_image: {error:#}");
                1
            }
        }
    }

    fn run(args: Args) -> anyhow::Result<()> {
        let attachment = load_attachment(&args.path)?;
        if !attachment.media_type.starts_with("image/") {
            anyhow::bail!("unsupported image type: {}", attachment.media_type);
        }
        write_image_artifact(&attachment, args.detail)?;
        println!(
            "Viewed image: {} ({}, {} bytes, detail={})",
            attachment.filename,
            attachment.media_type,
            attachment.data.len(),
            args.detail
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn detail_is_optional_and_defaults_to_auto() {
            let args = Args::try_parse_from(["view_image", "image.png"]).unwrap();
            assert_eq!(args.detail, ImageDetail::Auto);
            assert_eq!(args.path, PathBuf::from("image.png"));
        }

        #[test]
        fn accepts_every_detail_value() {
            for (value, expected) in [
                ("auto", ImageDetail::Auto),
                ("low", ImageDetail::Low),
                ("high", ImageDetail::High),
                ("original", ImageDetail::Original),
            ] {
                let args =
                    Args::try_parse_from(["view_image", "--detail", value, "image.png"]).unwrap();
                assert_eq!(args.detail, expected);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatches_only_known_argv0_basenames() {
        assert_eq!(
            from_argv0(OsStr::new("/x/apply_patch")),
            Some(Applet::ApplyPatch)
        );
        assert_eq!(from_argv0(OsStr::new("edit")), Some(Applet::Edit));
        assert_eq!(
            from_argv0(OsStr::new("view_image")),
            Some(Applet::ViewImage)
        );
        assert_eq!(from_argv0(OsStr::new("mu")), None);
        assert_eq!(from_argv0(OsStr::new("renamed-mu")), None);
    }
}
