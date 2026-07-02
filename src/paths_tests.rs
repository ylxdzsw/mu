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

#[test]
fn init_project_layout_at_creates_minimal_scaffold() {
    let root = std::env::temp_dir().join(format!("mu-layout-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let result = init_project_layout_at(&root, true).unwrap();

    let state_dir = root.join(".mu");
    assert_eq!(result.root, root);
    assert_eq!(
        result.created_files,
        vec![".mu/", ".mu/config.jsonc", ".mu/.gitignore"]
    );
    assert!(!result.already_initialized);
    assert!(state_dir.is_dir());
    assert_eq!(
        std::fs::read_to_string(state_dir.join("config.jsonc")).unwrap(),
        PROJECT_CONFIG_TEMPLATE
    );
    assert_eq!(
        std::fs::read_to_string(state_dir.join(".gitignore")).unwrap(),
        STATE_GITIGNORE
    );
    assert!(!state_dir.join("skills").exists());

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn init_project_layout_at_rejects_nested_target_without_force() {
    let root = std::env::temp_dir().join(format!("mu-nested-{}", uuid::Uuid::new_v4()));
    let nested = root.join("subdir");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();

    let err = init_project_layout_at(&nested, false).unwrap_err();
    assert!(
        err.to_string()
            .contains("rerun with --force to create a nested mu project")
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn init_project_layout_at_allows_nested_target_with_force() {
    let root = std::env::temp_dir().join(format!("mu-force-{}", uuid::Uuid::new_v4()));
    let nested = root.join("subdir");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir(root.join(".git")).unwrap();

    let result = init_project_layout_at(&nested, true).unwrap();

    assert_eq!(result.root, nested);
    assert!(result.created_files.contains(&".mu/"));
    assert!(nested.join(".mu").is_dir());

    let _ = std::fs::remove_dir_all(root);
}
