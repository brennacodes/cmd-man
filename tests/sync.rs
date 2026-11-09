//! Offline tests for automatic background sync, using a local bare repository as
//! the "remote" so nothing ever touches a real GitHub account.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::sync::Once;
use std::time::{Duration, Instant};

use cmd_man::backup;
use cmd_man::config::Config;
use cmd_man::model::{Entry, EntryKey, Kind};
use cmd_man::paths::Paths;
use cmd_man::scopes::{Scope, ScopeRegistry};
use cmd_man::shell::Shell;
use cmd_man::store::Store;
use tempfile::TempDir;

static GIT_IDENTITY: Once = Once::new();

/// Give git a committer identity and an empty configuration via env, so these
/// tests neither need nor inherit the developer's global git settings. The
/// variables are also inherited by the binary and by the detached sync helper.
fn ensure_git_identity() {
    GIT_IDENTITY.call_once(|| unsafe {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_AUTHOR_NAME", "cmd-man test");
        std::env::set_var("GIT_AUTHOR_EMAIL", "test@example.com");
        std::env::set_var("GIT_COMMITTER_NAME", "cmd-man test");
        std::env::set_var("GIT_COMMITTER_EMAIL", "test@example.com");
    });
}

/// Run one sync cycle and assert the outcome it recorded, so a machine that
/// fails to seed the remote fails here rather than as a confusing assertion
/// about the machine that syncs next.
fn sync_expecting(paths: &Paths, config: &Config, timestamp: &str, outcome: &str) {
    backup::run_sync(paths, config, timestamp).unwrap();
    let status = backup::read_status(paths).unwrap();
    assert_eq!(status.outcome, outcome, "message: {}", status.message);
}

fn bare_remote() -> TempDir {
    let dir = TempDir::new().unwrap();
    let ok = Command::new("git")
        .args(["init", "--bare", "-b", "main"])
        .arg(dir.path())
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "failed to init bare remote");
    dir
}

fn config_with_remote(url: &str) -> Config {
    let mut config = Config::default();
    config.backup.remote_url = Some(url.to_string());
    config.backup.disable_gh = true;
    config.backup.disable_oauth = true;
    config
}

fn seed_store(paths: &Paths, name: &str, command: &str) {
    seed_store_in(paths, name, command, "general");
}

fn seed_store_in(paths: &Paths, name: &str, command: &str, category: &str) {
    paths.ensure_dirs().unwrap();
    let mut store = Store::default();
    let mut entry = Entry::new(name, Kind::Alias, command, "desc");
    entry.category = category.to_string();
    store.add(entry).unwrap();
    store.save(paths).unwrap();
}

/// Seed a store holding a single entry that belongs to the given scope id.
fn seed_scoped_store_in(paths: &Paths, name: &str, command: &str, scope: &str) {
    paths.ensure_dirs().unwrap();
    let mut store = Store::default();
    let mut entry = Entry::new(name, Kind::Alias, command, "desc");
    entry.scope = Some(scope.to_string());
    store.add(entry).unwrap();
    store.save(paths).unwrap();
}

fn seed_scope(paths: &Paths, id: &str, path: &str) {
    paths.ensure_dirs().unwrap();
    let mut registry = ScopeRegistry::load(paths).unwrap();
    registry.insert(Scope {
        id: id.to_string(),
        path: path.to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
    });
    registry.save(paths).unwrap();
}

fn clone_url_into(url: &str, dest: &Path) {
    let ok = Command::new("git")
        .arg("clone")
        .arg(url)
        .arg(dest)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "failed to clone {url}");
}

fn install_commit_msg_hook(paths: &Paths, script: &str) {
    install_hook(paths, "commit-msg", script);
}

fn install_hook(paths: &Paths, name: &str, script: &str) {
    let hooks = paths.root().join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join(name);
    std::fs::write(&hook, script).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
}

const REJECT_LOWERCASE_SUBJECT: &str =
    "#!/bin/sh\ncase \"$(head -n 1 \"$1\")\" in\n  [[:lower:]]*) exit 1 ;;\nesac\n";

fn notice_after_failed_sync(paths: &Paths, config: &Config, outcome: &str) -> String {
    sync_expecting(paths, config, "2026-08-16T00:01:00Z", outcome);
    backup::take_failure_notice(paths).expect("a failed sync should produce a notice")
}

