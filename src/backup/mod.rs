//! GitHub backup and restore across three tiers: gh CLI, OAuth device flow,
//! and a plain git remote.

pub mod git;
pub mod oauth;
pub mod plan;
mod sync;

use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::config::{BackupConfig, BackupMethod, Config};
use crate::paths::Paths;

use git::GitRepo;
pub use plan::{Availability, BackupPlan, new_repo_link, plan_backup, remote_https_url};
pub use sync::{
    SYNC_EXEC_ARG, SyncStatus, read_status, run_sync, spawn_sync, sync_exec_main,
    take_failure_notice,
};

/// Human-facing outcome of a backup run.
#[derive(Debug)]
pub struct BackupReport {
    pub method: BackupMethod,
    pub committed: bool,
    pub pushed: bool,
    pub messages: Vec<String>,
}

/// Detect which backup tiers are usable right now.
pub fn detect_availability() -> Availability {
    let git = which::which("git").is_ok();
    let gh_installed = which::which("gh").is_ok();
    let gh = gh_installed && gh_auth_ok();
    let gh_login = if gh { gh_login() } else { None };
    let oauth = oauth::client_id().is_some();
    Availability {
        gh,
        git,
        oauth,
        gh_login,
    }
}

/// Prepare the store directory as a git repo that ignores generated files.
pub fn prepare_repo(paths: &Paths) -> Result<GitRepo> {
    paths.ensure_dirs()?;
    let repo = GitRepo::new(paths.root().clone());
    repo.init()?;
    repo.ensure_gitignore(&[
        "shell/",
        ".DS_Store",
        ".sync.lock",
        ".sync-state.toml",
        ".sync-notified",
        ".sync-clone/",
    ])?;
    Ok(repo)
}

/// Run a backup, choosing the tier from config + availability.
pub fn run_backup(paths: &Paths, config: &Config, timestamp: &str) -> Result<BackupReport> {
    let avail = detect_availability();
    let selected = plan_backup(&config.backup, &avail).map_err(|e| anyhow::anyhow!(e))?;
    let mut messages = vec![selected.reason.clone()];

    let repo = prepare_repo(paths)?;
    let committed = repo.commit_all(&format!("cmd-man backup {timestamp}"))?;

    let pushed = match selected.method {
        BackupMethod::Gh => push_via_gh(&repo, &config.backup, &avail, &mut messages)?,
        BackupMethod::Oauth => push_via_oauth(&repo, &config.backup, &mut messages)?,
        BackupMethod::Git => push_via_git(&repo, &config.backup, &avail, &mut messages)?,
    };

    Ok(BackupReport {
        method: selected.method,
        committed,
        pushed,
        messages,
    })
}

fn push_via_gh(
    repo: &GitRepo,
    cfg: &BackupConfig,
    avail: &Availability,
    messages: &mut Vec<String>,
) -> Result<bool> {
    let login = avail
        .gh_login
        .clone()
        .or_else(gh_login)
        .context("could not determine GitHub login from gh")?;
    gh_ensure_repo(&login, &cfg.repo_name, messages)?;
    let url = remote_https_url(&login, &cfg.repo_name);
    repo.set_remote("origin", &url)?;
    repo.push("origin", "main")?;
    messages.push(format!("pushed to {login}/{}", cfg.repo_name));
    Ok(true)
}

fn push_via_oauth(repo: &GitRepo, cfg: &BackupConfig, messages: &mut Vec<String>) -> Result<bool> {
    let client_id = oauth::client_id().context("no GitHub client id configured")?;
    let token = match oauth::load_token() {
        Some(t) => t,
        None => {
            let device = oauth::request_device_code(&client_id)?;
            messages.push(format!(
                "Open {} and enter code {}",
                device.verification_uri, device.user_code
            ));
            let token = oauth::poll_for_token(&client_id, &device)?;
            oauth::store_token(&token)?;
            token
        }
    };
    let login = oauth::login_for_token(&token)?;
    oauth::create_repo(&token, &cfg.repo_name)?;
    // Push with a one-shot authenticated URL; the persisted remote stays clean.
    let clean = remote_https_url(&login, &cfg.repo_name);
    repo.set_remote("origin", &clean)?;
    let auth_url = format!(
        "https://x-access-token:{token}@github.com/{login}/{}.git",
        cfg.repo_name
    );
    push_to_url(repo, &auth_url)?;
    messages.push(format!("pushed to {login}/{}", cfg.repo_name));
    Ok(true)
}

