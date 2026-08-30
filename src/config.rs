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
    pub scope: ScopeConfig,
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
    /// Automatically pull on invocation and commit+push on every change, in a
    /// detached background process, whenever a remote is resolvable.
    pub auto_sync: bool,
}

impl Default for BackupConfig {
    fn default() -> Self {
        BackupConfig {
            method: None,
            disable_gh: false,
            disable_oauth: false,
            remote_url: None,
            repo_name: "cmd-man-backup".to_string(),
            auto_sync: true,
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

/// Where a new entry is placed when the command line says nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeDefault {
    /// Place the entry globally.
    Global,
    /// Place the entry in the enclosing repository, when there is one.
    Repo,
}

/// Directory-scope settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScopeConfig {
    /// Default placement for a new entry, either `global` or `repo`. Kept as a
    /// string so an unrecognized value falls back instead of failing the load.
    pub default: String,
}

impl Default for ScopeConfig {
    fn default() -> Self {
        ScopeConfig {
            default: "global".to_string(),
        }
    }
}

impl ScopeConfig {
    /// Returns the effective mode and, when the configured value is not
    /// recognized, a notice describing the fallback.
    pub fn resolve(&self) -> (ScopeDefault, Option<String>) {
        match self.default.as_str() {
            "global" => (ScopeDefault::Global, None),
            "repo" => (ScopeDefault::Repo, None),
            other => (
                ScopeDefault::Global,
                Some(format!(
                    "config: unknown scope.default '{other}', using 'global'"
                )),
            ),
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
        assert!(c.backup.auto_sync);
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
        // A config that predates auto_sync still opts in by default.
        assert!(c.backup.auto_sync);
        assert_eq!(c.scope.default, "global");
    }

    #[test]
    fn scope_config_defaults_to_global() {
        let cfg = Config::default();
        assert_eq!(cfg.scope.default, "global");
        assert_eq!(cfg.scope.resolve(), (ScopeDefault::Global, None));
    }

    #[test]
    fn scope_config_accepts_repo() {
        let cfg: Config = toml::from_str("[scope]\ndefault = \"repo\"\n").unwrap();
        assert_eq!(cfg.scope.resolve(), (ScopeDefault::Repo, None));
    }

    #[test]
    fn an_invalid_scope_default_falls_back_with_a_notice() {
        let cfg: Config = toml::from_str("[scope]\ndefault = \"nonsense\"\n").unwrap();
        let (mode, notice) = cfg.scope.resolve();
        assert_eq!(mode, ScopeDefault::Global);
        assert!(notice.unwrap().contains("nonsense"));
        let text = toml::to_string(&cfg).unwrap();
        assert!(
            text.contains("nonsense"),
            "the raw value must survive a round trip: {text}"
        );
    }

    #[test]
    fn a_config_predating_scopes_loads_as_global() {
        let cfg: Config = toml::from_str("[shells]\nzsh = true\n").unwrap();
        assert_eq!(cfg.scope.resolve(), (ScopeDefault::Global, None));
    }

    #[test]
    fn auto_sync_round_trips_and_can_be_disabled() {
        let mut c = Config::default();
        c.backup.auto_sync = false;
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert!(!back.backup.auto_sync);
    }
}
