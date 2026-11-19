//! Automatic background sync: a detached, short-lived process that pulls on
//! every invocation and commits+pushes on every change whenever a backup remote
//! is resolvable. All the real work lives in [`run_sync`], which is synchronous
//! and testable; [`spawn_sync`] and [`sync_exec_main`] are thin shells around it.

use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

use crate::app::now_timestamp;
use crate::config::{BackupConfig, Config};
use crate::model::EntryKey;
use crate::paths::Paths;
use crate::proc::new_session;
use crate::scopes::ScopeRegistry;
use crate::shell;
use crate::store::{Store, StoreLock, try_flock};

use super::git::{self, GitRepo, remote_has_content};
use super::plan::Availability;
use super::{
    commit_snapshot, detect_availability, merge_fetched_safely, oauth, prepare_repo, push_to_url,
    remote_https_url,
};

/// Hidden argv marker that runs the sync helper instead of the normal CLI.
pub const SYNC_EXEC_ARG: &str = "__sync-exec";

/// Env var that disables all automatic sync (used by tests and as an opt-out).
const DISABLE_ENV: &str = "CMD_MAN_DISABLE_SYNC";

/// Overrides how long the background helper may run, in seconds.
const DEADLINE_ENV: &str = "CMD_MAN_SYNC_DEADLINE_SECS";
const HELPER_DEADLINE: Duration = Duration::from_secs(120);

/// How long a command that changed data waits to report its own sync.
const CHANGE_SYNC_WAIT: Duration = Duration::from_secs(10);
const CHANGE_SYNC_POLL: Duration = Duration::from_millis(50);
/// A sync finishing within this needs no progress message.
const CHANGE_SYNC_QUIET: Duration = Duration::from_millis(100);

/// A busy sync can be asked to run again, but not indefinitely.
const MAX_SYNC_CYCLES: u32 = 3;

/// The attempt on record when this process first started a sync for a change.
static CHANGE_SYNC_BASELINE: OnceLock<Option<String>> = OnceLock::new();

/// Persisted outcome of the most recent background sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncStatus {
    pub last_attempt: String,
    pub outcome: String,
    pub message: String,
    #[serde(default)]
    pub remote: String,
}

impl SyncStatus {
    fn new(last_attempt: &str, outcome: &str, message: impl Into<String>) -> Self {
        SyncStatus {
            last_attempt: last_attempt.to_string(),
            outcome: outcome.to_string(),
            message: message.into(),
            remote: String::new(),
        }
    }

    fn for_remote(mut self, url: &str) -> Self {
        self.remote = without_credentials(url);
        self
    }

    pub fn found_no_remote(&self) -> bool {
        self.outcome == OUTCOME_NO_REMOTE
    }

    pub fn failed(&self) -> bool {
        failure_text(&self.outcome).is_some()
    }
}

/// Outcome codes written to the status file.
const OUTCOME_OK: &str = "ok";
const OUTCOME_BOOTSTRAPPED: &str = "bootstrapped";
const OUTCOME_NO_REMOTE: &str = "no-remote";
const OUTCOME_FAILED: &str = "failed";
const OUTCOME_COMMIT_FAILED: &str = "commit-failed";
const OUTCOME_PULL_FAILED: &str = "pull-failed";
const OUTCOME_PUSH_FAILED: &str = "push-failed";

struct SyncFailure {
    outcome: &'static str,
    error: anyhow::Error,
}

impl SyncFailure {
    fn at(outcome: &'static str) -> impl FnOnce(anyhow::Error) -> SyncFailure {
        move |error| SyncFailure { outcome, error }
    }
}

impl From<anyhow::Error> for SyncFailure {
    fn from(error: anyhow::Error) -> Self {
        SyncFailure {
            outcome: OUTCOME_FAILED,
            error,
        }
    }
}

/// Spawn the detached background sync process for this data root. Fire-and-die:
/// the parent never waits, and stdio is discarded so nothing touches the
/// terminal. A no-op under tests or when `CMD_MAN_DISABLE_SYNC` is set.
pub fn spawn_sync(paths: &Paths) -> bool {
    if cfg!(test) || std::env::var_os(DISABLE_ENV).is_some() {
        return false;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(_) => return false,
    };
    let mut cmd = Command::new(exe);
    cmd.arg(SYNC_EXEC_ARG)
        // Pin the child to this exact data root rather than re-resolving env.
        .env("CMD_MAN_HOME", paths.root())
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    new_session(&mut cmd);
    cmd.spawn().is_ok()
}

/// [`spawn_sync`] for a sync the user caused, whose failure must be reported
/// even when it repeats the previous one.
pub fn spawn_change_sync(paths: &Paths) -> bool {
    let before = last_sync_attempt(paths);
    let marker = paths.sync_report_pending_file();
    let _ = fs::write(&marker, before.as_deref().unwrap_or(""));
    let started = spawn_sync(paths);
    if started {
        let _ = CHANGE_SYNC_BASELINE.set(before);
    } else {
        let _ = fs::remove_file(&marker);
    }
    started
}

