use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub root: PathBuf,
    pub marker: ProjectMarker,
    pub worktree: Option<GitWorktreeInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectMarker {
    Mu,
    Git,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitWorktreeInfo {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    pub common_dir: Option<PathBuf>,
}

impl GitWorktreeInfo {
    pub fn main_worktree_root(&self) -> Option<&Path> {
        let common_dir = self.common_dir.as_ref()?;
        if common_dir.file_name()? != ".git" {
            return None;
        }

        let admin_name = self
            .git_dir
            .strip_prefix(common_dir.join("worktrees"))
            .ok()?;
        if admin_name.components().count() != 1 {
            return None;
        }

        common_dir.parent()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Project(Project),
    Global,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInitResult {
    pub root: PathBuf,
    pub created_files: Vec<&'static str>,
    pub already_initialized: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateLayout {
    Global,
    AutomaticProject,
    ExplicitProject,
}

impl Scope {
    pub fn state_dir(&self) -> PathBuf {
        match self {
            Scope::Project(project) => project.root.join(".mu"),
            Scope::Global => global_dir(),
        }
    }

    pub fn session_store_path(&self) -> PathBuf {
        self.state_dir()
    }

    pub fn project(&self) -> Option<&Project> {
        match self {
            Scope::Project(project) => Some(project),
            Scope::Global => None,
        }
    }
}

pub fn discover_scope(cwd: &Path) -> Scope {
    discover_project(cwd).map_or(Scope::Global, Scope::Project)
}

pub fn discover_project(cwd: &Path) -> Option<Project> {
    for dir in cwd.ancestors() {
        if is_home(dir) || dir.parent().is_none() {
            break;
        }
        if dir.join(".mu").is_dir() {
            return Some(Project {
                root: dir.to_path_buf(),
                marker: ProjectMarker::Mu,
                worktree: git_worktree_info(dir),
            });
        }
        if dir.join(".git").exists() {
            let worktree = git_worktree_info(dir);
            let root = worktree
                .as_ref()
                .and_then(GitWorktreeInfo::main_worktree_root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| dir.to_path_buf());
            return Some(Project {
                root,
                marker: ProjectMarker::Git,
                worktree,
            });
        }
    }
    None
}

pub fn global_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MU_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    dirs_home().join(".mu")
}

pub fn builtins_dir() -> Result<PathBuf> {
    crate::install::builtins_dir()
}

pub fn applets_dir() -> Result<PathBuf> {
    crate::install::applets_dir()
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"))
}

pub fn ensure_dir(path: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    Ok(())
}

pub(crate) fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(_) | Component::RootDir | Component::Prefix(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

/// Return Mu's private, flat runtime directory.
///
/// The directory is deliberately outside project state. It is shared by the
/// current user's Mu processes, but never treated as a world-shared `/tmp`
/// namespace. A pre-existing unsafe directory is rejected instead of being
/// relaxed or followed.
pub fn runtime_dir() -> Result<PathBuf> {
    let directory = std::env::temp_dir().join("mu");
    let metadata = match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            use std::os::unix::fs::DirBuilderExt;

            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(&directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "creating private Mu temporary directory {}",
                            directory.display()
                        )
                    });
                }
            }
            std::fs::symlink_metadata(&directory).with_context(|| {
                format!("checking Mu temporary directory {}", directory.display())
            })?
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("checking Mu temporary directory {}", directory.display())
            });
        }
    };
    if !metadata.is_dir() {
        bail!(
            "Mu temporary path is not a directory: {}",
            directory.display()
        );
    }
    use std::os::unix::fs::MetadataExt;
    let mode = metadata.mode() & 0o777;
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid || mode & 0o077 != 0 {
        bail!(
            "Mu temporary directory is not private to this user: {}",
            directory.display()
        );
    }
    Ok(directory)
}

pub fn ensure_project_layout(scope: &Scope) -> Result<()> {
    let layout = match scope {
        Scope::Global => StateLayout::Global,
        Scope::Project(_) => StateLayout::AutomaticProject,
    };
    ensure_state_layout(&scope.state_dir(), layout)?;
    Ok(())
}

pub fn init_project_layout_at(root: &Path, force: bool) -> Result<ProjectInitResult> {
    validate_project_init_root(root, force)?;
    let created_files = ensure_state_layout(&root.join(".mu"), StateLayout::ExplicitProject)?;
    Ok(ProjectInitResult {
        root: root.to_path_buf(),
        already_initialized: created_files.is_empty(),
        created_files,
    })
}

