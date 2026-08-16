//! Automatic background sync: a detached, short-lived process that pulls on
//! every invocation and commits+pushes on every change whenever a backup remote
//! is resolvable. All the real work lives in [`run_sync`], which is synchronous
//! and testable; [`spawn_sync`] and [`sync_exec_main`] are thin shells around it.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::app::now_timestamp;
use crate::config::{BackupConfig, Config};
use crate::paths::Paths;
use crate::proc::own_process_group;
use crate::shell;
use crate::store::Store;

use super::git::{self, GitRepo, remote_has_content};
use super::plan::Availability;
use super::{detect_availability, oauth, prepare_repo, push_to_url, remote_https_url};

/// Hidden argv marker that runs the sync helper instead of the normal CLI.
pub const SYNC_EXEC_ARG: &str = "__sync-exec";

/// Env var that disables all automatic sync (used by tests and as an opt-out).
const DISABLE_ENV: &str = "CMD_MAN_DISABLE_SYNC";

/// A stale lock older than this is assumed to belong to a dead process.
const LOCK_STALE: Duration = Duration::from_secs(300);

/// Persisted outcome of the most recent background sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatus {
    pub last_attempt: String,
    pub outcome: String,
    pub message: String,
}

impl SyncStatus {
    fn new(last_attempt: &str, outcome: &str, message: impl Into<String>) -> Self {
        SyncStatus {
            last_attempt: last_attempt.to_string(),
            outcome: outcome.to_string(),
            message: message.into(),
        }
    }
}

/// Outcome codes written to the status file.
const OUTCOME_OK: &str = "ok";
const OUTCOME_BOOTSTRAPPED: &str = "bootstrapped";
const OUTCOME_NO_REMOTE: &str = "no-remote";
const OUTCOME_FAILED: &str = "failed";

/// Spawn the detached background sync process for this data root. Fire-and-die:
/// the parent never waits, and stdio is discarded so nothing touches the
/// terminal. A no-op under tests or when `CMD_MAN_DISABLE_SYNC` is set.
pub fn spawn_sync(paths: &Paths) {
    if cfg!(test) || std::env::var_os(DISABLE_ENV).is_some() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return,
    };
    let mut cmd = Command::new(exe);
    cmd.arg(SYNC_EXEC_ARG)
        // Pin the child to this exact data root rather than re-resolving env.
        .env("CMD_MAN_HOME", paths.root())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    own_process_group(&mut cmd);
    let _ = cmd.spawn();
}

/// Entry point for the hidden `__sync-exec` process. Always exits 0; operational
/// failures are recorded in the status file, not propagated.
pub fn sync_exec_main() -> ! {
    let _ = run_sync_exec();
    std::process::exit(0);
}

fn run_sync_exec() -> Result<()> {
    let paths = Paths::resolve()?;
    let config = Config::load(&paths)?;
    run_sync(&paths, &config, &now_timestamp())?;
    Ok(())
}

/// How to reach the resolved backup remote.
struct ResolvedRemote {
    /// Credential-free URL persisted as `origin`.
    clean_url: String,
    kind: RemoteKind,
}

enum RemoteKind {
    /// A user-configured `remote_url`; use ambient git credentials.
    Configured,
    /// Derived from an authenticated `gh`; use ambient git credentials.
    Gh,
    /// Derived from a stored OAuth token; embed it for network operations.
    Oauth { token: String },
}

impl ResolvedRemote {
    /// The URL to use for network operations, with a one-shot token when needed.
    fn network_url(&self) -> String {
        match &self.kind {
            RemoteKind::Oauth { token } => {
                self.clean_url
                    .replacen("https://", &format!("https://x-access-token:{token}@"), 1)
            }
            _ => self.clean_url.clone(),
        }
    }
}