pub fn change_sync_started() -> bool {
    CHANGE_SYNC_BASELINE.get().is_some()
}

pub fn last_sync_attempt(paths: &Paths) -> Option<String> {
    read_status(paths).map(|status| status.last_attempt)
}

/// Wait for the sync this command started and return its failure report. A sync
/// that outlasts the wait keeps running and is reported on the next use.
pub fn await_change_sync(paths: &Paths) -> Option<String> {
    let before = CHANGE_SYNC_BASELINE.get()?;
    if !Config::load(paths).is_ok_and(|config| config.backup.auto_sync) {
        return None;
    }
    if !wait_for_new_attempt(paths, before.as_deref(), CHANGE_SYNC_QUIET) {
        let interactive = std::io::stderr().is_terminal();
        if interactive {
            eprintln!("cmd-man: syncing backup (up to 10s, Ctrl-C is safe)...");
        }
        if !wait_for_new_attempt(paths, before.as_deref(), CHANGE_SYNC_WAIT) {
            if interactive {
                eprintln!("cmd-man: backup sync is still running; any failure will show next time");
            }
            return None;
        }
    }
    take_failure_notice(paths)
}

fn wait_for_new_attempt(paths: &Paths, before: Option<&str>, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        let finished =
            last_sync_attempt(paths).is_some_and(|attempt| Some(attempt.as_str()) != before);
        if finished && !lock_is_held(paths) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(CHANGE_SYNC_POLL);
    }
}

/// Wait for any running sync to finish, returning whether one still holds the
/// lock at the limit.
pub fn await_sync_idle(paths: &Paths) -> bool {
    wait_until_unlocked(paths, CHANGE_SYNC_WAIT)
}

fn wait_until_unlocked(paths: &Paths, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    while lock_is_held(paths) {
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(CHANGE_SYNC_POLL);
    }
    true
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
    let timestamp = now_timestamp();
    enforce_deadline(paths.clone(), timestamp.clone(), helper_deadline());
    run_sync(&paths, &config, &timestamp)?;
    Ok(())
}

fn helper_deadline() -> Duration {
    std::env::var(DEADLINE_ENV)
        .ok()
        .and_then(|seconds| seconds.parse().ok())
        .map_or(HELPER_DEADLINE, Duration::from_secs)
}

/// End the helper and everything it started at the deadline, so a remote or
/// credential program that never answers cannot hold the lock forever.
fn enforce_deadline(paths: Paths, timestamp: String, limit: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(limit);
        let message = format!("the sync did not finish within {} seconds", limit.as_secs());
        write_status(&paths, SyncStatus::new(&timestamp, OUTCOME_FAILED, message));
        // SAFETY: both calls only inspect or signal this helper's own group.
        unsafe {
            if libc::getpgrp() == libc::getpid() {
                libc::killpg(libc::getpgrp(), libc::SIGKILL);
            }
        }
        std::process::exit(1);
    });
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
    paths.ensure_dirs()?;

    let rerun = paths.sync_rerun_file();
    let _lock = match SyncLock::acquire(paths)? {
        Some(lock) => lock,
        None => {
            let _ = fs::write(&rerun, "");
            return Ok(());
        }
    };

    // An interrupted clone can leave an OAuth token in the staging repo's config.
    let _ = fs::remove_dir_all(clone_staging(paths));
    if !config.backup.auto_sync {
        return Ok(());
    }

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

    let mut cycles = 0;
    let result = loop {
        let _ = fs::remove_file(&rerun);
        let result = sync_cycle(paths, config, &avail, &remote, timestamp);
        cycles += 1;
        if cycles == MAX_SYNC_CYCLES || !rerun.exists() {
            break result;
        }
    };

    match result {
        Ok(outcome) => write_status(paths, SyncStatus::new(timestamp, outcome, "")),
        Err(failure) => write_status(
            paths,
            SyncStatus::new(timestamp, failure.outcome, format!("{:#}", failure.error))
                .for_remote(&remote.clean_url),
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
) -> Result<&'static str, SyncFailure> {
    let network_url = remote.network_url();
    let repo = GitRepo::new(paths.root().clone());

    // Fresh machine: clone the existing backup instead of pushing an empty store.
    let interrupted_clone = paths.sync_bootstrap_pending_file();
    if repo.is_initialized() {
        let _ = fs::remove_file(&interrupted_clone);
    }
    let resuming = !repo.is_initialized() && interrupted_clone.exists();
    if resuming || (!repo.is_initialized() && store_is_empty(paths)) {
        if remote_has_content(&network_url) {
            bootstrap(paths, config, remote)?;
            return Ok(OUTCOME_BOOTSTRAPPED);
        }
        if resuming {
            if !git::remote_is_reachable(&network_url) {
                return Err(anyhow::anyhow!(
                    "could not reach the backup to finish the first clone"
                )
                .into());
            }
            let _ = fs::remove_file(&interrupted_clone);
        }
    }

    let repo = prepare_repo(paths)?;
    let marker = paths.sync_regen_pending_file();
    if marker.exists() {
        let _store_lock = StoreLock::acquire(paths)?;
        regenerate_pulled(paths, config)?;
    }
    commit_snapshot(paths, &repo, timestamp).map_err(SyncFailure::at(OUTCOME_COMMIT_FAILED))?;

    if remote_has_content(&network_url) {
        fs::write(&marker, "").context("marking shell regeneration as pending")?;
        if pull(&repo, remote, paths).map_err(SyncFailure::at(OUTCOME_PULL_FAILED))? {
            let _store_lock = StoreLock::acquire(paths)?;
            regenerate_shells(paths, config)?;
        }
        fs::remove_file(&marker).context("clearing the pending shell regeneration")?;
    }

    push(&repo, &config.backup, avail, remote).map_err(SyncFailure::at(OUTCOME_PUSH_FAILED))?;
    Ok(OUTCOME_OK)
}

