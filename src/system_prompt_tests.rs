use std::path::{Path, PathBuf};

use super::{ROLE_PREAMBLE, assemble_prompt, cwd_changed_context, initial_environment_context};
use crate::paths::{Project, ProjectMarker};

#[test]
fn role_preamble_explicitly_limits_tools() {
    let prompt = assemble_prompt(&[], Path::new("/tmp/mu-test-global"), None);
    assert!(prompt.starts_with(ROLE_PREAMBLE));
    assert!(prompt.contains("Exactly one tool is available: `bash`."));
    assert!(prompt.contains(
        "Do not invent or call `read`, `write`, `edit`, `fetch`, `search`, `apply_patch`, `view_image`, or any other tool name."
    ));
    assert!(prompt.contains(
        "If a skill or `AGENTS.md` mentions another tool, treat it as historical shorthand"
    ));
}

#[test]
fn initial_environment_context_omits_project_lines_in_global_scope() {
    let context = initial_environment_context(Path::new("/tmp/work"), None, "session-1");
    assert!(context.contains("current working directory: /tmp/work"));
    assert!(context.contains("active session id: session-1"));
    assert!(!context.contains("project root:"));
}

#[test]
fn initial_environment_context_reports_project_and_worktree_metadata() {
    let context = initial_environment_context(
        Path::new("/tmp/work/subdir"),
        Some(&Project {
            root: PathBuf::from("/tmp/work"),
            marker: ProjectMarker::Git,
            worktree: Some(crate::paths::GitWorktreeInfo {
                git_dir: PathBuf::from("/tmp/repo/.git/worktrees/feature"),
                common_dir: Some(PathBuf::from("/tmp/repo/.git")),
            }),
        }),
        "session-1",
    );
    assert!(context.contains("project root: /tmp/work"));
    assert!(context.contains("git worktree: yes"));
}

#[test]
fn cwd_changed_context_is_wrapped_in_system_reminder() {
    let context = cwd_changed_context(Path::new("/tmp/next"));
    assert_eq!(
        context,
        "<system-reminder>\ncurrent working directory changed to: /tmp/next\n</system-reminder>"
    );
}
