//! Resolution of on-disk locations for the cmd-man data store.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::shell::Shell;

/// Resolved filesystem locations for the store.
#[derive(Debug, Clone)]
pub struct Paths {
    root: PathBuf,
}

impl Paths {
    /// Resolve the data directory.
    ///
    /// Honors `CMD_MAN_HOME` (used by tests and power users), then
    /// `XDG_CONFIG_HOME`, then `~/.config/cmd-man` on every platform so the
    /// generated shell files live at a predictable, backup-friendly path.
    pub fn resolve() -> Result<Self> {
        if let Some(dir) = std::env::var_os("CMD_MAN_HOME") {
            return Ok(Self::with_root(PathBuf::from(dir)));
        }
        if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
            return Ok(Self::with_root(PathBuf::from(dir).join("cmd-man")));
        }
        let home = home_dir().context("could not determine home directory")?;
        Ok(Self::with_root(home.join(".config").join("cmd-man")))
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn categories_dir(&self) -> PathBuf {
        self.root.join("categories")
    }

    pub fn scopes_file(&self) -> PathBuf {
        self.root.join("scopes.toml")
    }

    pub fn shell_dir(&self) -> PathBuf {
        self.root.join("shell")
    }

    pub fn shell_file(&self, shell: Shell) -> PathBuf {
        self.shell_dir().join(shell.filename())
    }

    /// Lock file guarding a single background sync at a time.
    pub fn sync_lock_file(&self) -> PathBuf {
        self.root.join(".sync.lock")
    }

    /// State file recording the outcome of the last background sync.
    pub fn sync_state_file(&self) -> PathBuf {
        self.root.join(".sync-state.toml")
    }

    /// Marker recording which sync failure has already been surfaced.
    pub fn sync_notified_file(&self) -> PathBuf {
        self.root.join(".sync-notified")
    }

    /// Create the data, categories, and shell directories if missing.
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(self.categories_dir())
            .with_context(|| format!("creating {}", self.categories_dir().display()))?;
        std::fs::create_dir_all(self.shell_dir())
            .with_context(|| format!("creating {}", self.shell_dir().display()))?;
        Ok(())
    }
}

/// Resolve the user's home directory across platforms.
pub fn home_dir() -> Option<PathBuf> {
    directories::BaseDirs::new().map(|b| b.home_dir().to_path_buf())
}

/// Express a path using `$HOME` when it lives under the given home directory,
/// so stored and generated paths stay portable across machines.
pub fn portable_path_with(path: &Path, home: Option<&Path>) -> String {
    if let Some(home) = home {
        if path == home {
            return "$HOME".to_string();
        }
        if let Ok(rest) = path.strip_prefix(home) {
            return format!("$HOME/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// [`portable_path_with`] against the current user's home directory.
pub fn portable_path(path: &Path) -> String {
    portable_path_with(path, home_dir().as_deref())
}

/// Turn a `$HOME`- or `~`-prefixed path back into an absolute one. Text that
/// only starts with those characters, such as `~other` or `$HOMEBREW`, is left
/// alone.
pub fn expand_path_with(text: &str, home: Option<&Path>) -> PathBuf {
    let Some(home) = home else {
        return PathBuf::from(text);
    };
    for prefix in ["$HOME", "~"] {
        let Some(rest) = text.strip_prefix(prefix) else {
            continue;
        };
        if rest.is_empty() {
            return home.to_path_buf();
        }
        if let Some(rest) = rest.strip_prefix('/') {
            return home.join(rest);
        }
    }
    PathBuf::from(text)
}

/// [`expand_path_with`] against the current user's home directory.
pub fn expand_path(text: &str) -> PathBuf {
    expand_path_with(text, home_dir().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_rooted_under_data_dir() {
        let p = Paths::with_root(PathBuf::from("/tmp/cm"));
        assert_eq!(p.config_file(), PathBuf::from("/tmp/cm/config.toml"));
        assert_eq!(p.categories_dir(), PathBuf::from("/tmp/cm/categories"));
        assert_eq!(
            p.shell_file(Shell::Zsh),
            PathBuf::from("/tmp/cm/shell/cmd-man.zsh")
        );
        assert_eq!(
            p.shell_file(Shell::Bash),
            PathBuf::from("/tmp/cm/shell/cmd-man.bash")
        );
        assert_eq!(p.sync_lock_file(), PathBuf::from("/tmp/cm/.sync.lock"));
        assert_eq!(
            p.sync_state_file(),
            PathBuf::from("/tmp/cm/.sync-state.toml")
        );
        assert_eq!(
            p.sync_notified_file(),
            PathBuf::from("/tmp/cm/.sync-notified")
        );
    }

    #[test]
    fn portable_path_uses_home_placeholder() {
        let home = PathBuf::from("/Users/ada");
        assert_eq!(
            portable_path_with(Path::new("/Users/ada/work/api"), Some(&home)),
            "$HOME/work/api"
        );
        assert_eq!(
            portable_path_with(Path::new("/Users/ada"), Some(&home)),
            "$HOME"
        );
        assert_eq!(
            portable_path_with(Path::new("/opt/tools"), Some(&home)),
            "/opt/tools"
        );
        assert_eq!(
            portable_path_with(Path::new("/Users/adamant/x"), Some(&home)),
            "/Users/adamant/x"
        );
        assert_eq!(portable_path_with(Path::new("/tmp/x"), None), "/tmp/x");
    }

    #[test]
    fn expand_path_reverses_home_placeholder() {
        let home = PathBuf::from("/Users/ada");
        assert_eq!(
            expand_path_with("$HOME/work/api", Some(&home)),
            PathBuf::from("/Users/ada/work/api")
        );
        assert_eq!(
            expand_path_with("~/work/api", Some(&home)),
            PathBuf::from("/Users/ada/work/api")
        );
        assert_eq!(
            expand_path_with("~", Some(&home)),
            PathBuf::from("/Users/ada")
        );
        assert_eq!(
            expand_path_with("$HOME", Some(&home)),
            PathBuf::from("/Users/ada")
        );
        assert_eq!(
            expand_path_with("/opt/tools", Some(&home)),
            PathBuf::from("/opt/tools")
        );
        assert_eq!(
            expand_path_with("~adamant/x", Some(&home)),
            PathBuf::from("~adamant/x")
        );
        assert_eq!(
            expand_path_with("$HOMEBREW/x", Some(&home)),
            PathBuf::from("$HOMEBREW/x")
        );
        assert_eq!(expand_path_with("~/x", None), PathBuf::from("~/x"));
    }

    #[test]
    fn scopes_file_sits_beside_config() {
        let paths = Paths::with_root(PathBuf::from("/tmp/store"));
        assert_eq!(paths.scopes_file(), PathBuf::from("/tmp/store/scopes.toml"));
    }
}