fn ensure_state_layout(dir: &Path, layout: StateLayout) -> Result<Vec<&'static str>> {
    let mut created_files = Vec::new();
    if !dir.exists() {
        ensure_dir(dir)?;
        created_files.push(".mu/");
    } else {
        ensure_dir(dir)?;
    }
    if layout == StateLayout::ExplicitProject {
        let config = dir.join("config.jsonc");
        if !config.exists() {
            std::fs::write(&config, PROJECT_CONFIG_TEMPLATE)?;
            created_files.push(".mu/config.jsonc");
        }
    }
    if layout != StateLayout::Global {
        let gitignore = dir.join(".gitignore");
        if !gitignore.exists() {
            std::fs::write(&gitignore, STATE_GITIGNORE)?;
            created_files.push(".mu/.gitignore");
        } else {
            reconcile_state_gitignore(&gitignore)?;
        }
    }
    Ok(created_files)
}

fn reconcile_state_gitignore(path: &Path) -> Result<()> {
    let existing = std::fs::read_to_string(path)?;
    let present = existing.lines().collect::<std::collections::HashSet<_>>();
    let missing = STATE_GITIGNORE_LINES
        .iter()
        .copied()
        .filter(|line| !present.contains(line))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    let mut updated = existing.clone();
    if !existing.is_empty() && !existing.ends_with('\n') {
        updated.push('\n');
    }
    for line in missing {
        updated.push_str(line);
        updated.push('\n');
    }
    let suffix = crate::random::random_bytes::<8>()?
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary = path.with_file_name(format!(".gitignore.{suffix}"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    use std::io::Write;
    file.write_all(updated.as_bytes())?;
    file.sync_all()?;
    std::fs::rename(&temporary, path)?;
    std::fs::File::open(path.parent().context(".gitignore has no parent")?)?.sync_all()?;
    Ok(())
}

pub fn validate_project_init_root(root: &Path, force: bool) -> Result<()> {
    if is_home(root) {
        bail!(
            "cannot initialize a mu project at {}; home is reserved for global scope",
            root.display()
        );
    }
    if root.parent().is_none() {
        bail!(
            "cannot initialize a mu project at {}; filesystem root is not a project scope",
            root.display()
        );
    }
    if root.join(".mu").is_dir() {
        return Ok(());
    }
    if let Some(project) = discover_project(root)
        && project.root != root
        && !force
    {
        bail!(
            "target {} is inside existing {} project {}; rerun with --force to create a nested mu project",
            root.display(),
            project_marker_name(project.marker),
            project.root.display()
        );
    }
    Ok(())
}

fn is_home(path: &Path) -> bool {
    path == dirs_home()
}

fn project_marker_name(marker: ProjectMarker) -> &'static str {
    match marker {
        ProjectMarker::Mu => "mu",
        ProjectMarker::Git => "git",
    }
}

const PROJECT_CONFIG_TEMPLATE: &str =
    "{\n  // Optional project-local overrides merged over ~/.mu/config.jsonc.\n}\n";

const STATE_GITIGNORE_LINES: &[&str] = &[
    ".gitignore",
    ".env",
    "sessions/",
    "objects/",
    "current-session",
    ".current-session.*",
];
const STATE_GITIGNORE: &str =
    ".gitignore\n.env\nsessions/\nobjects/\ncurrent-session\n.current-session.*\n";

fn git_worktree_info(root: &Path) -> Option<GitWorktreeInfo> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(GitWorktreeInfo {
            root: root.to_path_buf(),
            git_dir: dot_git,
            common_dir: None,
        });
    }

    let text = std::fs::read_to_string(&dot_git).ok()?;
    let git_dir = text.strip_prefix("gitdir:")?.trim();
    let git_dir = absolutize(root, Path::new(git_dir));
    let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
        .ok()
        .map(|text| absolutize(&git_dir, Path::new(text.trim())));
    Some(GitWorktreeInfo {
        root: root.to_path_buf(),
        git_dir,
        common_dir,
    })
}

