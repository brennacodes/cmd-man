//! Offline tests for automatic background sync, using a local bare repository as
//! the "remote" so nothing ever touches a real GitHub account.

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