#[test]
fn background_backup_commit_passes_a_capitalized_subject_hook() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();

    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "gitsw", "git switch");
    backup::prepare_repo(&paths).unwrap();
    install_commit_msg_hook(&paths, REJECT_LOWERCASE_SUBJECT);

    sync_expecting(
        &paths,
        &config_with_remote(url),
        "2026-08-16T00:00:00Z",
        "ok",
    );
}

#[test]
fn manual_backup_commit_passes_a_capitalized_subject_hook() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();

    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "gitsw", "git switch");
    backup::prepare_repo(&paths).unwrap();
    install_commit_msg_hook(&paths, REJECT_LOWERCASE_SUBJECT);

    let report =
        backup::run_backup(&paths, &config_with_remote(url), "2026-08-16T00:00:00Z").unwrap();
    assert!(report.committed);
}

#[test]
fn a_rejected_commit_is_not_reported_as_a_remote_problem() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();

    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "gitsw", "git switch");
    backup::prepare_repo(&paths).unwrap();
    install_commit_msg_hook(&paths, "#!/bin/sh\nexit 1\n");

    let notice = notice_after_failed_sync(&paths, &config_with_remote(url), "commit-failed");
    assert!(notice.contains("could not commit your changes"), "{notice}");
    assert!(!notice.contains("remote"), "{notice}");
    assert!(
        notice.contains(paths.sync_state_file().to_str().unwrap()),
        "{notice}"
    );
}

#[test]
fn an_unreachable_remote_is_reported_as_a_push_failure() {
    ensure_git_identity();
    let missing = TempDir::new().unwrap();
    let url = missing.path().join("absent.git");

    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "gitsw", "git switch");

    let notice = notice_after_failed_sync(
        &paths,
        &config_with_remote(url.to_str().unwrap()),
        "push-failed",
    );
    assert!(notice.contains("could not push to the remote"), "{notice}");
}

#[test]
fn a_conflicting_pull_is_reported_as_a_pull_failure() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();
    let config = config_with_remote(url);

    let seed = TempDir::new().unwrap();
    let seed_paths = Paths::with_root(seed.path().to_path_buf());
    seed_store(&seed_paths, "shared", "echo remote");
    sync_expecting(&seed_paths, &config, "2026-08-16T00:00:00Z", "ok");

    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "shared", "echo local");

    let notice = notice_after_failed_sync(&paths, &config, "pull-failed");
    assert!(
        notice.contains("could not pull from the remote"),
        "{notice}"
    );
    assert!(notice.contains("Why:    CONFLICT"), "{notice}");
}

#[test]
fn a_sync_asked_to_run_again_does_another_cycle() {
    ensure_git_identity();
    let bare = bare_remote();
    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "gitsw", "git switch");
    backup::prepare_repo(&paths).unwrap();
    install_hook(&paths, "post-commit", "#!/bin/sh\ntouch .sync-rerun\n");

    sync_expecting(
        &paths,
        &config_with_remote(bare.path().to_str().unwrap()),
        "2026-08-16T00:00:00Z",
        "ok",
    );
    assert!(!paths.sync_rerun_file().exists());
}

fn failing_add(home: &TempDir, paths: &Paths, name: &str) -> String {
    let output = binary_in(home, paths.root())
        .args(["add", name, "-c", "git switch", "--desc", "s", "--no-help"])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(output).unwrap()
}

#[test]
fn a_remote_that_never_answers_ends_in_a_reported_failure_and_a_free_lock() {
    ensure_git_identity();
    let stalled = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/x.git", stalled.local_addr().unwrap());

    let home = TempDir::new().unwrap();
    let store = home.path().join("store");
    let paths = Paths::with_root(store.clone());
    paths.ensure_dirs().unwrap();
    std::fs::write(
        paths.config_file(),
        format!("[backup]\nremote_url = \"{url}\"\ndisable_gh = true\ndisable_oauth = true\n"),
    )
    .unwrap();

    let output = binary_in(&home, &store)
        .env("CMD_MAN_SYNC_DEADLINE_SECS", "1")
        .args([
            "add",
            "gitsw",
            "-c",
            "git switch",
            "--desc",
            "s",
            "--no-help",
        ])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(
        stderr.contains("did not finish within 1 seconds"),
        "stderr: {stderr}"
    );

    let lock = std::fs::File::open(paths.sync_lock_file()).unwrap();
    assert!(lock.try_lock().is_ok(), "the helper still holds the lock");
}