/// Resolve a remote for silent background use, preferring the lowest-friction
/// path: an explicit remote URL, then an authenticated `gh`, then a stored OAuth
/// token. The interactive OAuth device flow is never started here.
fn resolve_sync_remote(cfg: &BackupConfig, avail: &Availability) -> Option<ResolvedRemote> {
    if let Some(url) = &cfg.remote_url {
        return Some(ResolvedRemote {
            clean_url: url.clone(),
            kind: RemoteKind::Configured,
        });
    }
    if avail.gh
        && !cfg.disable_gh
        && let Some(login) = &avail.gh_login
    {
        return Some(ResolvedRemote {
            clean_url: remote_https_url(login, &cfg.repo_name),
            kind: RemoteKind::Gh,
        });
    }
    if !cfg.disable_oauth
        && oauth::client_id().is_some()
        && let Some(token) = oauth::load_token()
        && let Ok(login) = oauth::login_for_token(&token)
    {
        return Some(ResolvedRemote {
            clean_url: remote_https_url(&login, &cfg.repo_name),
            kind: RemoteKind::Oauth { token },
        });
    }
    None
}

/// Run one full sync cycle. Records a status file and never returns an error for
/// ordinary operational failures (offline, conflicts): those become a `failed`
/// status so the foreground can surface them without being blocked.
pub fn run_sync(paths: &Paths, config: &Config, timestamp: &str) -> Result<()> {
    if !config.backup.auto_sync {
        return Ok(());
    }
    paths.ensure_dirs()?;

    let _lock = match SyncLock::acquire(paths)? {
        Some(lock) => lock,
        None => return Ok(()), // another sync is already running
    };

    let avail = detect_availability();
    let remote = match resolve_sync_remote(&config.backup, &avail) {
        Some(remote) => remote,
        None => {
            write_status(
                paths,
                SyncStatus::new(timestamp, OUTCOME_NO_REMOTE, "no backup remote resolvable"),
            );
            return Ok(());
        }
    };

    match sync_cycle(paths, config, &avail, &remote, timestamp) {
        Ok(outcome) => write_status(paths, SyncStatus::new(timestamp, outcome, "")),
        Err(e) => write_status(
            paths,
            SyncStatus::new(timestamp, OUTCOME_FAILED, format!("{e:#}")),
        ),
    }
    Ok(())
}

fn sync_cycle(
    paths: &Paths,
    config: &Config,
    avail: &Availability,
    remote: &ResolvedRemote,
    timestamp: &str,
) -> Result<&'static str> {
    let network_url = remote.network_url();
    let repo = GitRepo::new(paths.root().clone());

    // Fresh machine: clone the existing backup instead of pushing an empty store.
    if !repo.is_initialized() && store_is_empty(paths) && remote_has_content(&network_url) {
        bootstrap(paths, config, remote)?;
        return Ok(OUTCOME_BOOTSTRAPPED);
    }

    let repo = prepare_repo(paths)?;
    repo.commit_all(&format!("cmd-man backup {timestamp}"))?;

    if remote_has_content(&network_url) {
        let changed = pull(&repo, remote)?;
        if changed {
            regenerate_shells(paths, config)?;
        }
    }

    push(&repo, &config.backup, avail, remote)?;
    Ok(OUTCOME_OK)
}

/// Whether the local store has no user entries yet.
fn store_is_empty(paths: &Paths) -> bool {
    Store::load(paths)
        .map(|store| store.entries().is_empty())
        .unwrap_or(true)
}

/// Clone the remote into the data root and regenerate shell files. Only called
/// when the local store is empty, so it never clobbers local-only aliases.
fn bootstrap(paths: &Paths, config: &Config, remote: &ResolvedRemote) -> Result<()> {
    let root = paths.root();
    let staging = root.join(".sync-clone");
    if staging.exists() {
        fs::remove_dir_all(&staging).context("clearing stale clone staging dir")?;
    }
    git::clone(&remote.network_url(), &staging)?;

    let git_src = staging.join(".git");
    let git_dst = root.join(".git");
    if git_dst.exists() {
        fs::remove_dir_all(&git_dst).ok();
    }
    fs::rename(&git_src, &git_dst).context("moving cloned .git into place")?;
    fs::remove_dir_all(&staging).ok();

    let repo = GitRepo::new(root.clone());
    repo.set_remote("origin", &remote.clean_url)?;
    repo.reset_hard()?;
    regenerate_shells(paths, config)?;
    Ok(())
}