/// Whether the local store has no user entries yet.
fn store_is_empty(paths: &Paths) -> bool {
    Store::load(paths)
        .map(|store| store.entries().is_empty())
        .unwrap_or(false)
}

/// Clone the remote into the data root and regenerate shell files. Only called
/// when the local store is empty, so it never clobbers local-only aliases.
fn bootstrap(paths: &Paths, config: &Config, remote: &ResolvedRemote) -> Result<()> {
    let root = paths.root();
    let staging = clone_staging(paths);
    git::clone(&remote.network_url(), &staging)?;
    GitRepo::new(staging.clone()).set_remote("origin", &remote.clean_url)?;

    let _store_lock = StoreLock::acquire(paths)?;
    let pending = paths.sync_bootstrap_pending_file();
    if !local_entries_are_in_clone(paths, &staging) {
        let _ = fs::remove_file(&pending);
        anyhow::bail!("this machine has entries that are not in the backup yet");
    }
    fs::write(&pending, "").context("marking the first clone as in progress")?;

    // The repo must appear last: a repo over an unpopulated tree would make the
    // next sync commit every backed-up file as deleted.
    restore_backup(&staging, root).context("restoring the backup into place")?;
    fs::write(paths.sync_regen_pending_file(), "")
        .context("marking shell regeneration as pending")?;

    let git_src = staging.join(".git");
    let git_dst = root.join(".git");
    if git_dst.exists() {
        fs::remove_dir_all(&git_dst).ok();
    }
    fs::rename(&git_src, &git_dst).context("moving cloned .git into place")?;
    fs::remove_file(&pending).context("marking the first clone as finished")?;
    fs::remove_dir_all(&staging).ok();

    GitRepo::new(root.clone()).reset_hard()?;
    regenerate_pulled(paths, config)
}

/// Whether restoring the clone would lose nothing. Anything else has to go
/// through a merge, where a conflict is reported instead of overwritten.
fn local_entries_are_in_clone(paths: &Paths, staging: &Path) -> bool {
    let staged = Store::load(&Paths::with_root(staging.to_path_buf()));
    match (Store::load(paths), staged) {
        (Ok(local), Ok(staged)) => local
            .entries()
            .iter()
            .all(|entry| staged.find(&EntryKey::of(entry)) == Some(entry)),
        _ => false,
    }
}

fn clone_staging(paths: &Paths) -> PathBuf {
    paths.root().join(".sync-clone")
}

/// Copy a clone's files over `root`, publishing `categories` last and in one
/// step, so an interruption leaves a store that still reads as empty.
fn restore_backup(staging: &Path, root: &Path) -> Result<()> {
    for entry in fs::read_dir(staging)? {
        let entry = entry?;
        let name = entry.file_name();
        if name != ".git" && name != "categories" {
            copy_entry(&entry.path(), &root.join(name))?;
        }
    }
    let categories = staging.join("categories");
    if categories.exists() && fs::rename(&categories, root.join("categories")).is_err() {
        copy_entry(&categories, &root.join("categories"))?;
    }
    Ok(())
}

fn copy_entry(from: &Path, to: &Path) -> Result<()> {
    let kind = fs::symlink_metadata(from)?.file_type();
    let in_the_way = fs::symlink_metadata(to).is_ok_and(|existing| {
        existing.file_type().is_symlink() || (kind.is_symlink() && !existing.is_dir())
    });
    if in_the_way {
        fs::remove_file(to)?;
    }
    if kind.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_entry(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else if kind.is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(from)?, to)?;
    } else {
        fs::copy(from, to)?;
    }
    Ok(())
}