#[test]
fn an_interrupted_first_clone_never_pushes_the_backup_as_deleted() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();
    let config = config_with_remote(url);

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_store(&paths_a, "kept", "echo kept");
    sync_expecting(&paths_a, &config, "2026-08-16T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    paths_b.ensure_dirs().unwrap();
    let blocker = b.path().join(".gitignore");
    std::fs::create_dir(&blocker).unwrap();
    backup::run_sync(&paths_b, &config, "2026-08-16T00:00:01Z").unwrap();
    std::fs::remove_dir(&blocker).unwrap();

    let status = backup::read_status(&paths_b).unwrap();
    assert_eq!(status.outcome, "failed", "message: {}", status.message);
    assert!(!b.path().join(".git").exists());
    assert!(Store::load(&paths_b).unwrap().entries().is_empty());

    sync_expecting(&paths_b, &config, "2026-08-16T00:00:02Z", "bootstrapped");
    let loaded = Store::load(&paths_b).unwrap();
    assert!(loaded.find(&EntryKey::global("kept")).is_some());
    let zsh = std::fs::read_to_string(paths_b.shell_file(Shell::Zsh)).unwrap();
    assert!(zsh.contains("echo kept"), "zsh was: {zsh}");

    sync_expecting(&paths_b, &config, "2026-08-16T00:00:03Z", "ok");
    let check = TempDir::new().unwrap();
    clone_url_into(url, check.path());
    assert!(check.path().join("categories/general.toml").exists());
}

#[test]
fn the_deadline_also_ends_what_the_helper_started() {
    ensure_git_identity();
    let bare = bare_remote();
    let home = TempDir::new().unwrap();
    let paths = store_rejecting_commits(&home, bare.path().to_str().unwrap());
    install_commit_msg_hook(
        &paths,
        "#!/bin/sh\necho $$ > .git/hook.pid\nexec sleep 30\n",
    );

    let output = binary_in(&home, paths.root())
        .env("CMD_MAN_SYNC_DEADLINE_SECS", "3")
        .args([
            "add",
            "gitsw",
            "-c",
            "git switch",
            "--desc",
            "s",
            "--no-help",
        ])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(stderr.contains("did not finish"), "stderr: {stderr}");

    let pid: libc::pid_t = std::fs::read_to_string(paths.root().join(".git/hook.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    // SAFETY: signal 0 only checks whether the process exists.
    while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    // SAFETY: as above.
    assert_ne!(
        unsafe { libc::kill(pid, 0) },
        0,
        "the hook outlived the helper"
    );
}

#[test]
fn a_clone_left_behind_by_an_interruption_is_removed() {
    ensure_git_identity();
    let bare = bare_remote();
    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "gitsw", "git switch");
    let leftover = paths.root().join(".sync-clone/.git");
    std::fs::create_dir_all(&leftover).unwrap();
    std::fs::write(leftover.join("config"), "x-access-token").unwrap();

    sync_expecting(
        &paths,
        &config_with_remote(bare.path().to_str().unwrap()),
        "2026-08-16T00:00:00Z",
        "ok",
    );
    assert!(!paths.root().join(".sync-clone").exists());
}

#[test]
fn a_first_clone_interrupted_after_restoring_finishes_on_the_next_sync() {
    ensure_git_identity();
    let bare = bare_remote();
    let config = config_with_remote(bare.path().to_str().unwrap());

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_store(&paths_a, "kept", "echo kept");
    sync_expecting(&paths_a, &config, "2026-08-16T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    paths_b.ensure_dirs().unwrap();
    std::fs::copy(
        paths_a.categories_dir().join("general.toml"),
        paths_b.categories_dir().join("general.toml"),
    )
    .unwrap();
    std::fs::write(b.path().join(".gitignore"), "shell/\n").unwrap();
    std::fs::write(paths_b.sync_bootstrap_pending_file(), "").unwrap();

    sync_expecting(&paths_b, &config, "2026-08-16T00:00:01Z", "bootstrapped");
    assert!(!paths_b.sync_bootstrap_pending_file().exists());
    sync_expecting(&paths_b, &config, "2026-08-16T00:00:02Z", "ok");
}

#[test]
fn resuming_a_first_clone_never_overwrites_an_entry_added_since() {
    ensure_git_identity();
    let bare = bare_remote();
    let config = config_with_remote(bare.path().to_str().unwrap());

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_store(&paths_a, "frombackup", "echo backup");
    sync_expecting(&paths_a, &config, "2026-08-16T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    seed_store(&paths_b, "localonly", "echo local");
    std::fs::write(paths_b.sync_bootstrap_pending_file(), "").unwrap();

    sync_expecting(&paths_b, &config, "2026-08-16T00:00:01Z", "failed");
    assert!(
        Store::load(&paths_b)
            .unwrap()
            .find(&EntryKey::global("localonly"))
            .is_some()
    );
    assert!(!paths_b.sync_bootstrap_pending_file().exists());
}

#[test]
fn a_marker_left_in_an_initialized_repo_cannot_trigger_a_later_clone() {
    ensure_git_identity();
    let bare = bare_remote();
    let config = config_with_remote(bare.path().to_str().unwrap());

    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "gitsw", "git switch");
    sync_expecting(&paths, &config, "2026-08-16T00:00:00Z", "ok");
    std::fs::write(paths.sync_bootstrap_pending_file(), "").unwrap();

    sync_expecting(&paths, &config, "2026-08-16T00:00:01Z", "ok");
    assert!(!paths.sync_bootstrap_pending_file().exists());
}

#[test]
fn an_interrupted_first_clone_gives_way_when_the_backup_is_now_empty() {
    ensure_git_identity();
    let bare = bare_remote();
    let config = config_with_remote(bare.path().to_str().unwrap());

    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "gitsw", "git switch");
    std::fs::write(paths.sync_bootstrap_pending_file(), "").unwrap();

    sync_expecting(&paths, &config, "2026-08-16T00:00:00Z", "ok");
    assert!(!paths.sync_bootstrap_pending_file().exists());
}

#[test]
fn an_interrupted_first_clone_waits_for_the_remote_instead_of_starting_a_new_history() {
    ensure_git_identity();
    let missing = TempDir::new().unwrap();
    let config = config_with_remote(missing.path().join("absent.git").to_str().unwrap());

    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "kept", "echo kept");
    std::fs::write(paths.sync_bootstrap_pending_file(), "").unwrap();

    sync_expecting(&paths, &config, "2026-08-16T00:00:00Z", "failed");
    assert!(!home.path().join(".git").exists());
}