/// Pull the remote into the working tree. Returns whether tracked files changed.
/// On a merge failure the merge is aborted so local content is preserved.
fn pull(repo: &GitRepo, remote: &ResolvedRemote) -> Result<bool> {
    let before = repo.head_commit();
    let result = match remote.kind {
        RemoteKind::Oauth { .. } => repo.pull_url(&remote.network_url(), "main"),
        _ => {
            repo.set_remote("origin", &remote.clean_url)?;
            repo.pull("origin", "main")
        }
    };
    if let Err(e) = result {
        let _ = repo.abort_merge();
        return Err(e);
    }
    Ok(repo.head_commit() != before)
}

/// Ensure the remote repo exists, then push the current branch.
fn push(
    repo: &GitRepo,
    cfg: &BackupConfig,
    avail: &Availability,
    remote: &ResolvedRemote,
) -> Result<()> {
    ensure_remote_exists(cfg, avail, remote)?;
    repo.set_remote("origin", &remote.clean_url)?;
    match remote.kind {
        RemoteKind::Oauth { .. } => push_to_url(repo, &remote.network_url()),
        _ => repo.push("origin", "main"),
    }
}

fn ensure_remote_exists(
    cfg: &BackupConfig,
    avail: &Availability,
    remote: &ResolvedRemote,
) -> Result<()> {
    match &remote.kind {
        RemoteKind::Gh => {
            if let Some(login) = &avail.gh_login {
                super::gh_ensure_repo(login, &cfg.repo_name, &mut Vec::new())?;
            }
            Ok(())
        }
        RemoteKind::Oauth { token } => oauth::create_repo(token, &cfg.repo_name),
        RemoteKind::Configured => Ok(()),
    }
}

fn regenerate_shells(paths: &Paths, config: &Config) -> Result<()> {
    let store = Store::load(paths)?;
    shell::regenerate(
        paths,
        store.entries(),
        config.shells.zsh,
        config.shells.bash,
    )
}

fn write_status(paths: &Paths, status: SyncStatus) {
    // Failure messages can echo a git command line carrying an embedded token;
    // scrub before persisting so no credential ever lands on disk.
    let status = SyncStatus {
        message: super::scrub_token(&status.message),
        ..status
    };
    if let Ok(text) = toml::to_string_pretty(&status) {
        let _ = fs::write(paths.sync_state_file(), text);
    }
}

/// Read the last recorded sync status, if any.
pub fn read_status(paths: &Paths) -> Option<SyncStatus> {
    let text = fs::read_to_string(paths.sync_state_file()).ok()?;
    toml::from_str(&text).ok()
}

/// Message shown when the most recent background sync could not reach the remote.
const FAILURE_NOTICE: &str =
    "cmd-man: backup sync could not reach the remote; it will retry on your next change.";

/// Return a one-time notice if the last sync failed and has not been surfaced
/// yet, recording it so a persistent failure is reported once per new attempt
/// rather than on every command.
pub fn take_failure_notice(paths: &Paths) -> Option<String> {
    let status = read_status(paths)?;
    if status.outcome != OUTCOME_FAILED {
        return None;
    }
    let already = fs::read_to_string(paths.sync_notified_file()).ok();
    if already.as_deref().map(str::trim) == Some(status.last_attempt.as_str()) {
        return None;
    }
    let _ = fs::write(paths.sync_notified_file(), &status.last_attempt);
    Some(FAILURE_NOTICE.to_string())
}

/// An advisory lock ensuring a single background sync runs at a time.
struct SyncLock {
    path: PathBuf,
}

