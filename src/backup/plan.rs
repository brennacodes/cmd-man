//! Backup method selection: pick the best available tier without ever forcing
//! a method the user opted out of.

use crate::config::{BackupConfig, BackupMethod};

/// Which backup capabilities are available on this machine right now.
#[derive(Debug, Clone, Default)]
pub struct Availability {
    /// `gh` is installed and authenticated.
    pub gh: bool,
    /// `git` is installed.
    pub git: bool,
    /// A GitHub OAuth client id is configured, so the device flow can run.
    pub oauth: bool,
    /// The resolved GitHub login, when known (from `gh`).
    pub gh_login: Option<String>,
}

/// The chosen backup method and why it was chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupPlan {
    pub method: BackupMethod,
    pub reason: String,
}

/// Decide which backup method to use.
///
/// An explicit `config.method` is honored (and errors if that method is not
/// available). Otherwise the best available tier is chosen in order gh, oauth,
/// git, skipping any the user disabled.
pub fn plan_backup(config: &BackupConfig, avail: &Availability) -> Result<BackupPlan, String> {
    if let Some(method) = config.method {
        return match method {
            BackupMethod::Gh if avail.gh => Ok(plan(method, "chosen in config")),
            BackupMethod::Gh => {
                Err("config selects gh, but gh is not available or not authenticated".into())
            }
            BackupMethod::Oauth if avail.oauth => Ok(plan(method, "chosen in config")),
            BackupMethod::Oauth => {
                Err("config selects oauth, but no GitHub client id is configured".into())
            }
            BackupMethod::Git if avail.git => Ok(plan(method, "chosen in config")),
            BackupMethod::Git => Err("config selects git, but git is not installed".into()),
        };
    }

    if avail.gh && !config.disable_gh {
        return Ok(plan(
            BackupMethod::Gh,
            "gh CLI is available and authenticated",
        ));
    }
    if avail.oauth && !config.disable_oauth {
        return Ok(plan(
            BackupMethod::Oauth,
            "GitHub OAuth device flow is available",
        ));
    }
    if avail.git {
        let reason = if config.disable_gh || config.disable_oauth {
            "falling back to a plain git remote (higher tiers disabled or unavailable)"
        } else {
            "falling back to a plain git remote"
        };
        return Ok(plan(BackupMethod::Git, reason));
    }

    Err("no backup method available: install git, or gh, or configure a GitHub client id".into())
}

fn plan(method: BackupMethod, reason: &str) -> BackupPlan {
    BackupPlan {
        method,
        reason: reason.to_string(),
    }
}

/// Pre-filled GitHub "new repository" URL for the fallback tier.
pub fn new_repo_link(repo: &str) -> String {
    format!("https://github.com/new?name={repo}&visibility=private")
}

/// HTTPS remote URL for a user's backup repo.
pub fn remote_https_url(user: &str, repo: &str) -> String {
    format!("https://github.com/{user}/{repo}.git")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avail(gh: bool, oauth: bool, git: bool) -> Availability {
        Availability {
            gh,
            git,
            oauth,
            gh_login: None,
        }
    }

    #[test]
    fn auto_prefers_gh_then_oauth_then_git() {
        let cfg = BackupConfig::default();
        assert_eq!(
            plan_backup(&cfg, &avail(true, true, true)).unwrap().method,
            BackupMethod::Gh
        );
        assert_eq!(
            plan_backup(&cfg, &avail(false, true, true)).unwrap().method,
            BackupMethod::Oauth
        );
        assert_eq!(
            plan_backup(&cfg, &avail(false, false, true))
                .unwrap()
                .method,
            BackupMethod::Git
        );
    }

    #[test]
    fn disabled_tiers_are_skipped_even_if_available() {
        let mut cfg = BackupConfig {
            disable_gh: true,
            ..Default::default()
        };
        assert_eq!(
            plan_backup(&cfg, &avail(true, true, true)).unwrap().method,
            BackupMethod::Oauth
        );
        cfg.disable_oauth = true;
        assert_eq!(
            plan_backup(&cfg, &avail(true, true, true)).unwrap().method,
            BackupMethod::Git
        );
    }

    #[test]
    fn explicit_choice_is_honored_or_errors() {
        let mut cfg = BackupConfig {
            method: Some(BackupMethod::Git),
            ..Default::default()
        };
        // Git chosen even though gh is available.
        assert_eq!(
            plan_backup(&cfg, &avail(true, true, true)).unwrap().method,
            BackupMethod::Git
        );
        cfg.method = Some(BackupMethod::Gh);
        assert!(plan_backup(&cfg, &avail(false, true, true)).is_err());
    }

    #[test]
    fn no_method_available_is_an_error() {
        let cfg = BackupConfig::default();
        assert!(plan_backup(&cfg, &avail(false, false, false)).is_err());
    }

    #[test]
    fn link_and_remote_builders() {
        assert_eq!(
            new_repo_link("cmd-man-backup"),
            "https://github.com/new?name=cmd-man-backup&visibility=private"
        );
        assert_eq!(
            remote_https_url("brennacodes", "cmd-man-backup"),
            "https://github.com/brennacodes/cmd-man-backup.git"
        );
    }
}