#[test]
fn a_store_that_cannot_be_read_is_not_replaced_by_a_first_clone() {
    ensure_git_identity();
    let bare = bare_remote();
    let config = config_with_remote(bare.path().to_str().unwrap());

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_store(&paths_a, "kept", "echo kept");
    sync_expecting(&paths_a, &config, "2026-08-16T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    paths_b.ensure_dirs().unwrap();
    let mine = paths_b.categories_dir().join("general.toml");
    std::fs::write(&mine, "not [valid").unwrap();

    backup::run_sync(&paths_b, &config, "2026-08-16T00:00:01Z").unwrap();
    assert_ne!(
        backup::read_status(&paths_b).unwrap().outcome,
        "bootstrapped"
    );
}

#[test]
fn a_leftover_clone_is_removed_even_with_automatic_sync_off() {
    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    paths.ensure_dirs().unwrap();
    let leftover = paths.root().join(".sync-clone/.git");
    std::fs::create_dir_all(&leftover).unwrap();
    let mut config = Config::default();
    config.backup.auto_sync = false;

    backup::run_sync(&paths, &config, "2026-08-16T00:00:00Z").unwrap();
    assert!(!paths.root().join(".sync-clone").exists());
}

#[test]
fn a_slow_commit_hook_does_not_hold_up_saves() {
    ensure_git_identity();
    let bare = bare_remote();
    let config = config_with_remote(bare.path().to_str().unwrap());

    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "gitsw", "git switch");
    backup::prepare_repo(&paths).unwrap();
    install_commit_msg_hook(
        &paths,
        "#!/bin/sh\ntouch .git/in-hook\ni=0\nwhile [ ! -f .git/go ] && [ $i -lt 400 ]; do\n  sleep 0.05\n  i=$((i + 1))\ndone\n",
    );

    let sync_paths = paths.clone();
    let sync_config = config.clone();
    let sync = std::thread::spawn(move || {
        backup::run_sync(&sync_paths, &sync_config, "2026-08-16T00:00:00Z").unwrap();
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    while !paths.root().join(".git/in-hook").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(paths.root().join(".git/in-hook").exists());
    let acquired = cmd_man::store::StoreLock::acquire(&paths);
    std::fs::write(paths.root().join(".git/go"), "").unwrap();
    sync.join().unwrap();

    assert!(acquired.is_ok(), "the commit held the store lock");
    assert_eq!(backup::read_status(&paths).unwrap().outcome, "ok");
}

#[test]
fn machines_with_different_ignore_lists_still_merge() {
    ensure_git_identity();
    let bare = bare_remote();
    let config = config_with_remote(bare.path().to_str().unwrap());

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_store_in(&paths_a, "fromolder", "echo older", "work");
    backup::prepare_repo(&paths_a).unwrap();
    std::fs::write(a.path().join(".gitignore"), "shell/\nonly-on-a\n").unwrap();
    let mut older = config.clone();
    older.backup.auto_sync = true;
    sync_expecting(&paths_a, &older, "2026-08-16T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    seed_store_in(&paths_b, "mine", "echo mine", "general");

    sync_expecting(&paths_b, &config, "2026-08-16T00:00:01Z", "ok");
    let merged = std::fs::read_to_string(b.path().join(".gitignore")).unwrap();
    assert!(merged.contains("only-on-a"), "{merged}");
    assert!(merged.contains(".sync-rerun"), "{merged}");
}

#[test]
fn a_merge_commit_runs_outside_the_lock_and_still_lands() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();
    let config = config_with_remote(url);

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_store_in(&paths_a, "ina", "echo a", "alpha");
    sync_expecting(&paths_a, &config, "2026-08-16T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    seed_store_in(&paths_b, "inb", "echo b", "beta");
    sync_expecting(&paths_b, &config, "2026-08-16T00:00:01Z", "ok");

    assert!(!b.path().join(".git/MERGE_HEAD").exists());
    let check = TempDir::new().unwrap();
    clone_url_into(url, check.path());
    assert!(check.path().join("categories/alpha.toml").exists());
    assert!(check.path().join("categories/beta.toml").exists());
}

#[test]
fn a_merge_that_would_break_the_store_is_backed_out_and_explained() {
    ensure_git_identity();
    let bare = bare_remote();
    let config = config_with_remote(bare.path().to_str().unwrap());

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_store_in(&paths_a, "gs", "echo from-a", "git");
    sync_expecting(&paths_a, &config, "2026-08-16T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    seed_store_in(&paths_b, "gs", "echo from-b", "tools");

    let notice = notice_after_failed_sync(&paths_b, &config, "pull-failed");
    assert!(notice.contains("duplicate entry 'gs'"), "{notice}");
    assert!(notice.contains("Rename or remove it here"), "{notice}");
    assert!(Store::load(&paths_b).is_ok());
    assert!(!b.path().join(".git/MERGE_HEAD").exists());
}

#[test]
fn every_change_reports_a_failure_that_keeps_happening() {
    ensure_git_identity();
    let bare = bare_remote();
    let home = TempDir::new().unwrap();
    let paths = store_rejecting_commits(&home, bare.path().to_str().unwrap());

    for name in ["first", "second"] {
        let stderr = failing_add(&home, &paths, name);
        assert!(
            stderr.contains("could not commit your changes"),
            "{name} stderr: {stderr}"
        );
    }
}

#[test]
fn a_change_does_not_wait_when_automatic_sync_is_off() {
    ensure_git_identity();
    let bare = bare_remote();
    let home = TempDir::new().unwrap();
    let paths = store_rejecting_commits(&home, bare.path().to_str().unwrap());
    let url = bare.path().to_str().unwrap();
    std::fs::write(
        paths.config_file(),
        format!(
            "[backup]\nremote_url = \"{url}\"\ndisable_gh = true\ndisable_oauth = true\nauto_sync = false\n"
        ),
    )
    .unwrap();

    let started = Instant::now();
    let stderr = failing_add(&home, &paths, "quick");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!stderr.contains("backup sync"), "stderr: {stderr}");
}

fn binary_in(home: &TempDir, store: &Path) -> assert_cmd::Command {
    let mut c = assert_cmd::Command::cargo_bin("cmd-man").unwrap();
    c.env("HOME", home.path())
        .env("CMD_MAN_HOME", store)
        .env("SHELL", "/bin/zsh");
    c
}

fn store_rejecting_commits(home: &TempDir, url: &str) -> Paths {
    let store = home.path().join("store");
    let paths = Paths::with_root(store.clone());
    paths.ensure_dirs().unwrap();
    std::fs::write(
        store.join("config.toml"),
        format!("[backup]\nremote_url = \"{url}\"\ndisable_gh = true\ndisable_oauth = true\n"),
    )
    .unwrap();
    backup::prepare_repo(&paths).unwrap();
    install_commit_msg_hook(&paths, "#!/bin/sh\nexit 1\n");
    paths
}

#[test]
fn a_cli_change_reports_its_sync_failure_in_the_same_command() {
    ensure_git_identity();
    let bare = bare_remote();
    let home = TempDir::new().unwrap();
    let paths = store_rejecting_commits(&home, bare.path().to_str().unwrap());

    let output = binary_in(&home, paths.root())
        .args([
            "add",
            "gitsw",
            "-c",
            "git switch",
            "--desc",
            "s",
            "--no-help",
        ])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(
        stderr.contains("could not commit your changes"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("Effect:"), "stderr: {stderr}");
    assert!(stderr.contains("`cmd-man sync`"), "stderr: {stderr}");
}

#[test]
fn an_unseen_failure_is_reported_once_on_the_next_read_only_command() {
    ensure_git_identity();
    let bare = bare_remote();
    let home = TempDir::new().unwrap();
    let paths = store_rejecting_commits(&home, bare.path().to_str().unwrap());
    seed_store(&paths, "gitsw", "git switch");
    let config = Config::load(&paths).unwrap();
    sync_expecting(&paths, &config, "2026-08-16T00:00:00Z", "commit-failed");

    let list = || {
        let output = binary_in(&home, paths.root())
            .env("CMD_MAN_DISABLE_SYNC", "1")
            .args(["list", "--all"])
            .assert()
            .success()
            .get_output()
            .stderr
            .clone();
        String::from_utf8(output).unwrap()
    };
    assert!(list().contains("could not commit your changes"));
    assert!(!list().contains("could not commit your changes"));
}

#[test]
fn sync_now_reports_a_failure_and_exits_nonzero() {
    ensure_git_identity();
    let bare = bare_remote();
    let home = TempDir::new().unwrap();
    let paths = store_rejecting_commits(&home, bare.path().to_str().unwrap());
    seed_store(&paths, "gitsw", "git switch");

    let output = binary_in(&home, paths.root())
        .env("CMD_MAN_DISABLE_SYNC", "1")
        .arg("sync")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let stderr = String::from_utf8(output).unwrap();
    assert!(
        stderr.contains("could not commit your changes"),
        "stderr: {stderr}"
    );
}

#[test]
fn sync_now_pushes_even_with_automatic_sync_off() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();
    let home = TempDir::new().unwrap();
    let store = home.path().join("store");
    let paths = Paths::with_root(store.clone());
    seed_store(&paths, "gitsw", "git switch");
    std::fs::write(
        store.join("config.toml"),
        format!(
            "[backup]\nremote_url = \"{url}\"\ndisable_gh = true\ndisable_oauth = true\nauto_sync = false\n"
        ),
    )
    .unwrap();

    binary_in(&home, &store)
        .env("CMD_MAN_DISABLE_SYNC", "1")
        .arg("sync")
        .assert()
        .success()
        .stdout(predicates::str::contains("Backup is in sync."));

    let check = TempDir::new().unwrap();
    clone_url_into(url, check.path());
    assert!(check.path().join("categories/general.toml").exists());
}

fn block_zsh_file(paths: &Paths) {
    let zsh = paths.shell_file(Shell::Zsh);
    if zsh.exists() {
        std::fs::remove_file(&zsh).unwrap();
    }
    std::fs::create_dir_all(&zsh).unwrap();
}

fn unblock_zsh_file(paths: &Paths) {
    std::fs::remove_dir(paths.shell_file(Shell::Zsh)).unwrap();
}

#[test]
fn a_failed_regeneration_after_a_pull_is_retried_and_never_pushed() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();
    let config = config_with_remote(url);

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_store(&paths_a, "first", "echo first");
    sync_expecting(&paths_a, &config, "2026-08-16T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    paths_b.ensure_dirs().unwrap();
    sync_expecting(&paths_b, &config, "2026-08-16T00:00:01Z", "bootstrapped");

    seed_store(&paths_a, "second", "echo second");
    sync_expecting(&paths_a, &config, "2026-08-16T00:00:02Z", "ok");

    block_zsh_file(&paths_b);
    sync_expecting(&paths_b, &config, "2026-08-16T00:00:03Z", "failed");
    assert!(paths_b.sync_regen_pending_file().exists());

    unblock_zsh_file(&paths_b);
    sync_expecting(&paths_b, &config, "2026-08-16T00:00:04Z", "ok");
    let zsh = std::fs::read_to_string(paths_b.shell_file(Shell::Zsh)).unwrap();
    assert!(zsh.contains("echo second"), "zsh was: {zsh}");
    assert!(!paths_b.sync_regen_pending_file().exists());

    let check = TempDir::new().unwrap();
    clone_url_into(url, check.path());
    assert!(!check.path().join(".sync-regen-pending").exists());
}

#[test]
fn a_failed_regeneration_after_a_bootstrap_is_retried() {
    ensure_git_identity();
    let bare = bare_remote();
    let config = config_with_remote(bare.path().to_str().unwrap());

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_store(&paths_a, "first", "echo first");
    sync_expecting(&paths_a, &config, "2026-08-16T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    paths_b.ensure_dirs().unwrap();
    block_zsh_file(&paths_b);
    sync_expecting(&paths_b, &config, "2026-08-16T00:00:01Z", "failed");

    unblock_zsh_file(&paths_b);
    sync_expecting(&paths_b, &config, "2026-08-16T00:00:02Z", "ok");
    let zsh = std::fs::read_to_string(paths_b.shell_file(Shell::Zsh)).unwrap();
    assert!(zsh.contains("echo first"), "zsh was: {zsh}");
}

#[test]
fn sync_pushes_local_entries_to_remote() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();

    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "gitsw", "git switch");

    sync_expecting(
        &paths,
        &config_with_remote(url),
        "2026-08-16T00:00:00Z",
        "ok",
    );

    let check = TempDir::new().unwrap();
    clone_url_into(url, check.path());
    let toml = std::fs::read_to_string(check.path().join("categories/general.toml")).unwrap();
    assert!(
        toml.contains("gitsw"),
        "entry did not travel to remote: {toml}"
    );
}

#[test]
fn sync_bootstraps_empty_store_from_remote() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();

    // Seed the remote from a source machine.
    let seed = TempDir::new().unwrap();
    let seed_paths = Paths::with_root(seed.path().to_path_buf());
    seed_store(&seed_paths, "seeded", "echo seeded");
    sync_expecting(
        &seed_paths,
        &config_with_remote(url),
        "2026-08-16T00:00:00Z",
        "ok",
    );

    // Fresh machine: empty home pointed at the populated remote.
    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    sync_expecting(
        &paths,
        &config_with_remote(url),
        "2026-08-16T00:01:00Z",
        "bootstrapped",
    );

    let loaded = Store::load(&paths).unwrap();
    assert!(
        loaded.find(&EntryKey::global("seeded")).is_some(),
        "bootstrap did not import entry"
    );

    // Shell files are regenerated from the pulled entries.
    let zsh = std::fs::read_to_string(paths.shell_file(Shell::Zsh)).unwrap();
    assert!(zsh.contains("seeded"), "shell file not regenerated: {zsh}");
}

#[test]
fn sync_does_not_clobber_local_only_entries() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();

    // Remote holds an unrelated history.
    let seed = TempDir::new().unwrap();
    let seed_paths = Paths::with_root(seed.path().to_path_buf());
    seed_store(&seed_paths, "remoteonly", "echo remote");
    sync_expecting(
        &seed_paths,
        &config_with_remote(url),
        "2026-08-16T00:00:00Z",
        "ok",
    );

    // Local machine already has its own entry and no git repo yet.
    let home = TempDir::new().unwrap();
    let paths = Paths::with_root(home.path().to_path_buf());
    seed_store(&paths, "localonly", "echo local");

    backup::run_sync(&paths, &config_with_remote(url), "2026-08-16T00:01:00Z").unwrap();

    // The local entry survives regardless of the sync outcome.
    let loaded = Store::load(&paths).unwrap();
    assert!(
        loaded.find(&EntryKey::global("localonly")).is_some(),
        "local-only entry must not be clobbered by a populated remote"
    );
}