fn push_via_git(
    repo: &GitRepo,
    cfg: &BackupConfig,
    avail: &Availability,
    messages: &mut Vec<String>,
) -> Result<bool> {
    if let Some(url) = &cfg.remote_url {
        repo.set_remote("origin", url)?;
        repo.push("origin", "main")?;
        messages.push(format!("pushed to {url}"));
        return Ok(true);
    }

    if let Some(login) = &avail.gh_login {
        let url = remote_https_url(login, &cfg.repo_name);
        repo.set_remote("origin", &url)?;
        match repo.push("origin", "main") {
            Ok(()) => {
                messages.push(format!("pushed to {login}/{}", cfg.repo_name));
                return Ok(true);
            }
            Err(_) => {
                messages.push(format!(
                    "Could not push. Create the repo, then re-run backup: {}",
                    new_repo_link(&cfg.repo_name)
                ));
                return Ok(false);
            }
        }
    }

    messages.push(format!(
        "No remote configured. Create a repo here: {}  then set backup.remote_url in config.",
        new_repo_link(&cfg.repo_name)
    ));
    Ok(false)
}

fn push_to_url(repo: &GitRepo, url: &str) -> Result<()> {
    let output = Command::new("git")
        .args(["push", url, "main"])
        .current_dir(repo.workdir())
        .output()
        .context("git push")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git push failed: {}", scrub_token(stderr.trim()));
    }
    Ok(())
}

/// Remove any embedded `x-access-token:...@` credential from a message so a
/// token never leaks through an error string.
fn scrub_token(message: &str) -> String {
    let mut out = String::new();
    let mut rest = message;
    while let Some(idx) = rest.find("x-access-token:") {
        out.push_str(&rest[..idx]);
        out.push_str("x-access-token:<redacted>@");
        rest = match rest[idx..].find('@') {
            Some(at) => &rest[idx + at + 1..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Restore the store from the backup repository.
pub fn run_restore(paths: &Paths, config: &Config) -> Result<Vec<String>> {
    let avail = detect_availability();
    let mut messages = Vec::new();

    let url = if let Some(url) = &config.backup.remote_url {
        url.clone()
    } else if let Some(login) = avail
        .gh_login
        .clone()
        .or_else(|| oauth::load_token().and_then(|t| oauth::login_for_token(&t).ok()))
    {
        remote_https_url(&login, &config.backup.repo_name)
    } else {
        bail!("no backup remote known; set backup.remote_url or authenticate with gh");
    };

    let repo = GitRepo::new(paths.root().clone());
    if repo.is_initialized() {
        repo.set_remote("origin", &url)?;
        repo.pull("origin", "main")?;
        messages.push(format!("pulled latest from {url}"));
    } else {
        git::clone(&url, paths.root())?;
        messages.push(format!("cloned {url}"));
    }
    Ok(messages)
}

fn gh_auth_ok() -> bool {
    Command::new("gh")
        .args(["auth", "status"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn gh_login() -> Option<String> {
    let output = Command::new("gh")
        .args(["api", "user", "--jq", ".login"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let login = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if login.is_empty() { None } else { Some(login) }
}

fn gh_ensure_repo(login: &str, repo: &str, messages: &mut Vec<String>) -> Result<()> {
    let full = format!("{login}/{repo}");
    let exists = Command::new("gh")
        .args(["repo", "view", &full])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if exists {
        return Ok(());
    }
    let output = Command::new("gh")
        .args(["repo", "create", &full, "--private"])
        .output()
        .context("gh repo create")?;
    if !output.status.success() {
        bail!(
            "gh repo create failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    messages.push(format!("created private repo {full}"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_token_redacts_embedded_credentials() {
        let msg = "fatal: unable to access https://x-access-token:ghp_secret@github.com/u/r.git";
        let scrubbed = scrub_token(msg);
        assert!(!scrubbed.contains("ghp_secret"));
        assert!(scrubbed.contains("x-access-token:<redacted>@github.com/u/r.git"));
    }

    #[test]
    fn prepare_repo_initializes_and_ignores_generated() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let repo = prepare_repo(&paths).unwrap();
        assert!(repo.is_initialized());
        let ignore = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(ignore.contains("shell/"));
    }
}