fn absolutize(base: &Path, path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    absolute.canonicalize().unwrap_or(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_required_gitignore(path: &Path) {
        let contents = std::fs::read_to_string(path).unwrap();
        let lines = contents.lines().collect::<Vec<_>>();
        for required in STATE_GITIGNORE_LINES {
            assert!(lines.contains(required), "missing {required}");
        }
    }

    #[test]
    fn runtime_directory_is_private_and_owned_by_the_current_user() {
        use std::os::unix::fs::MetadataExt;

        let directory = runtime_dir().unwrap();
        let metadata = std::fs::symlink_metadata(directory).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o077, 0);
    }

    #[test]
    fn discovers_nearest_mu_project_without_creating_files() {
        let root = std::env::temp_dir().join(format!("mu-paths-{}", uuid::Uuid::new_v4()));
        let nested = root.join("a/b");
        std::fs::create_dir_all(root.join(".mu")).unwrap();
        std::fs::create_dir_all(&nested).unwrap();

        let project = discover_project(&nested).unwrap();
        assert_eq!(project.root, root);
        assert_eq!(project.marker, ProjectMarker::Mu);
    }

    #[test]
    fn linked_worktree_uses_primary_project_unless_it_has_local_mu_state() {
        let repository = std::env::temp_dir().join(format!("mu-worktree-{}", uuid::Uuid::new_v4()));
        let worktree = repository.join("worktrees/feature");
        let nested = worktree.join("src/nested");
        let git_dir = repository.join(".git/worktrees/feature");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();

        let project = discover_project(&nested).unwrap();

        assert_eq!(project.root, repository);
        assert_eq!(project.marker, ProjectMarker::Git);
        assert_eq!(
            project.worktree,
            Some(GitWorktreeInfo {
                root: worktree.clone(),
                git_dir,
                common_dir: Some(repository.join(".git")),
            })
        );
        assert_eq!(
            Scope::Project(project).session_store_path(),
            repository.join(".mu")
        );
        assert!(validate_project_init_root(&worktree, false).is_err());
        assert!(validate_project_init_root(&worktree, true).is_ok());

        std::fs::create_dir_all(worktree.join(".mu")).unwrap();
        let project = discover_project(&nested).unwrap();
        assert_eq!(project.root, worktree);
        assert_eq!(project.marker, ProjectMarker::Mu);

        let _ = std::fs::remove_dir_all(repository);
    }

    #[test]
    fn nonstandard_common_dir_keeps_worktree_local_scope() {
        let repository =
            std::env::temp_dir().join(format!("mu-bare-worktree-{}", uuid::Uuid::new_v4()));
        let common_dir = repository.join("repo.git");
        let git_dir = common_dir.join("worktrees/feature");
        let worktree = repository.join("feature");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", git_dir.display()),
        )
        .unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();

        let project = discover_project(&worktree).unwrap();

        assert_eq!(project.root, worktree);
        assert_eq!(project.marker, ProjectMarker::Git);

        let _ = std::fs::remove_dir_all(repository);
    }

    #[test]
    fn init_project_layout_at_creates_minimal_scaffold() {
        let root = std::env::temp_dir().join(format!("mu-layout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        let result = init_project_layout_at(&root, true).unwrap();

        let state_dir = root.join(".mu");
        assert_eq!(result.root, root);
        for expected in [".mu/", ".mu/config.jsonc", ".mu/.gitignore"] {
            assert!(result.created_files.contains(&expected));
        }
        assert!(!result.already_initialized);
        assert!(state_dir.is_dir());
        assert!(state_dir.join("config.jsonc").is_file());
        assert_required_gitignore(&state_dir.join(".gitignore"));
        assert!(!state_dir.join("skills").exists());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn automatic_project_layout_omits_project_config() {
        let root = std::env::temp_dir().join(format!("mu-layout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let scope = Scope::Project(Project {
            root: root.clone(),
            marker: ProjectMarker::Git,
            worktree: None,
        });

        ensure_project_layout(&scope).unwrap();

        let state_dir = root.join(".mu");
        assert!(state_dir.is_dir());
        assert!(!state_dir.join("config.jsonc").exists());
        assert_required_gitignore(&state_dir.join(".gitignore"));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn existing_state_gitignore_keeps_old_entries_and_adds_current_paths() {
        let root = std::env::temp_dir().join(format!("mu-layout-{}", uuid::Uuid::new_v4()));
        let state = root.join(".mu");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join(".gitignore"),
            ".gitignore\nsessions.db\ncustom\n",
        )
        .unwrap();
        let scope = Scope::Project(Project {
            root: root.clone(),
            marker: ProjectMarker::Git,
            worktree: None,
        });

        ensure_project_layout(&scope).unwrap();

        let contents = std::fs::read_to_string(state.join(".gitignore")).unwrap();
        assert!(contents.lines().any(|line| line == "custom"));
        assert_required_gitignore(&state.join(".gitignore"));
        let _ = std::fs::remove_dir_all(root);
    }
}