#[test]
fn sync_merges_disjoint_categories_across_machines() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();

    // Machine A backs up an entry in its own category.
    let machine_a = TempDir::new().unwrap();
    let a_paths = Paths::with_root(machine_a.path().to_path_buf());
    seed_store_in(&a_paths, "gitsw", "git switch", "git");
    sync_expecting(
        &a_paths,
        &config_with_remote(url),
        "2026-08-16T00:00:00Z",
        "ok",
    );

    // Machine B has a different entry and no repo yet, then syncs.
    let machine_b = TempDir::new().unwrap();
    let b_paths = Paths::with_root(machine_b.path().to_path_buf());
    seed_store_in(&b_paths, "cls", "clear", "shell");
    // Disjoint histories reconcile into the union of both machines' entries.
    sync_expecting(
        &b_paths,
        &config_with_remote(url),
        "2026-08-16T00:01:00Z",
        "ok",
    );

    let loaded = Store::load(&b_paths).unwrap();
    assert!(
        loaded.find(&EntryKey::global("cls")).is_some(),
        "local entry lost"
    );
    assert!(
        loaded.find(&EntryKey::global("gitsw")).is_some(),
        "remote entry did not merge in"
    );
}

#[test]
fn add_triggers_background_push() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();

    let home = TempDir::new().unwrap();
    let store = home.path().join("store");
    std::fs::create_dir_all(store.join("categories")).unwrap();
    let config = format!(
        "[backup]\nremote_url = \"{url}\"\ndisable_gh = true\ndisable_oauth = true\nauto_sync = true\n"
    );
    std::fs::write(store.join("config.toml"), config).unwrap();

    // Sync is left enabled for this invocation so the detached helper runs.
    let mut c = assert_cmd::Command::cargo_bin("cmd-man").unwrap();
    c.env("HOME", home.path())
        .env("CMD_MAN_HOME", &store)
        .env("SHELL", "/bin/zsh")
        .args(["add", "gitsw", "-c", "git switch", "--desc", "switch"])
        .assert()
        .success();

    // The push happens in a detached process; poll the remote for the commit.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut pushed = false;
    while Instant::now() < deadline {
        let out = Command::new("git")
            .args(["ls-remote", url])
            .output()
            .unwrap();
        if !out.stdout.is_empty() {
            pushed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        pushed,
        "background sync did not push to the remote within the timeout"
    );
}