/// Pull the remote into the working tree. Returns whether tracked files changed.
/// On a merge failure the merge is aborted so local content is preserved.
fn pull(repo: &GitRepo, remote: &ResolvedRemote, paths: &Paths) -> Result<bool> {
    let before = repo.head_commit();
    match remote.kind {
        RemoteKind::Oauth { .. } => repo.fetch(&remote.network_url(), "main")?,
        _ => {
            repo.set_remote("origin", &remote.clean_url)?;
            repo.fetch("origin", "main")?;
        }
    }
    merge_fetched_safely(paths, repo)?;
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

fn regenerate_pulled(paths: &Paths, config: &Config) -> Result<()> {
    let marker = paths.sync_regen_pending_file();
    fs::write(&marker, "").context("marking shell regeneration as pending")?;
    regenerate_shells(paths, config)?;
    fs::remove_file(&marker).context("clearing the pending shell regeneration")?;
    Ok(())
}

fn regenerate_shells(paths: &Paths, config: &Config) -> Result<()> {
    let store = Store::load(paths)?;
    let scopes = ScopeRegistry::load(paths)?;
    shell::regenerate(
        paths,
        store.entries(),
        &scopes,
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

struct FailureText {
    problem: &'static str,
    effect: &'static str,
    fix: &'static str,
}

fn failure_text(outcome: &str) -> Option<FailureText> {
    let text = |problem, effect, fix| {
        Some(FailureText {
            problem,
            effect,
            fix,
        })
    };
    match outcome {
        OUTCOME_COMMIT_FAILED => text(
            "could not commit your changes",
            "Your latest changes are saved on this machine and your aliases still work, but they are not backed up.",
            "Read the log above, clear what it reports, then run `cmd-man sync`.",
        ),
        OUTCOME_PULL_FAILED => text(
            "could not pull from the remote",
            "Your changes are saved on this machine, but they have not been combined with your backup or pushed.",
            "Check your connection and access to the backup repository, then run `cmd-man sync`.",
        ),
        OUTCOME_PUSH_FAILED => text(
            "could not push to the remote",
            "Your changes are saved on this machine, but your backup does not have them yet.",
            "Check your connection and access to the backup repository, then run `cmd-man sync`.",
        ),
        OUTCOME_FAILED => text(
            "failed",
            "Your changes are saved on this machine, but this sync did not finish.",
            "Run `cmd-man sync` to try again. If it fails the same way, read the log above.",
        ),
        _ => None,
    }
}

const CONFLICT_FIX: &str = "In the folder above run `git merge --no-edit --allow-unrelated-histories FETCH_HEAD`, edit the marked files to keep what you want and delete the <<<<<<<, ======= and >>>>>>> lines, run `git add -A && git commit --no-edit` before using cmd-man again, then run `cmd-man sync`.";
const DUPLICATE_FIX: &str = "That name was added on two machines. Rename or remove it here with `cmd-man edit` or `cmd-man rm`, then run `cmd-man sync`.";
const MARKERS_WHY: &str = "an entry file still has merge conflict markers";
const MARKERS_FIX: &str = "In the folder above delete the <<<<<<<, ======= and >>>>>>> lines so each entry is valid, run `git add -A && git commit -m \"Resolve cmd-man conflict\"`, then run `cmd-man sync`.";
const SSH_KEY_FIX: &str = "Load an SSH key the backup accepts (check with `ssh-add -l`, and `ssh -T git@github.com` for GitHub), then run `cmd-man sync`.";
const NETWORK_WHY: &str = "could not reach the backup (no network connection?)";
const NETWORK_FIX: &str = "Reconnect, then run `cmd-man sync`.";
const SIGN_IN_WHY: &str = "the backup rejected this machine's sign-in";
const SIGN_IN_FIX: &str =
    "Sign in with `gh auth login` or `cmd-man backup`, then run `cmd-man sync`.";

fn mentions_any(message: &str, phrases: &[&str]) -> bool {
    phrases.iter().any(|phrase| message.contains(phrase))
}

/// The most telling recorded line, without git's command echo or severity.
fn readable_cause(message: &str) -> String {
    let mut lines = message.lines().filter(|line| !line.trim().is_empty());
    let cause = lines
        .clone()
        .find(|line| mentions_any(line, &["CONFLICT", "fatal:", "error:"]));
    let line = cause
        .or_else(|| lines.next())
        .unwrap_or("no details were recorded");
    let detail = line.split_once(" failed: ").map_or(line, |(_, rest)| rest);
    let detail = detail
        .strip_prefix("fatal: ")
        .or_else(|| detail.strip_prefix("error: "))
        .unwrap_or(detail);
    let readable = if detail.trim().is_empty() {
        line
    } else {
        detail
    };
    readable.chars().filter(|c| !c.is_control()).collect()
}

/// What went wrong and what to do, in terms of the causes people can act on.
fn why_and_fix(status: &SyncStatus, text: &FailureText) -> (String, &'static str) {
    let message = &status.message;
    let reached_for_the_remote = status.outcome != OUTCOME_COMMIT_FAILED;
    if reached_for_the_remote
        && mentions_any(
            message,
            &[
                "Could not resolve host",
                "Network is unreachable",
                "Connection refused",
                "Connection timed out",
            ],
        )
    {
        return (NETWORK_WHY.to_string(), NETWORK_FIX);
    }
    if reached_for_the_remote
        && mentions_any(
            message,
            &[
                "could not read Username",
                "Authentication failed",
                "returned error: 403",
            ],
        )
    {
        return (SIGN_IN_WHY.to_string(), SIGN_IN_FIX);
    }
    if reached_for_the_remote && message.contains("Permission denied (publickey)") {
        return (SIGN_IN_WHY.to_string(), SSH_KEY_FIX);
    }
    if message.contains("duplicate entry") {
        return (readable_cause(message), DUPLICATE_FIX);
    }
    if message.contains("<<<<<<<") {
        return (MARKERS_WHY.to_string(), MARKERS_FIX);
    }
    let fix = if message.contains("CONFLICT") {
        CONFLICT_FIX
    } else {
        text.fix
    };
    (readable_cause(message), fix)
}

fn without_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}/{path}"),
        None => url.to_string(),
    }
}

