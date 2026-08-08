//! User configuration persisted to `config.toml`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub shells: ShellConfig,
    pub backup: BackupConfig,
    pub capture: CaptureConfig,
}

/// Which shells receive generated definitions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    pub zsh: bool,
    pub bash: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        ShellConfig {
            zsh: true,
            bash: true,
        }
    }
}

/// The preferred backup method, chosen automatically when unset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackupMethod {
    /// Use the `gh` CLI.
    Gh,
    /// Use the built-in GitHub OAuth device flow.
    Oauth,
    /// Use a plain git remote with existing credentials.
    Git,
}

/// Backup-related settings and opt-outs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupConfig {
    /// Explicitly chosen method. When `None`, the best available tier is used.
    pub method: Option<BackupMethod>,
    /// Never use the `gh` CLI even when available.
    pub disable_gh: bool,
    /// Never use the OAuth device flow even when available.
    pub disable_oauth: bool,
    /// Remote URL used by the plain-git tier.
    pub remote_url: Option<String>,
    /// Name of the backup repository.
    pub repo_name: String,
}

impl Default for BackupConfig {
    fn default() -> Self {
        BackupConfig {
            method: None,
            disable_gh: false,
            disable_oauth: false,
            remote_url: None,
            repo_name: "cmd-man-backup".to_string(),
        }
    }
}

/// Output-capture settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    /// Hard timeout, in seconds, for any captured process.
    pub timeout_secs: u64,
    /// Whether to wrap capture in the filesystem/network sandbox.
    pub sandbox: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        CaptureConfig {
            timeout_secs: 10,
            sandbox: true,
        }
    }
}

impl Config {
    /// Load configuration, returning defaults when no file exists yet.
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        if !file.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let config: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        Ok(config)
    }

    /// Persist configuration to disk, creating directories as needed.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure_dirs()?;
        let text = toml::to_string_pretty(self).context("serializing config")?;
        let file = paths.config_file();
        std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_enable_both_shells_and_sandbox() {
        let c = Config::default();
        assert!(c.shells.zsh && c.shells.bash);
        assert!(c.capture.sandbox);
        assert_eq!(c.capture.timeout_secs, 10);
        assert_eq!(c.backup.repo_name, "cmd-man-backup");
        assert!(c.backup.method.is_none());
    }

    #[test]
    fn round_trips_through_toml() {
        let mut c = Config::default();
        c.backup.method = Some(BackupMethod::Gh);
        c.shells.bash = false;
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.backup.method, Some(BackupMethod::Gh));
        assert!(!back.shells.bash);
        assert!(back.shells.zsh);
    }

    #[test]
    fn partial_toml_fills_defaults() {
        let text = "[capture]\ntimeout_secs = 30\n";
        let c: Config = toml::from_str(text).unwrap();
        assert_eq!(c.capture.timeout_secs, 30);
        assert!(c.capture.sandbox);
        assert!(c.shells.zsh);
    }
}
