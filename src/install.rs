use std::path::{Path, PathBuf};

#[cfg(feature = "portable")]
use std::ffi::OsStr;

#[cfg(feature = "portable")]
use anyhow::bail;
use anyhow::{Context, Result};

#[cfg(feature = "portable")]
const BUILTINS: &[(&str, &str)] = &[
    (
        "agent-browser.md",
        include_str!("../builtins/agent-browser.md"),
    ),
    (
        "background-task.md",
        include_str!("../builtins/background-task.md"),
    ),
    (
        "brave-search.md",
        include_str!("../builtins/brave-search.md"),
    ),
    (
        "customize-mu.md",
        include_str!("../builtins/customize-mu.md"),
    ),
    ("exa-search.md", include_str!("../builtins/exa-search.md")),
    ("markitdown.md", include_str!("../builtins/markitdown.md")),
    ("subagent.md", include_str!("../builtins/subagent.md")),
];

#[cfg(feature = "portable")]
const APPLET_NAMES: &[&str] = &["apply_patch", "edit", "view_image"];

pub fn prepare() -> Result<()> {
    #[cfg(feature = "portable")]
    {
        let executable = std::env::current_exe().context("locating the Mu executable")?;
        let paths = portable_paths(
            &executable,
            std::env::var_os("XDG_CACHE_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
            cfg!(target_os = "macos"),
        )?;

        if paths.builtins.cached {
            initialize_cache_root(&paths.cache_root)?;
            initialize_builtins(&paths.builtins.path, BUILTINS)?;
        }
        if paths.applets.cached {
            initialize_cache_root(&paths.cache_root)?;
            initialize_applets(&executable, &paths.applets.path, APPLET_NAMES)?;
        }
    }
    Ok(())
}

pub fn builtins_dir() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("locating the Mu executable")?;
    builtins_dir_from_executable(&executable)
}

pub fn applets_dir() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("locating the Mu executable")?;
    applets_dir_from_executable(&executable)
}

fn executable_dir(executable: &Path) -> Result<&Path> {
    executable
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .with_context(|| {
            format!(
                "Mu executable has no containing directory: {}",
                executable.display()
            )
        })
}

fn install_prefix(executable: &Path) -> Result<&Path> {
    executable_dir(executable)?
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .with_context(|| {
            format!(
                "Mu executable is not installed under <prefix>/bin: {}",
                executable.display()
            )
        })
}

fn native_builtins_dir(executable: &Path) -> Result<PathBuf> {
    Ok(install_prefix(executable)?.join("share/mu"))
}

fn native_applets_dir(executable: &Path) -> Result<PathBuf> {
    Ok(install_prefix(executable)?.join("libexec/mu"))
}

#[cfg(not(feature = "portable"))]
fn builtins_dir_from_executable(executable: &Path) -> Result<PathBuf> {
    native_builtins_dir(executable)
}

#[cfg(not(feature = "portable"))]
fn applets_dir_from_executable(executable: &Path) -> Result<PathBuf> {
    native_applets_dir(executable)
}

#[cfg(feature = "portable")]
fn builtins_dir_from_executable(executable: &Path) -> Result<PathBuf> {
    Ok(portable_paths(
        executable,
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        cfg!(target_os = "macos"),
    )?
    .builtins
    .path)
}