fn describe_attempt(attempt: &str, now: OffsetDateTime) -> String {
    let Ok(at) = OffsetDateTime::parse(attempt, &Rfc3339) else {
        return attempt.to_string();
    };
    let plural = |count: i64, unit: &str| {
        let suffix = if count == 1 { "" } else { "s" };
        format!("{count} {unit}{suffix} ago")
    };
    let seconds = (now - at).whole_seconds();
    let age = match seconds {
        ..60 => "just now".to_string(),
        60..3600 => plural(seconds / 60, "minute"),
        3600..86400 => plural(seconds / 3600, "hour"),
        _ => plural(seconds / 86400, "day"),
    };
    let clock = at
        .to_offset(UtcOffset::UTC)
        .format(format_description!("[year]-[month]-[day] [hour]:[minute]"))
        .unwrap_or_else(|_| attempt.to_string());
    format!("{age} ({clock} UTC)")
}

fn failure_report(paths: &Paths, status: &SyncStatus, text: &FailureText) -> String {
    let (why, fix) = why_and_fix(status, text);
    let when = describe_attempt(&status.last_attempt, OffsetDateTime::now_utc());
    let mut place = format!("this machine ({})", paths.root().display());
    if !status.remote.is_empty() {
        place.push_str(&format!(", backup {}", status.remote));
    }
    format!(
        "cmd-man: backup sync {}.\n  When:   {when}\n  Where:  {place}\n  Why:    {why}\n  Effect: {}\n  Log:    {}\n  Fix:    {fix}",
        text.problem,
        text.effect,
        paths.sync_state_file().display()
    )
}

/// Return a report if the last sync failed and has not been surfaced yet. A sync
/// the user caused is always reported, even from a later process; otherwise a
/// failure at the same stage as the last report stays quiet so a persistent
/// problem is not repeated on every read-only command.
pub fn take_failure_notice(paths: &Paths) -> Option<String> {
    take_failure(paths, change_report_is_due(paths))
}

/// Whether a newer attempt has replaced the one on record when a change started
/// its sync, so an older failure is never mistaken for the change's own.
fn change_report_is_due(paths: &Paths) -> bool {
    let Ok(before) = fs::read_to_string(paths.sync_report_pending_file()) else {
        return false;
    };
    last_sync_attempt(paths).is_some_and(|attempt| attempt != before.trim())
}

/// The report for a known failure, recorded so it is not shown again.
pub fn claim_failure_report(paths: &Paths, status: &SyncStatus) -> Option<String> {
    let text = failure_text(&status.outcome)?;
    let _ = fs::write(
        paths.sync_notified_file(),
        format!("{}\n{}\n", status.last_attempt, status.outcome),
    );
    Some(failure_report(paths, status, &text))
}

fn take_failure(paths: &Paths, report_repeats: bool) -> Option<String> {
    let status = read_status(paths)?;
    if !status.failed() {
        let _ = fs::remove_file(paths.sync_notified_file());
        let _ = fs::remove_file(paths.sync_report_pending_file());
        return None;
    }
    let notified = fs::read_to_string(paths.sync_notified_file()).unwrap_or_default();
    let mut notified = notified.lines();
    if notified.next() == Some(status.last_attempt.as_str()) {
        return None;
    }
    if !report_repeats && notified.next() == Some(status.outcome.as_str()) {
        return None;
    }
    let report = claim_failure_report(paths, &status);
    if report_repeats {
        let _ = fs::remove_file(paths.sync_report_pending_file());
    }
    report
}

/// A kernel lock ensuring a single background sync runs at a time. It ends with
/// its holder however that process exits. The file records the holder so a
/// waiter only has to touch the lock while a sync is claiming it.
struct SyncLock {
    file: File,
}