#[test]
fn scopes_file_round_trips_through_the_remote() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();
    let config = config_with_remote(url);

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_store(&paths_a, "ll", "ls -la");
    seed_scope(&paths_a, "api", "$HOME/work/api");
    sync_expecting(&paths_a, &config, "2026-08-29T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    paths_b.ensure_dirs().unwrap();
    sync_expecting(&paths_b, &config, "2026-08-29T00:00:01Z", "bootstrapped");

    let pulled = ScopeRegistry::load(&paths_b).unwrap();
    let scope = pulled
        .find("api")
        .expect("scope registry did not travel to the other machine");
    assert_eq!(scope.path, "$HOME/work/api");
}

#[test]
fn a_pull_regenerates_scope_blocks() {
    ensure_git_identity();
    let bare = bare_remote();
    let url = bare.path().to_str().unwrap();
    let config = config_with_remote(url);

    let a = TempDir::new().unwrap();
    let paths_a = Paths::with_root(a.path().to_path_buf());
    seed_scoped_store_in(&paths_a, "ll", "ls -lah", "api");
    seed_scope(&paths_a, "api", "/tmp/cmd-man-scope-api");
    sync_expecting(&paths_a, &config, "2026-08-29T00:00:00Z", "ok");

    let b = TempDir::new().unwrap();
    let paths_b = Paths::with_root(b.path().to_path_buf());
    paths_b.ensure_dirs().unwrap();
    sync_expecting(&paths_b, &config, "2026-08-29T00:00:01Z", "bootstrapped");

    let zsh = std::fs::read_to_string(paths_b.shell_file(Shell::Zsh)).unwrap();
    assert!(
        zsh.contains("_cmdman_dir_1='/tmp/cmd-man-scope-api'"),
        "zsh was: {zsh}"
    );
    assert!(zsh.contains("_cmdman_on_1"), "zsh was: {zsh}");
    assert!(zsh.contains("ls -lah"), "zsh was: {zsh}");
}
