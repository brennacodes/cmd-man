//! Resolution of on-disk locations for the cmd-man data store.

use std::path::PathBuf;

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

    pub fn shell_dir(&self) -> PathBuf {
        self.root.join("shell")
    }

    pub fn shell_file(&self, shell: Shell) -> PathBuf {
        self.shell_dir().join(shell.filename())
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
    }
}
