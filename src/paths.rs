use std::path::{Path, PathBuf};

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
    pub git_dir: PathBuf,
    pub common_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Project(Project),
    Global,
}

impl Scope {
    pub fn state_dir(&self) -> PathBuf {
        match self {
            Scope::Project(project) => project.root.join(".mu"),
            Scope::Global => global_dir(),
        }
    }

    pub fn session_db_path(&self) -> PathBuf {
        self.state_dir().join("sessions.db")
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
            return Some(Project {
                root: dir.to_path_buf(),
                marker: ProjectMarker::Git,
                worktree: git_worktree_info(dir),
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

pub fn ensure_project_layout(scope: &Scope) -> anyhow::Result<()> {
    ensure_state_layout(&scope.state_dir(), matches!(scope, Scope::Project(_)))
}

pub fn ensure_project_layout_at(root: &Path) -> anyhow::Result<()> {
    ensure_state_layout(&root.join(".mu"), true)
}

fn ensure_state_layout(dir: &Path, project: bool) -> anyhow::Result<()> {
    ensure_dir(&dir)?;
    ensure_dir(&dir.join("skills"))?;
    if project {
        let config = dir.join("config.jsonc");
        if !config.exists() {
            std::fs::write(&config, "{\n}\n")?;
        }
    }
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(
            &gitignore,
            ".env\nsessions.db\nsessions.db-*\n*.db\n*.db-*\n",
        )?;
    }
    Ok(())
}

fn is_home(path: &Path) -> bool {
    path == dirs_home()
}

fn git_worktree_info(root: &Path) -> Option<GitWorktreeInfo> {
    let dot_git = root.join(".git");
    if dot_git.is_dir() {
        return Some(GitWorktreeInfo {
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
        git_dir,
        common_dir,
    })
}

fn absolutize(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn stops_at_home_when_no_project_exists() {
        let home = dirs_home();
        assert!(discover_project(&home).is_none());
    }

    #[test]
    fn treats_git_worktree_root_as_the_project_root() {
        let root = std::env::temp_dir().join(format!("mu-worktree-{}", uuid::Uuid::new_v4()));
        let worktree = root.join("feature");
        let nested = worktree.join("src");
        let git_dir = worktree.join("../repo/.git/worktrees/feature");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../repo/.git/worktrees/feature\n",
        )
        .unwrap();
        std::fs::write(git_dir.join("commondir"), "../..\n").unwrap();

        let project = discover_project(&nested).unwrap();
        assert_eq!(project.root, worktree);
        assert_eq!(project.marker, ProjectMarker::Git);
        assert_eq!(
            project.worktree,
            Some(GitWorktreeInfo {
                git_dir: git_dir.clone(),
                common_dir: Some(git_dir.join("../..")),
            })
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_scope_uses_dot_mu_sessions_db() {
        let project = Project {
            root: PathBuf::from("/tmp/work"),
            marker: ProjectMarker::Git,
            worktree: None,
        };
        let scope = Scope::Project(project);
        assert_eq!(
            scope.session_db_path(),
            PathBuf::from("/tmp/work/.mu/sessions.db")
        );
    }
}
