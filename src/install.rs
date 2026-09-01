use std::path::{Path, PathBuf};

#[cfg(feature = "portable")]
use std::ffi::OsStr;
#[cfg(feature = "portable")]
use std::os::unix::fs::PermissionsExt;

#[cfg(feature = "portable")]
use anyhow::bail;
use anyhow::{Context, Result};

#[cfg(feature = "portable")]
const BUILTINS: &[(&str, &str, bool)] = &[
    (
        "agent-browser",
        include_str!("../builtins/agent-browser"),
        false,
    ),
    (
        "background-task",
        include_str!("../builtins/background-task"),
        false,
    ),
    (
        "brave-search",
        include_str!("../builtins/brave-search"),
        false,
    ),
    ("cli", include_str!("../builtins/cli"), false),
    ("config", include_str!("../builtins/config"), false),
    ("exa-search", include_str!("../builtins/exa-search"), false),
    ("goal", include_str!("../builtins/goal"), true),
    ("grill", include_str!("../builtins/grill"), true),
    ("markitdown", include_str!("../builtins/markitdown"), false),
    ("mu-doc", include_str!("../builtins/mu-doc"), false),
    ("subagent", include_str!("../builtins/subagent"), false),
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
        let executable_mtime = if paths.builtins.cached || paths.applets.cached {
            Some(
                std::fs::metadata(&executable)
                    .with_context(|| format!("reading Mu executable {}", executable.display()))?
                    .modified()
                    .with_context(|| {
                        format!("reading Mu executable mtime {}", executable.display())
                    })?,
            )
        } else {
            None
        };

        if paths.builtins.cached {
            initialize_cache_root(&paths.cache_root)?;
            initialize_builtins(
                &paths.builtins.path,
                BUILTINS,
                executable_mtime.expect("cached resources have an executable mtime"),
            )?;
        }
        if paths.applets.cached {
            initialize_cache_root(&paths.cache_root)?;
            initialize_applets(
                &executable,
                &paths.applets.path,
                APPLET_NAMES,
                executable_mtime.expect("cached resources have an executable mtime"),
            )?;
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
fn initialize_builtins(
    directory: &Path,
    builtins: &[(&str, &str, bool)],
    executable_mtime: std::time::SystemTime,
) -> Result<()> {
    if !prepare_cache_directory(directory, "built-in", executable_mtime)? {
        return Ok(());
    }

    std::fs::create_dir(directory)
        .with_context(|| format!("creating portable built-ins {}", directory.display()))?;
    let result = (|| {
        for (name, contents, executable) in builtins {
            let path = directory.join(name);
            std::fs::write(&path, contents)
                .with_context(|| format!("writing portable built-in {}", path.display()))?;
            std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(if *executable { 0o755 } else { 0o644 }),
            )
            .with_context(|| format!("setting portable built-in mode {}", path.display()))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(directory);
    }
    result
}

#[cfg(feature = "portable")]
fn initialize_applets(
    executable: &Path,
    directory: &Path,
    names: &[&str],
    executable_mtime: std::time::SystemTime,
) -> Result<()> {
    if !prepare_cache_directory(directory, "applet", executable_mtime)? {
        return Ok(());
    }

    std::fs::create_dir(directory)
        .with_context(|| format!("creating portable applets {}", directory.display()))?;
    let result = (|| {
        for name in names {
            let path = directory.join(name);
            std::os::unix::fs::symlink(executable, &path)
                .with_context(|| format!("creating portable applet {}", path.display()))?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(directory);
    }
    result
}

#[cfg(feature = "portable")]
fn prepare_cache_directory(
    directory: &Path,
    kind: &str,
    executable_mtime: std::time::SystemTime,
) -> Result<bool> {
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() => {
            let directory_mtime = metadata.modified().with_context(|| {
                format!("reading portable {kind} mtime {}", directory.display())
            })?;
            if directory_mtime >= executable_mtime {
                return Ok(false);
            }
            std::fs::remove_dir_all(directory).with_context(|| {
                format!("removing stale portable {kind}s {}", directory.display())
            })?;
            Ok(true)
        }
        Ok(_) => bail!(
            "portable {kind} path is not a directory: {}",
            directory.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => {
            Err(error).with_context(|| format!("checking portable {kind}s {}", directory.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        crate::random::create_temp_dir(&std::env::temp_dir(), &format!("mu-install-{name}-"))
            .unwrap()
    }

    #[test]
    fn native_paths_are_derived_without_checking_installation_resources() {
        let tmp = temp_root("native-paths");
        let root = tmp.join("missing");
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
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[cfg(feature = "portable")]
    #[test]
    fn embedded_builtins_exactly_cover_the_shipped_files_and_modes() {
        let mut embedded = BUILTINS
            .iter()
            .map(|(name, _, executable)| ((*name).to_string(), *executable))
            .collect::<Vec<_>>();
        embedded.sort_unstable();
        let mut shipped = std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("builtins"))
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let executable = entry.metadata().unwrap().permissions().mode() & 0o111 != 0;
                let name = entry
                    .file_name()
                    .into_string()
                    .expect("built-in names are UTF-8");
                (name, executable)
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
        assert!(cache_root(None, None, false).is_err());
        assert!(cache_root(None, Some(OsStr::new("home")), false).is_err());
        assert!(
            cache_root(
                Some(OsStr::new("cache")),
                Some(OsStr::new("/home/me")),
                false
            )
            .is_err()
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

        initialize_builtins(&builtins, BUILTINS, std::time::UNIX_EPOCH).unwrap();
        initialize_applets(&executable, &applets, APPLET_NAMES, std::time::UNIX_EPOCH).unwrap();

        for (name, _, executable) in BUILTINS {
            let path = builtins.join(name);
            assert!(path.is_file());
            assert_eq!(
                path.metadata().unwrap().permissions().mode() & 0o111 != 0,
                *executable
            );
        }
        for name in APPLET_NAMES {
            assert_eq!(std::fs::read_link(applets.join(name)).unwrap(), executable);
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "portable")]
    #[test]
    fn fresh_existing_empty_and_partial_directories_are_trusted() {
        let root = temp_root("trust");
        let builtins = root.join("builtins");
        let applets = root.join("applets");
        std::fs::create_dir_all(&builtins).unwrap();
        std::fs::create_dir_all(&applets).unwrap();
        std::fs::write(applets.join("partial"), "keep").unwrap();

        initialize_builtins(&builtins, BUILTINS, std::time::UNIX_EPOCH).unwrap();
        initialize_applets(
            &root.join("mu"),
            &applets,
            APPLET_NAMES,
            std::time::UNIX_EPOCH,
        )
        .unwrap();

        assert!(std::fs::read_dir(&builtins).unwrap().next().is_none());
        assert_eq!(
            std::fs::read_to_string(applets.join("partial")).unwrap(),
            "keep"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "portable")]
    #[test]
    fn stale_directories_are_replaced() {
        use std::fs::FileTimes;
        use std::time::{Duration, UNIX_EPOCH};

        let root = temp_root("stale");
        let executable = root.join("mu");
        let builtins = root.join("builtins");
        let applets = root.join("applets");
        std::fs::create_dir_all(&builtins).unwrap();
        std::fs::create_dir_all(&applets).unwrap();
        std::fs::write(&executable, "binary").unwrap();
        std::fs::write(builtins.join("obsolete"), "remove").unwrap();
        std::os::unix::fs::symlink(root.join("old-mu"), applets.join("apply_patch")).unwrap();

        let old = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let executable_mtime = old + Duration::from_secs(1);
        for directory in [&builtins, &applets] {
            std::fs::File::open(directory)
                .unwrap()
                .set_times(FileTimes::new().set_modified(old))
                .unwrap();
        }

        initialize_builtins(&builtins, BUILTINS, executable_mtime).unwrap();
        initialize_applets(&executable, &applets, APPLET_NAMES, executable_mtime).unwrap();

        assert!(!builtins.join("obsolete").exists());
        for (name, contents, _) in BUILTINS {
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
    fn equal_directory_mtime_is_fresh() {
        use std::fs::FileTimes;
        use std::time::{Duration, UNIX_EPOCH};

        let root = temp_root("equal-mtime");
        let builtins = root.join("builtins");
        std::fs::create_dir_all(&builtins).unwrap();
        let mtime = UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        std::fs::File::open(&builtins)
            .unwrap()
            .set_times(FileTimes::new().set_modified(mtime))
            .unwrap();

        initialize_builtins(&builtins, BUILTINS, mtime).unwrap();

        assert!(std::fs::read_dir(&builtins).unwrap().next().is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "portable")]
    #[test]
    fn failed_population_removes_partial_directory() {
        let root = temp_root("failed-population");
        let builtins = root.join("builtins");

        assert!(
            initialize_builtins(
                &builtins,
                &[("missing/child", "contents", false)],
                std::time::UNIX_EPOCH,
            )
            .is_err()
        );
        assert!(!builtins.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(feature = "portable")]
    #[test]
    fn conflicting_files_are_rejected() {
        let root = temp_root("conflict");
        std::fs::write(root.join("builtins"), "occupied").unwrap();
        std::fs::write(root.join("applets"), "occupied").unwrap();

        assert!(
            initialize_builtins(&root.join("builtins"), BUILTINS, std::time::UNIX_EPOCH,).is_err()
        );
        assert!(
            initialize_applets(
                &root.join("mu"),
                &root.join("applets"),
                APPLET_NAMES,
                std::time::UNIX_EPOCH,
            )
            .is_err()
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
