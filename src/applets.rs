use std::ffi::OsStr;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applet {
    ApplyPatch,
    ViewImage,
}

pub fn from_argv0(argv0: &OsStr) -> Option<Applet> {
    match Path::new(argv0).file_name().and_then(OsStr::to_str) {
        Some("apply_patch") => Some(Applet::ApplyPatch),
        Some("view_image") => Some(Applet::ViewImage),
        _ => None,
    }
}

pub fn dispatch(applet: Applet) -> i32 {
    match applet {
        Applet::ApplyPatch => apply_patch::main(),
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
            content: String,
            permissions: fs::Permissions,
        },
        Move {
            from: PathBuf,
            to: PathBuf,
            content: String,
            permissions: fs::Permissions,
        },
    }

    pub fn main() -> i32 {
        if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--help"))
            && std::env::args_os().nth(2).is_none()
        {
            println!(
                "Usage: apply_patch 'PATCH'\n       apply_patch < patch.txt\n\nApply a Mu/Codex-style *** Begin Patch envelope relative to the current directory."
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
        if path.is_absolute() {
            bail!("patch paths must be relative: {}", path.display());
        }
        Ok(path)
    }

    fn preflight(cwd: &Path, operations: Vec<Operation>) -> Result<Vec<PlannedChange>> {
        let mut touched = HashSet::new();
        let mut changes = Vec::new();
        for operation in operations {
            match operation {
                Operation::Add { path, content } => {
                    claim_path(&mut touched, &path)?;
                    let full = cwd.join(&path);
                    if fs::symlink_metadata(&full).is_ok() {
                        bail!("add destination already exists: {}", path.display());
                    }
                    changes.push(PlannedChange::Add {
                        path: full,
                        content,
                    });
                }
                Operation::Delete { path } => {
                    claim_path(&mut touched, &path)?;
                    let full = cwd.join(&path);
                    regular_file_metadata(&full, "delete")?;
                    changes.push(PlannedChange::Delete { path: full });
                }
                Operation::Update {
                    path,
                    move_to,
                    chunks,
                } => {
                    claim_path(&mut touched, &path)?;
                    if let Some(destination) = &move_to {
                        claim_path(&mut touched, destination)?;
                    }
                    let full = cwd.join(&path);
                    let metadata = regular_file_metadata(&full, "update")?;
                    let original = fs::read_to_string(&full)
                        .with_context(|| format!("reading file to update {}", path.display()))?;
                    let content = if chunks.is_empty() {
                        original
                    } else {
                        apply_chunks(&original, &path, &chunks)?
                    };
                    if let Some(destination) = move_to {
                        let destination_full = cwd.join(&destination);
                        if fs::symlink_metadata(&destination_full).is_ok() {
                            bail!("move destination already exists: {}", destination.display());
                        }
                        changes.push(PlannedChange::Move {
                            from: full,
                            to: destination_full,
                            content,
                            permissions: metadata.permissions(),
                        });
                    } else {
                        changes.push(PlannedChange::Update {
                            path: full,
                            content,
                            permissions: metadata.permissions(),
                        });
                    }
                }
            }
        }
        Ok(changes)
    }

    fn claim_path(touched: &mut HashSet<PathBuf>, path: &Path) -> Result<()> {
        if !touched.insert(normalize_relative_path(path)) {
            bail!(
                "patch contains conflicting operations for {}",
                path.display()
            );
        }
        Ok(())
    }

    fn normalize_relative_path(path: &Path) -> PathBuf {
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => normalized.push(part),
                Component::ParentDir => {
                    if normalized.file_name() == Some(std::ffi::OsStr::new(".."))
                        || !normalized.pop()
                    {
                        normalized.push("..");
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    unreachable!("relative path validated")
                }
            }
        }
        normalized
    }

    fn regular_file_metadata(path: &Path, action: &str) -> Result<fs::Metadata> {
        let metadata = fs::symlink_metadata(path)
            .with_context(|| format!("cannot {action} missing file {}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("cannot {action} non-regular file {}", path.display());
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

    fn atomic_write(
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
            PlannedChange::Update { path, .. } => format!("M {}", relative(path)),
            PlannedChange::Move { from, to, .. } => {
                format!("R {} -> {}", relative(from), relative(to))
            }
        }
    }

    fn change_label(change: &PlannedChange) -> String {
        match change {
            PlannedChange::Add { path, .. } => format!("A {}", path.display()),
            PlannedChange::Delete { path } => format!("D {}", path.display()),
            PlannedChange::Update { path, .. } => format!("M {}", path.display()),
            PlannedChange::Move { from, to, .. } => {
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
            assert!(preflight(&dir, parse_patch(patch).unwrap()).is_err());
            assert_eq!(fs::read_to_string(dir.join("one.txt")).unwrap(), "old\n");
            assert_eq!(
                fs::read_to_string(dir.join("exists.txt")).unwrap(),
                "keep\n"
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
        fn rejects_absolute_paths() {
            let error =
                parse_patch("*** Begin Patch\n*** Add File: /tmp/nope\n+x\n*** End Patch\n")
                    .unwrap_err();
            assert!(error.to_string().contains("must be relative"));
        }

        #[test]
        fn rejects_lexically_aliased_operations() {
            let dir = temp_dir();
            let patch = "*** Begin Patch\n*** Add File: sub/../same.txt\n+one\n*** Add File: same.txt\n+two\n*** End Patch\n";
            let error = preflight(&dir, parse_patch(patch).unwrap()).unwrap_err();
            assert!(error.to_string().contains("conflicting operations"));
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
        assert_eq!(
            from_argv0(OsStr::new("view_image")),
            Some(Applet::ViewImage)
        );
        assert_eq!(from_argv0(OsStr::new("mu")), None);
        assert_eq!(from_argv0(OsStr::new("renamed-mu")), None);
    }
}