impl SyncLock {
    fn acquire(paths: &Paths) -> Result<Option<SyncLock>> {
        let path = paths.sync_lock_file();
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "{}", std::process::id());
                Ok(Some(SyncLock { path }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if lock_is_stale(&path) {
                    let _ = fs::remove_file(&path);
                    return SyncLock::acquire(paths);
                }
                Ok(None)
            }
            Err(e) => Err(e).context("acquiring sync lock"),
        }
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn lock_is_stale(path: &PathBuf) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age > LOCK_STALE)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avail(gh: bool, login: Option<&str>) -> Availability {
        Availability {
            gh,
            git: true,
            oauth: false,
            gh_login: login.map(|s| s.to_string()),
        }
    }

    #[test]
    fn resolve_prefers_configured_url() {
        let cfg = BackupConfig {
            remote_url: Some("https://example.com/x.git".into()),
            ..Default::default()
        };
        let remote = resolve_sync_remote(&cfg, &avail(true, Some("me"))).unwrap();
        assert_eq!(remote.clean_url, "https://example.com/x.git");
        assert!(matches!(remote.kind, RemoteKind::Configured));
        // Configured/gh remotes carry no embedded credentials.
        assert_eq!(remote.network_url(), remote.clean_url);
    }

    #[test]
    fn resolve_uses_gh_when_no_configured_url() {
        let cfg = BackupConfig::default();
        let remote = resolve_sync_remote(&cfg, &avail(true, Some("brennacodes"))).unwrap();
        assert_eq!(
            remote.clean_url,
            "https://github.com/brennacodes/cmd-man-backup.git"
        );
        assert!(matches!(remote.kind, RemoteKind::Gh));
    }

    #[test]
    fn resolve_none_when_gh_disabled_and_nothing_else() {
        let cfg = BackupConfig {
            disable_gh: true,
            disable_oauth: true,
            ..Default::default()
        };
        assert!(resolve_sync_remote(&cfg, &avail(true, Some("me"))).is_none());
    }

    #[test]
    fn oauth_network_url_embeds_token() {
        let remote = ResolvedRemote {
            clean_url: "https://github.com/me/cmd-man-backup.git".into(),
            kind: RemoteKind::Oauth {
                token: "gho_secret".into(),
            },
        };
        assert_eq!(
            remote.network_url(),
            "https://x-access-token:gho_secret@github.com/me/cmd-man-backup.git"
        );
    }

    #[test]
    fn status_file_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        write_status(
            &paths,
            SyncStatus::new("2026-08-16T00:00:00Z", OUTCOME_FAILED, "offline"),
        );
        let back = read_status(&paths).unwrap();
        assert_eq!(back.outcome, OUTCOME_FAILED);
        assert_eq!(back.message, "offline");
        assert_eq!(back.last_attempt, "2026-08-16T00:00:00Z");
    }

    #[test]
    fn lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let first = SyncLock::acquire(&paths).unwrap();
        assert!(first.is_some());
        assert!(SyncLock::acquire(&paths).unwrap().is_none());
        drop(first);
        assert!(SyncLock::acquire(&paths).unwrap().is_some());
    }

    #[test]
    fn disabled_auto_sync_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let mut config = Config::default();
        config.backup.auto_sync = false;
        run_sync(&paths, &config, "2026-08-16T00:00:00Z").unwrap();
        assert!(read_status(&paths).is_none());
    }

    #[test]
    fn write_status_scrubs_embedded_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let leaky = "git pull https://x-access-token:ghp_secret@github.com/u/r.git main failed";
        write_status(
            &paths,
            SyncStatus::new("2026-08-16T00:00:00Z", OUTCOME_FAILED, leaky),
        );
        let back = read_status(&paths).unwrap();
        assert!(
            !back.message.contains("ghp_secret"),
            "token leaked: {}",
            back.message
        );
        assert!(back.message.contains("x-access-token:<redacted>@"));
    }
}