impl SyncLock {
    fn acquire(paths: &Paths) -> Result<Option<SyncLock>> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(paths.sync_lock_file())
            .context("acquiring sync lock")?;
        if !try_flock(&file, libc::LOCK_EX) {
            return Ok(None);
        }
        let _ = file.set_len(0);
        let _ = writeln!(file, "{}", std::process::id());
        Ok(Some(SyncLock { file }))
    }
}

impl Drop for SyncLock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        try_flock(&self.file, libc::LOCK_UN);
    }
}

fn lock_is_held(paths: &Paths) -> bool {
    let path = paths.sync_lock_file();
    if !fs::read_to_string(&path).is_ok_and(|holder| !holder.trim().is_empty()) {
        return false;
    }
    File::open(&path).is_ok_and(|file| {
        let free = try_flock(&file, libc::LOCK_SH);
        if free {
            try_flock(&file, libc::LOCK_UN);
        }
        !free
    })
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
    fn a_status_from_before_stages_existed_still_gives_a_notice_once() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        fs::write(
            paths.sync_state_file(),
            "last_attempt = \"2026-08-16T00:00:00Z\"\noutcome = \"failed\"\nmessage = \"offline\"\n",
        )
        .unwrap();
        let notice = take_failure_notice(&paths).unwrap();
        assert!(
            notice.starts_with("cmd-man: backup sync failed."),
            "{notice}"
        );
        assert!(take_failure_notice(&paths).is_none());
    }

    fn paths_with_failure(dir: &tempfile::TempDir, attempt: &str, outcome: &str) -> Paths {
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        write_status(&paths, SyncStatus::new(attempt, outcome, "first\nsecond"));
        paths
    }

    #[test]
    fn the_report_says_what_when_where_why_the_effect_and_the_fix() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_failure(&dir, "2026-08-16T00:00:00Z", OUTCOME_COMMIT_FAILED);
        let report = take_failure(&paths, false).unwrap();
        assert!(report.contains("could not commit your changes"), "{report}");
        assert!(report.contains("2026-08-16 00:00 UTC"), "{report}");
        assert!(
            report.contains(&format!("this machine ({})", paths.root().display())),
            "{report}"
        );
        assert!(report.contains("Why:    first"), "{report}");
        assert!(!report.contains("second"), "{report}");
        assert!(report.contains("Effect: "), "{report}");
        assert!(report.contains("`cmd-man sync`"), "{report}");
    }

    #[test]
    fn control_characters_in_the_error_do_not_reach_the_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        write_status(
            &paths,
            SyncStatus::new("t1", OUTCOME_PUSH_FAILED, "bad\u{1b}[2Jthing"),
        );
        let report = take_failure(&paths, false).unwrap();
        assert!(!report.contains('\u{1b}'), "{report}");
    }

    #[test]
    fn a_repeat_at_the_same_stage_stays_quiet_on_read_only_use() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_failure(&dir, "t1", OUTCOME_PUSH_FAILED);
        assert!(take_failure(&paths, false).is_some());
        write_status(&paths, SyncStatus::new("t2", OUTCOME_PUSH_FAILED, "again"));
        assert!(take_failure(&paths, false).is_none());
    }

    #[test]
    fn a_repeat_is_reported_when_the_user_caused_the_sync() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_failure(&dir, "t1", OUTCOME_PUSH_FAILED);
        assert!(take_failure(&paths, true).is_some());
        write_status(&paths, SyncStatus::new("t2", OUTCOME_PUSH_FAILED, "again"));
        assert!(take_failure(&paths, true).is_some());
        assert!(take_failure(&paths, true).is_none());
    }

    #[test]
    fn a_different_stage_or_a_recovery_reports_the_next_failure() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_failure(&dir, "t1", OUTCOME_PUSH_FAILED);
        assert!(take_failure(&paths, false).is_some());
        write_status(&paths, SyncStatus::new("t2", OUTCOME_PULL_FAILED, "x"));
        assert!(take_failure(&paths, false).is_some());
        write_status(&paths, SyncStatus::new("t3", OUTCOME_OK, ""));
        assert!(take_failure(&paths, false).is_none());
        write_status(&paths, SyncStatus::new("t4", OUTCOME_PULL_FAILED, "x"));
        assert!(take_failure(&paths, false).is_some());
    }

    #[test]
    fn a_change_outlasting_the_wait_is_reported_by_a_later_process() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_failure(&dir, "t1", OUTCOME_PUSH_FAILED);
        assert!(take_failure_notice(&paths).is_some());

        fs::write(paths.sync_report_pending_file(), "t1").unwrap();
        write_status(&paths, SyncStatus::new("t2", OUTCOME_PUSH_FAILED, "slow"));
        assert!(take_failure_notice(&paths).is_some());
        assert!(!paths.sync_report_pending_file().exists());

        write_status(&paths, SyncStatus::new("t3", OUTCOME_PUSH_FAILED, "slow"));
        assert!(take_failure_notice(&paths).is_none());
    }

    #[test]
    fn a_change_in_flight_does_not_surface_an_older_quiet_failure() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_failure(&dir, "t1", OUTCOME_PUSH_FAILED);
        assert!(take_failure_notice(&paths).is_some());
        write_status(&paths, SyncStatus::new("t2", OUTCOME_PUSH_FAILED, "quiet"));

        fs::write(paths.sync_report_pending_file(), "t2").unwrap();
        assert!(take_failure_notice(&paths).is_none());
        assert!(paths.sync_report_pending_file().exists());

        write_status(&paths, SyncStatus::new("t3", OUTCOME_PUSH_FAILED, "mine"));
        let report = take_failure_notice(&paths).unwrap();
        assert!(report.contains("mine"), "{report}");
    }

    #[test]
    fn a_failure_someone_else_already_saw_can_still_be_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_failure(&dir, "t1", OUTCOME_PUSH_FAILED);
        assert!(take_failure(&paths, false).is_some());
        let status = read_status(&paths).unwrap();
        assert!(claim_failure_report(&paths, &status).is_some());
    }

    #[test]
    fn only_one_holder_can_take_a_lock_left_by_a_dead_process() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        fs::write(paths.sync_lock_file(), "999999\n").unwrap();
        let holders: Vec<_> = (0..6).map(|_| SyncLock::acquire(&paths).unwrap()).collect();
        assert_eq!(holders.iter().filter(|lock| lock.is_some()).count(), 1);
    }

    #[test]
    fn releasing_the_lock_leaves_the_file_unclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        drop(SyncLock::acquire(&paths).unwrap().unwrap());
        assert!(paths.sync_lock_file().exists());
        assert!(!lock_is_held(&paths));
    }

    #[test]
    fn the_report_prefers_the_line_naming_the_cause() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        write_status(
            &paths,
            SyncStatus::new(
                "t1",
                OUTCOME_PULL_FAILED,
                "From /remote\nCONFLICT (add/add): general.toml",
            ),
        );
        let report = take_failure(&paths, false).unwrap();
        assert!(report.contains("Why:    CONFLICT"), "{report}");
    }

    #[test]
    fn a_lock_left_by_a_dead_process_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        fs::write(paths.sync_lock_file(), "999999\n").unwrap();
        assert!(!lock_is_held(&paths));
        assert!(SyncLock::acquire(&paths).unwrap().is_some());
    }

    #[test]
    fn a_lock_held_by_a_live_process_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let _lock = SyncLock::acquire(&paths).unwrap().unwrap();
        assert!(lock_is_held(&paths));
        assert!(!wait_until_unlocked(&paths, Duration::from_millis(120)));
    }

    #[test]
    fn a_sync_finding_the_lock_held_asks_for_another_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let _lock = SyncLock::acquire(&paths).unwrap().unwrap();
        run_sync(&paths, &Config::default(), "t1").unwrap();
        assert!(paths.sync_rerun_file().exists());
        assert!(read_status(&paths).is_none());
    }

    fn staged_backup() -> (tempfile::TempDir, tempfile::TempDir) {
        let staging = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(staging.path().join(".git")).unwrap();
        fs::create_dir_all(staging.path().join("categories")).unwrap();
        fs::write(staging.path().join("categories/g.toml"), "x").unwrap();
        fs::write(staging.path().join(".gitignore"), "shell/\n").unwrap();
        std::os::unix::fs::symlink("elsewhere", staging.path().join("link")).unwrap();
        (staging, root)
    }

    #[test]
    fn restoring_a_backup_keeps_symlinks_and_leaves_the_repo_behind() {
        let (staging, root) = staged_backup();
        fs::create_dir_all(root.path().join("categories")).unwrap();
        restore_backup(staging.path(), root.path()).unwrap();

        assert_eq!(
            fs::read_link(root.path().join("link")).unwrap(),
            PathBuf::from("elsewhere")
        );
        assert!(root.path().join(".gitignore").exists());
        assert!(root.path().join("categories/g.toml").exists());
        assert!(!root.path().join(".git").exists());
        assert!(!staging.path().join("categories").exists());
    }

    #[test]
    fn the_report_ends_on_the_fix_and_names_the_backup_without_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        write_status(
            &paths,
            SyncStatus::new("t1", OUTCOME_PUSH_FAILED, "git push failed: fatal: nope")
                .for_remote("https://user:secret@example.com/r.git"),
        );
        let report = take_failure(&paths, false).unwrap();
        assert!(
            report.lines().last().unwrap().starts_with("  Fix:"),
            "{report}"
        );
        assert!(
            report.contains("backup https://example.com/r.git"),
            "{report}"
        );
        assert!(!report.contains("secret"), "{report}");
        assert!(report.contains("Why:    nope"), "{report}");
    }

    fn fix_for(message: &str) -> (String, &'static str) {
        let status = SyncStatus::new("t1", OUTCOME_PULL_FAILED, message);
        why_and_fix(&status, &failure_text(OUTCOME_PULL_FAILED).unwrap())
    }

    #[test]
    fn a_commit_failure_is_never_blamed_on_the_remote() {
        let status = SyncStatus::new(
            "t1",
            OUTCOME_COMMIT_FAILED,
            "git commit failed: hook: Connection refused; Permission denied (publickey); Authentication failed",
        );
        let text = failure_text(OUTCOME_COMMIT_FAILED).unwrap();
        let (why, fix) = why_and_fix(&status, &text);
        assert_ne!(why, NETWORK_WHY);
        assert_ne!(why, SIGN_IN_WHY);
        assert_eq!(fix, text.fix);
    }

    #[test]
    fn common_causes_get_specific_advice() {
        assert_eq!(fix_for("fatal: Could not resolve host: x").1, NETWORK_FIX);
        assert_eq!(fix_for("could not read Username for 'x'").1, SIGN_IN_FIX);
        assert_eq!(
            fix_for("git@x: Permission denied (publickey).").1,
            SSH_KEY_FIX
        );
        assert_eq!(fix_for("parsing f: \n<<<<<<< HEAD\n").1, MARKERS_FIX);
        assert_eq!(
            fix_for("CONFLICT (content): in general.toml").1,
            CONFLICT_FIX
        );
    }

    #[test]
    fn a_status_written_before_remotes_were_recorded_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        fs::write(
            paths.sync_state_file(),
            "last_attempt = \"t1\"\noutcome = \"push-failed\"\nmessage = \"x\"\n",
        )
        .unwrap();
        assert_eq!(read_status(&paths).unwrap().remote, "");
    }

    #[test]
    fn an_attempt_is_described_by_its_age_and_its_time() {
        let now = OffsetDateTime::parse("2026-08-16T01:10:00Z", &Rfc3339).unwrap();
        let describe = |attempt| describe_attempt(attempt, now);
        assert_eq!(
            describe("2026-08-16T01:09:30Z"),
            "just now (2026-08-16 01:09 UTC)"
        );
        assert_eq!(
            describe("2026-08-16T01:09:00Z"),
            "1 minute ago (2026-08-16 01:09 UTC)"
        );
        assert_eq!(
            describe("2026-08-14T01:10:00Z"),
            "2 days ago (2026-08-14 01:10 UTC)"
        );
        assert_eq!(describe("not a time"), "not a time");
    }

    #[test]
    fn a_symlink_from_the_backup_replaces_a_regular_file() {
        let (staging, root) = staged_backup();
        fs::write(root.path().join("link"), "plain").unwrap();
        restore_backup(staging.path(), root.path()).unwrap();
        assert_eq!(
            fs::read_link(root.path().join("link")).unwrap(),
            PathBuf::from("elsewhere")
        );
    }

    #[test]
    fn restoring_replaces_a_symlink_at_the_target_instead_of_writing_through_it() {
        let (staging, root) = staged_backup();
        let victim = root.path().join("victim");
        fs::write(&victim, "precious").unwrap();
        std::os::unix::fs::symlink(&victim, root.path().join(".gitignore")).unwrap();

        restore_backup(staging.path(), root.path()).unwrap();

        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        let restored = fs::symlink_metadata(root.path().join(".gitignore")).unwrap();
        assert!(restored.file_type().is_file());
    }

    #[test]
    fn restoring_over_leftover_category_files_keeps_them_and_adds_the_backup() {
        let (staging, root) = staged_backup();
        fs::create_dir_all(root.path().join("categories")).unwrap();
        fs::write(root.path().join("categories/old.toml"), "y").unwrap();
        restore_backup(staging.path(), root.path()).unwrap();

        assert!(root.path().join("categories/old.toml").exists());
        assert!(root.path().join("categories/g.toml").exists());
    }

    #[test]
    fn the_wait_ends_when_a_new_attempt_lands() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_failure(&dir, "new", OUTCOME_PUSH_FAILED);
        assert!(wait_for_new_attempt(
            &paths,
            Some("old"),
            Duration::from_secs(5)
        ));
    }

    #[test]
    fn the_wait_gives_up_while_the_attempt_is_unchanged_or_locked() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths_with_failure(&dir, "same", OUTCOME_PUSH_FAILED);
        let limit = Duration::from_millis(120);
        assert!(!wait_for_new_attempt(&paths, Some("same"), limit));
        let _lock = SyncLock::acquire(&paths).unwrap().unwrap();
        assert!(!wait_for_new_attempt(&paths, Some("old"), limit));
    }

    #[test]
    fn a_successful_sync_gives_no_notice() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        write_status(
            &paths,
            SyncStatus::new("2026-08-16T00:00:00Z", OUTCOME_OK, ""),
        );
        assert!(take_failure_notice(&paths).is_none());
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