#[cfg(feature = "portable")]
fn applets_dir_from_executable(executable: &Path) -> Result<PathBuf> {
    Ok(portable_paths(
        executable,
        std::env::var_os("XDG_CACHE_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
        cfg!(target_os = "macos"),
    )?
    .applets
    .path)
}

#[cfg(feature = "portable")]
#[derive(Debug, PartialEq, Eq)]
struct ResourcePath {
    path: PathBuf,
    cached: bool,
}

#[cfg(feature = "portable")]
#[derive(Debug, PartialEq, Eq)]
struct PortablePaths {
    cache_root: PathBuf,
    builtins: ResourcePath,
    applets: ResourcePath,
}

#[cfg(feature = "portable")]
fn portable_paths(
    executable: &Path,
    xdg_cache_home: Option<&OsStr>,
    home: Option<&OsStr>,
    macos: bool,
) -> Result<PortablePaths> {
    let installed = executable_dir(executable)?
        .file_name()
        .is_some_and(|name| name == "bin");
    let native_builtins = native_builtins_dir(executable)?;
    let native_applets = native_applets_dir(executable)?;
    let use_native_builtins = installed && native_builtins.is_dir();
    let use_native_applets = installed && native_applets.is_dir();

    let cache_root = cache_root(xdg_cache_home, home, macos)?;
    Ok(PortablePaths {
        builtins: ResourcePath {
            path: if use_native_builtins {
                native_builtins
            } else {
                cache_root.join("builtins")
            },
            cached: !use_native_builtins,
        },
        applets: ResourcePath {
            path: if use_native_applets {
                native_applets
            } else {
                cache_root.join("applets")
            },
            cached: !use_native_applets,
        },
        cache_root,
    })
}

#[cfg(feature = "portable")]
fn cache_root(
    xdg_cache_home: Option<&OsStr>,
    home: Option<&OsStr>,
    macos: bool,
) -> Result<PathBuf> {
    if let Some(xdg) = xdg_cache_home {
        let xdg = PathBuf::from(xdg);
        if !xdg.is_absolute() {
            bail!("XDG_CACHE_HOME must be an absolute path: {}", xdg.display());
        }
        return Ok(xdg.join("mu"));
    }

    let home = home
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("cannot determine Mu cache directory: HOME is not set")?;
    if !home.is_absolute() {
        bail!(
            "cannot determine Mu cache directory: HOME must be an absolute path: {}",
            home.display()
        );
    }
    if macos {
        Ok(home.join("Library/Caches/mu"))
    } else {
        Ok(home.join(".cache/mu"))
    }
}

#[cfg(feature = "portable")]
fn initialize_cache_root(cache_root: &Path) -> Result<()> {
    std::fs::create_dir_all(cache_root)
        .with_context(|| format!("creating portable cache root {}", cache_root.display()))
}

#[cfg(feature = "portable")]
fn initialize_builtins(directory: &Path, builtins: &[(&str, &str)]) -> Result<()> {
    if trust_existing_directory(directory, "built-in")? {
        return Ok(());
    }

    std::fs::create_dir(directory)
        .with_context(|| format!("creating portable built-ins {}", directory.display()))?;
    for (name, contents) in builtins {
        let path = directory.join(name);
        std::fs::write(&path, contents)
            .with_context(|| format!("writing portable built-in {}", path.display()))?;
    }
    Ok(())
}

#[cfg(feature = "portable")]
fn initialize_applets(executable: &Path, directory: &Path, names: &[&str]) -> Result<()> {
    if trust_existing_directory(directory, "applet")? {
        return Ok(());
    }

    std::fs::create_dir(directory)
        .with_context(|| format!("creating portable applets {}", directory.display()))?;
    for name in names {
        let path = directory.join(name);
        std::os::unix::fs::symlink(executable, &path)
            .with_context(|| format!("creating portable applet {}", path.display()))?;
    }
    Ok(())
}

#[cfg(feature = "portable")]
fn trust_existing_directory(directory: &Path, kind: &str) -> Result<bool> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => bail!(
            "portable {kind} path is not a directory: {}",
            directory.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => {
            Err(error).with_context(|| format!("checking portable {kind}s {}", directory.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mu-install-{name}-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn native_paths_are_derived_without_checking_installation_resources() {
        let root = temp_root("native-paths");
        let executable = root.join("bin/mu");
        assert_eq!(
            native_builtins_dir(&executable).unwrap(),
            root.join("share/mu")
        );
        assert_eq!(
            native_applets_dir(&executable).unwrap(),
            root.join("libexec/mu")
        );
        assert!(!root.exists());
    }

    #[cfg(feature = "portable")]
    #[test]
    fn embedded_builtins_exactly_cover_the_shipped_files() {
        let mut embedded = BUILTINS.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        embedded.sort_unstable();
        let mut shipped = std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("builtins"))
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .into_string()
                    .expect("built-in names are UTF-8")
            })
            .collect::<Vec<_>>();
        shipped.sort_unstable();
        assert_eq!(embedded, shipped);
    }

    #[cfg(feature = "portable")]
    #[test]
    fn cache_root_uses_xdg_then_platform_home_conventions() {
        assert_eq!(
            cache_root(
                Some(OsStr::new("/cache")),
                Some(OsStr::new("/home/me")),
                false
            )
            .unwrap(),
            Path::new("/cache/mu")
        );
        assert_eq!(
            cache_root(None, Some(OsStr::new("/home/me")), true).unwrap(),
            Path::new("/home/me/Library/Caches/mu")
        );
        assert_eq!(
            cache_root(None, Some(OsStr::new("/home/me")), false).unwrap(),
            Path::new("/home/me/.cache/mu")
        );
    }

    #[cfg(feature = "portable")]
    #[test]
    fn cache_root_rejects_missing_or_relative_home_and_relative_xdg() {
        assert!(
            cache_root(None, None, false)
                .unwrap_err()
                .to_string()
                .contains("HOME")
        );
        assert!(
            cache_root(None, Some(OsStr::new("home")), false)
                .unwrap_err()
                .to_string()
                .contains("HOME must be an absolute path")
        );
        assert!(
            cache_root(
                Some(OsStr::new("cache")),
                Some(OsStr::new("/home/me")),
                false
            )
            .unwrap_err()
            .to_string()
            .contains("XDG_CACHE_HOME")
        );
    }

    #[cfg(feature = "portable")]
    #[test]
    fn installed_directories_take_precedence_independently() {
        let root = temp_root("installed");
        let executable = root.join("bin/mu");
        let cache = root.join("cache");
        std::fs::create_dir_all(root.join("share/mu")).unwrap();

        let paths = portable_paths(&executable, Some(cache.as_os_str()), None, false).unwrap();
        assert_eq!(paths.builtins.path, root.join("share/mu"));
        assert!(!paths.builtins.cached);
        assert_eq!(paths.applets.path, cache.join("mu/applets"));
        assert!(paths.applets.cached);

        std::fs::remove_dir_all(root.join("share")).unwrap();
        std::fs::create_dir_all(root.join("libexec/mu")).unwrap();
        let paths = portable_paths(&executable, Some(cache.as_os_str()), None, false).unwrap();
        assert_eq!(paths.builtins.path, cache.join("mu/builtins"));
        assert!(paths.builtins.cached);
        assert_eq!(paths.applets.path, root.join("libexec/mu"));
        assert!(!paths.applets.cached);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "portable")]
    #[test]
    fn non_bin_executables_ignore_nearby_installation_directories() {
        let root = temp_root("not-bin");
        let executable = root.join("elsewhere/mu");
        let cache = root.join("cache");
        std::fs::create_dir_all(root.join("share/mu")).unwrap();
        std::fs::create_dir_all(root.join("libexec/mu")).unwrap();

        let paths = portable_paths(&executable, Some(cache.as_os_str()), None, false).unwrap();
        assert_eq!(paths.builtins.path, cache.join("mu/builtins"));
        assert_eq!(paths.applets.path, cache.join("mu/applets"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "portable")]
    #[test]
    fn first_creation_populates_builtins_and_absolute_applet_symlinks() {
        let root = temp_root("create");
        let executable = root.join("mu");
        let builtins = root.join("cache/builtins");
        let applets = root.join("cache/applets");
        std::fs::create_dir_all(root.join("cache")).unwrap();
        std::fs::write(&executable, "binary").unwrap();

        initialize_builtins(&builtins, BUILTINS).unwrap();
        initialize_applets(&executable, &applets, APPLET_NAMES).unwrap();

        for (name, contents) in BUILTINS {
            assert_eq!(
                std::fs::read_to_string(builtins.join(name)).unwrap(),
                *contents
            );
        }
        for name in APPLET_NAMES {
            assert_eq!(std::fs::read_link(applets.join(name)).unwrap(), executable);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "portable")]
    #[test]
    fn existing_empty_and_partial_directories_are_trusted() {
        let root = temp_root("trust");
        let builtins = root.join("builtins");
        let applets = root.join("applets");
        std::fs::create_dir_all(&builtins).unwrap();
        std::fs::create_dir_all(&applets).unwrap();
        std::fs::write(builtins.join("partial"), "keep").unwrap();
        std::fs::write(applets.join("partial"), "keep").unwrap();

        initialize_builtins(&builtins, BUILTINS).unwrap();
        initialize_applets(&root.join("mu"), &applets, APPLET_NAMES).unwrap();

        assert_eq!(std::fs::read_dir(&builtins).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(&applets).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "portable")]
    #[test]
    fn conflicting_files_are_rejected() {
        let root = temp_root("conflict");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("builtins"), "occupied").unwrap();
        std::fs::write(root.join("applets"), "occupied").unwrap();

        assert!(
            initialize_builtins(&root.join("builtins"), BUILTINS)
                .unwrap_err()
                .to_string()
                .contains("not a directory")
        );
        assert!(
            initialize_applets(&root.join("mu"), &root.join("applets"), APPLET_NAMES)
                .unwrap_err()
                .to_string()
                .contains("not a directory")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "portable")]
    #[test]
    fn creation_write_and_link_failures_leave_trusted_partial_directories() {
        let root = temp_root("failures");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("cache"), "occupied").unwrap();
        assert!(
            initialize_cache_root(&root.join("cache/mu"))
                .unwrap_err()
                .to_string()
                .contains("creating portable cache root")
        );

        let missing_parent = root.join("missing/builtins");
        assert!(
            initialize_builtins(&missing_parent, BUILTINS)
                .unwrap_err()
                .to_string()
                .contains("creating portable built-ins")
        );
        assert!(
            initialize_applets(
                &root.join("mu"),
                &root.join("missing-applets/applets"),
                APPLET_NAMES
            )
            .unwrap_err()
            .to_string()
            .contains("creating portable applets")
        );

        let builtins = root.join("builtins");
        assert!(
            initialize_builtins(
                &builtins,
                &[("partial.md", "keep"), ("missing/file", "contents")]
            )
            .unwrap_err()
            .to_string()
            .contains("writing portable built-in")
        );
        assert!(builtins.is_dir());
        initialize_builtins(&builtins, BUILTINS).unwrap();
        assert_eq!(std::fs::read_dir(&builtins).unwrap().count(), 1);

        let applets = root.join("applets");
        assert!(
            initialize_applets(&root.join("mu"), &applets, &["partial", "missing/applet"])
                .unwrap_err()
                .to_string()
                .contains("creating portable applet")
        );
        assert!(applets.is_dir());
        initialize_applets(&root.join("mu"), &applets, APPLET_NAMES).unwrap();
        assert_eq!(std::fs::read_dir(&applets).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
