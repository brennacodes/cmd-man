//! End-to-end tests driving the real `cmd-man` binary against a temp store.

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// A binary invocation isolated to a temporary home + store.
fn cmd(home: &TempDir) -> Command {
    let mut c = Command::cargo_bin("cmd-man").unwrap();
    c.env("HOME", home.path())
        .env("CMD_MAN_HOME", home.path().join("store"))
        // Ensure a predictable shell for import/hint logic.
        .env("SHELL", "/bin/zsh")
        // Never let the background sync run against a real remote from tests.
        .env("CMD_MAN_DISABLE_SYNC", "1");
    c
}

/// [`cmd`] run from `dir`, with `PWD` exported the way a shell would so the
/// binary sees the same logical directory it is standing in.
fn cmd_in(home: &TempDir, dir: &std::path::Path) -> Command {
    let mut c = cmd(home);
    c.current_dir(dir).env("PWD", dir);
    c
}

fn store_dir(home: &TempDir) -> std::path::PathBuf {
    home.path().join("store")
}

/// True when zsh is installed. Probing by spawning avoids depending on any
/// particular lookup tool being present.
fn have_zsh() -> bool {
    std::process::Command::new("zsh")
        .arg("-c")
        .arg("exit 0")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Every entry the store holds, read straight off disk so assertions do not
/// depend on CLI output formatting.
fn app_entries(home: &TempDir) -> Vec<cmd_man::model::Entry> {
    let paths = cmd_man::paths::Paths::with_root(store_dir(home));
    cmd_man::store::Store::load(&paths)
        .unwrap()
        .entries()
        .to_vec()
}

#[test]
fn add_generates_live_alias_and_lists_it() {
    let home = TempDir::new().unwrap();

    cmd(&home)
        .args([
            "add",
            "gitsw",
            "--command",
            "git switch",
            "--desc",
            "switch branches",
            "--category",
            "git",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Added 'gitsw'"));

    // The generated zsh file contains a real alias definition.
    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(zsh.contains("alias gitsw='git switch'"), "zsh was: {zsh}");

    // The category file exists with a slugged name.
    assert!(store_dir(&home).join("categories/git.toml").exists());

    cmd(&home)
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gitsw"));
}

#[test]
fn search_finds_by_fuzzy_query() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args(["add", "gitsw", "-c", "git switch", "--desc", "switch"])
        .assert()
        .success();
    cmd(&home)
        .args(["add", "clr", "-c", "clear", "--desc", "clear screen"])
        .assert()
        .success();

    cmd(&home)
        .args(["search", "switch"])
        .assert()
        .success()
        .stdout(predicate::str::contains("gitsw"))
        .stdout(predicate::str::contains("clr").not());
}

#[test]
fn duplicate_name_is_rejected() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args(["add", "dup", "-c", "echo one", "--desc", "one"])
        .assert()
        .success();
    cmd(&home)
        .args(["add", "dup", "-c", "echo two", "--desc", "two"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn remove_updates_generated_file() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args(["add", "gone", "-c", "echo bye", "--desc", "bye"])
        .assert()
        .success();
    cmd(&home).args(["rm", "gone"]).assert().success();

    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(!zsh.contains("gone"));
}

#[test]
fn capture_records_sanitized_output_for_safe_command() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args([
            "add",
            "greet",
            "-c",
            "echo hello-capture",
            "--desc",
            "greet",
        ])
        .assert()
        .success();

    cmd(&home)
        .args(["capture", "greet"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Saved example output"));

    let toml = std::fs::read_to_string(store_dir(&home).join("categories/general.toml")).unwrap();
    assert!(
        toml.contains("hello-capture"),
        "expected captured output in store, got: {toml}"
    );
}

#[test]
fn destructive_capture_requires_confirmation_when_not_a_tty() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args(["add", "nuke", "-c", "rm -rf build", "--desc", "danger"])
        .assert()
        .success();

    // Non-interactive stdin means it should refuse without --yes.
    cmd(&home)
        .args(["capture", "nuke"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("needs confirmation"));
}

#[test]
fn init_writes_managed_block_to_zshrc() {
    let home = TempDir::new().unwrap();
    cmd(&home).args(["init"]).assert().success();

    let zshrc = std::fs::read_to_string(home.path().join(".zshrc")).unwrap();
    assert!(zshrc.contains("# >>> cmd-man >>>"));
    assert!(zshrc.contains("cmd-man.zsh"));

    // Running init again is idempotent (still one managed block).
    cmd(&home).args(["init"]).assert().success();
    let zshrc = std::fs::read_to_string(home.path().join(".zshrc")).unwrap();
    assert_eq!(zshrc.matches("# >>> cmd-man >>>").count(), 1);
}

#[test]
fn edit_updates_entry_and_regenerates() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args(["add", "gitsw", "-c", "git switch", "--desc", "switch"])
        .assert()
        .success();

    cmd(&home)
        .args([
            "edit",
            "gitsw",
            "--command",
            "git switch --create",
            "--desc",
            "switch or create",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Updated 'gitsw'"));

    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(zsh.contains("git switch --create"), "zsh was: {zsh}");
}

#[test]
fn reload_regenerates_and_prints_hint() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args(["add", "c", "-c", "clear", "--desc", "clear screen"])
        .assert()
        .success();

    // Remove the generated file, then prove reload recreates it.
    std::fs::remove_file(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    cmd(&home)
        .args(["reload"])
        .assert()
        .success()
        .stdout(predicate::str::contains("source"));
    assert!(store_dir(&home).join("shell/cmd-man.zsh").exists());
}

#[test]
fn function_kind_generates_function_definition() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args([
            "add",
            "hi",
            "--kind",
            "function",
            "-c",
            "echo hi there",
            "--desc",
            "say hi",
        ])
        .assert()
        .success();

    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(zsh.contains("function hi {"), "zsh was: {zsh}");
}

#[test]
fn an_entry_in_an_unknown_scope_warns_and_is_not_rendered() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args(["add", "ok", "-c", "echo ok", "--desc", "d"])
        .assert()
        .success();

    let file = store_dir(&home).join("categories/general.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
        &file,
        text.replace("name = \"ok\"", "name = \"ok\"\nscope = \"ghost\""),
    )
    .unwrap();

    cmd(&home)
        .args(["list"])
        .assert()
        .success()
        .stderr(predicate::str::contains("ghost"));

    cmd(&home).args(["reload"]).assert().success();
    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(!zsh.contains("echo ok"), "zsh was: {zsh}");
}

#[test]
fn add_here_scopes_the_entry_and_reports_it() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();

    cmd_in(&home, &api)
        .args([
            "add",
            "ll",
            "-c",
            "ls -lah",
            "--desc",
            "long list",
            "--here",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Added 'll' scoped to api ($HOME/work/api)",
        ));

    let scopes = std::fs::read_to_string(store_dir(&home).join("scopes.toml")).unwrap();
    assert!(scopes.contains("id = \"api\""), "scopes were: {scopes}");
    // Stored $HOME-relative so the registry survives a move to another machine.
    assert!(
        scopes.contains("path = \"$HOME/work/api\""),
        "scopes were: {scopes}"
    );
}

#[test]
fn add_here_with_capture_records_output_on_the_scoped_entry() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();

    cmd(&home)
        .args(["add", "gg", "-c", "echo global-one", "--desc", "g"])
        .assert()
        .success();
    cmd_in(&home, &api)
        .args([
            "add",
            "gg",
            "-c",
            "echo scoped-one",
            "--desc",
            "s",
            "--here",
            "--capture",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Saved example output for 'gg (api)'",
        ));

    let entries =
        std::fs::read_to_string(store_dir(&home).join("categories/general.toml")).unwrap();
    assert!(
        entries.contains("example_output = \"scoped-one\""),
        "entries were: {entries}"
    );
    assert!(
        !entries.contains("example_output = \"global-one\""),
        "the global entry was mutated: {entries}"
    );
}

#[test]
fn add_here_follows_a_symlinked_working_directory() {
    let home = TempDir::new().unwrap();
    let real = home.path().join("actual/proj");
    std::fs::create_dir_all(&real).unwrap();
    let link = home.path().join("proj");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    cmd_in(&home, &link)
        .args(["add", "dep", "-c", "make deploy", "--desc", "d", "--here"])
        .assert()
        .success();

    let scopes = std::fs::read_to_string(store_dir(&home).join("scopes.toml")).unwrap();
    assert!(
        scopes.contains("path = \"$HOME/proj\""),
        "scopes were: {scopes}"
    );
    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(
        zsh.contains(&format!("'{}'", link.display())),
        "zsh was: {zsh}"
    );
}

#[test]
fn the_same_name_can_exist_globally_and_in_a_scope() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();

    cmd(&home)
        .args(["add", "ll", "-c", "ls -la", "--desc", "g"])
        .assert()
        .success();
    cmd_in(&home, &api)
        .args(["add", "ll", "-c", "ls -lah", "--desc", "s", "--here"])
        .assert()
        .success();
    cmd_in(&home, &api)
        .args(["add", "ll", "-c", "ls -lahG", "--desc", "s2", "--here"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn add_with_no_flag_stays_global_inside_a_git_repo() {
    let home = TempDir::new().unwrap();
    let proj = home.path().join("proj");
    std::fs::create_dir_all(proj.join(".git")).unwrap();

    cmd_in(&home, &proj)
        .args(["add", "ll", "-c", "ls -la", "--desc", "d"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Added 'll'"))
        .stdout(predicate::str::contains("scoped to").not());
}

#[test]
fn repo_mode_scopes_to_the_repo_root() {
    let home = TempDir::new().unwrap();
    std::fs::create_dir_all(store_dir(&home)).unwrap();
    std::fs::write(
        store_dir(&home).join("config.toml"),
        "[scope]\ndefault = \"repo\"\n",
    )
    .unwrap();
    let proj = home.path().join("proj");
    let deep = proj.join("src");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::create_dir_all(proj.join(".git")).unwrap();

    cmd_in(&home, &deep)
        .args(["add", "ll", "-c", "ls -la", "--desc", "d"])
        .assert()
        .success()
        .stdout(predicate::str::contains("scoped to proj"));
}

#[test]
fn repo_mode_does_not_scope_to_home() {
    let home = TempDir::new().unwrap();
    std::fs::create_dir_all(store_dir(&home)).unwrap();
    std::fs::write(
        store_dir(&home).join("config.toml"),
        "[scope]\ndefault = \"repo\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.path().join(".git")).unwrap();

    cmd_in(&home, home.path())
        .args(["add", "ll", "-c", "ls -la", "--desc", "d"])
        .assert()
        .success()
        .stdout(predicate::str::contains("scoped to").not());
}

#[test]
fn new_prompts_for_placement() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();

    cmd_in(&home, &api)
        .args(["new"])
        .write_stdin("dep\nalias\necho deploy\ndeploy it\ngeneral\n\n\nhere\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("scoped to api"));
}

#[test]
fn new_reprompts_for_an_unknown_scope_instead_of_discarding_the_answers() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();

    cmd_in(&home, &api)
        .args(["add", "ll", "-c", "ls -lah", "--desc", "d", "--here"])
        .assert()
        .success();

    cmd_in(&home, &api)
        .args(["new"])
        .write_stdin("dep\nalias\necho deploy\ndeploy it\ngeneral\n\n\napx\napi\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("unknown scope 'apx'"))
        .stdout(predicate::str::contains("Known scopes: api"))
        .stdout(predicate::str::contains("Added 'dep' scoped to api"));
}

#[test]
fn a_directory_named_here_never_takes_the_here_keyword() {
    let home = TempDir::new().unwrap();
    let here = home.path().join("here");
    let other = home.path().join("other");
    std::fs::create_dir_all(&here).unwrap();
    std::fs::create_dir_all(&other).unwrap();

    cmd_in(&home, &here)
        .args(["add", "ll", "-c", "ls -lah", "--desc", "d", "--here"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Added 'll' scoped to here-2 ($HOME/here)",
        ));

    // Answering the Scope prompt with `here` still means "this directory".
    cmd_in(&home, &other)
        .args(["new"])
        .write_stdin("dep\nalias\necho deploy\ndeploy it\ngeneral\n\n\nhere\n")
        .assert()
        .success()
        .stdout(predicate::str::contains("scoped to other"));

    // The directory scope keeps its own id and stays addressable by it.
    cmd_in(&home, &other)
        .args([
            "add", "gg", "-c", "echo gg", "--desc", "d", "--scope", "here-2",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("scoped to here-2"));
}

#[test]
fn rm_inside_a_scope_removes_the_scoped_entry_and_says_so() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();
    cmd(&home)
        .args(["add", "dep", "-c", "echo global", "--desc", "g"])
        .assert()
        .success();
    cmd_in(&home, &api)
        .args(["add", "dep", "-c", "echo scoped", "--desc", "s", "--here"])
        .assert()
        .success();

    cmd_in(&home, &api)
        .args(["rm", "dep"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dep (api)"));

    // The global entry of the same name is untouched.
    cmd(&home)
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo global"))
        .stdout(predicate::str::contains("echo scoped").not());
}

#[test]
fn in_global_selects_the_global_entry_from_inside_a_scope() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();
    cmd(&home)
        .args(["add", "dep", "-c", "echo global", "--desc", "g"])
        .assert()
        .success();
    cmd_in(&home, &api)
        .args(["add", "dep", "-c", "echo scoped", "--desc", "s", "--here"])
        .assert()
        .success();

    cmd_in(&home, &api)
        .args([
            "edit",
            "dep",
            "--in",
            "global",
            "--desc",
            "edited",
            "--no-help",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Updated 'dep'"));

    let toml = std::fs::read_to_string(store_dir(&home).join("categories/general.toml")).unwrap();
    assert!(toml.contains("edited"), "store was: {toml}");
}

#[test]
fn current_scope_fails_outside_a_scope() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args(["add", "dep", "-c", "echo g", "--desc", "g"])
        .assert()
        .success();
    cmd_in(&home, home.path())
        .args(["rm", "dep", "--current-scope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no scope"));
}

#[test]
fn edit_move_here_scopes_an_existing_global() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();
    cmd(&home)
        .args(["add", "dep", "-c", "echo dep", "--desc", "d"])
        .assert()
        .success();

    cmd_in(&home, &api)
        .args(["edit", "dep", "--move-here", "--no-help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("dep (api)"));

    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(zsh.contains("_cmdman_on_1"), "zsh was: {zsh}");
}

#[test]
fn edit_move_to_an_unknown_scope_errors() {
    let home = TempDir::new().unwrap();
    cmd(&home)
        .args(["add", "dep", "-c", "echo dep", "--desc", "d"])
        .assert()
        .success();

    cmd(&home)
        .args(["edit", "dep", "--move-to", "nope", "--no-help"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unknown scope 'nope'"));

    let scopes_file = store_dir(&home).join("scopes.toml");
    assert!(
        !scopes_file.exists() || {
            let s = std::fs::read_to_string(&scopes_file).unwrap();
            !s.contains("nope")
        }
    );
}

#[test]
fn list_defaults_to_entries_active_in_this_directory() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();

    cmd_in(&home, home.path())
        .args(["add", "globalone", "-c", "echo global-one", "--desc", "g"])
        .assert()
        .success();
    cmd_in(&home, &api)
        .args([
            "add",
            "scopedone",
            "-c",
            "echo scoped-one",
            "--desc",
            "s",
            "--here",
        ])
        .assert()
        .success();

    cmd_in(&home, home.path())
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo global-one"))
        .stdout(predicate::str::contains("echo scoped-one").not());

    cmd_in(&home, &api)
        .args(["list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("scopedone (api)"))
        .stdout(predicate::str::contains("echo global-one"));

    cmd_in(&home, home.path())
        .args(["list", "--all"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo scoped-one"))
        .stdout(predicate::str::contains("echo global-one"));

    cmd_in(&home, home.path())
        .args(["list", "--in", "api"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo scoped-one"))
        .stdout(predicate::str::contains("echo global-one").not());

    cmd_in(&home, &api)
        .args(["list", "--in", "global"])
        .assert()
        .success()
        .stdout(predicate::str::contains("echo global-one"))
        .stdout(predicate::str::contains("echo scoped-one").not());
}

#[test]
fn search_finds_entries_from_every_scope() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();

    cmd_in(&home, &api)
        .args([
            "add",
            "deployer",
            "-c",
            "echo deploy",
            "--desc",
            "d",
            "--here",
        ])
        .assert()
        .success();

    cmd_in(&home, home.path())
        .args(["search", "deploy"])
        .assert()
        .success()
        .stdout(predicate::str::contains("deployer (api)"));
}

#[test]
fn scope_list_reports_scopes() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();
    cmd_in(&home, &api)
        .args(["add", "dep", "-c", "echo dep", "--desc", "d", "--here"])
        .assert()
        .success();

    cmd_in(&home, &api)
        .args(["scope", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("api"))
        .stdout(predicate::str::contains("1"))
        .stdout(predicate::str::contains("(active)"));

    cmd_in(&home, home.path())
        .args(["scope", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(active)").not());
}

#[test]
fn scope_list_explains_a_scope_the_shell_cannot_match_from_here() {
    let home = TempDir::new().unwrap();
    let real = home.path().join("actual/proj");
    std::fs::create_dir_all(&real).unwrap();
    let link = home.path().join("proj");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    cmd(&home)
        .args(["scope", "relocate", "proj", &real.display().to_string()])
        .assert()
        .success();

    // The dispatcher matches `$PWD`, so standing in the symlinked path leaves
    // the scope inactive even though it is the same directory.
    cmd_in(&home, &link)
        .args(["scope", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(active)").not())
        .stdout(predicate::str::contains(
            "(inactive here: $PWD is $HOME/proj, which reaches it through a symlink)",
        ));

    cmd_in(&home, &real)
        .args(["scope", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("(active)"))
        .stdout(predicate::str::contains("inactive here").not());
}

#[test]
fn scope_rm_requires_a_disposition() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();
    cmd_in(&home, &api)
        .args(["add", "dep", "-c", "echo dep", "--desc", "d", "--here"])
        .assert()
        .success();

    cmd_in(&home, home.path())
        .args(["scope", "rm", "api"])
        .assert()
        .failure();
    cmd_in(&home, home.path())
        .args(["scope", "rm", "api", "--promote-to-global"])
        .assert()
        .success();
    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(zsh.contains("alias dep="), "zsh was: {zsh}");
    assert!(!zsh.contains("_cmdman_dir_1"), "zsh was: {zsh}");
}

#[test]
fn scope_rm_can_delete_the_entries_with_the_scope() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();
    cmd_in(&home, &api)
        .args(["add", "dep", "-c", "echo dep", "--desc", "d", "--here"])
        .assert()
        .success();

    cmd_in(&home, home.path())
        .args(["scope", "rm", "api", "--delete-entries"])
        .assert()
        .success()
        .stdout(predicate::str::contains("1 entry"));
    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(!zsh.contains("echo dep"), "zsh was: {zsh}");
}

#[test]
fn scope_relocate_adopts_an_orphaned_entry() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();
    cmd_in(&home, home.path())
        .args(["add", "ghost", "-c", "echo boo", "--desc", "d"])
        .assert()
        .success();
    let file = store_dir(&home).join("categories/general.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
        &file,
        text.replace("name = \"ghost\"", "name = \"ghost\"\nscope = \"gone\""),
    )
    .unwrap();

    cmd_in(&home, home.path())
        .args(["scope", "relocate", "gone", api.to_str().unwrap()])
        .assert()
        .success();

    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(zsh.contains("echo boo"), "zsh was: {zsh}");
    assert!(
        zsh.contains(&format!("_cmdman_dir_1='{}'", api.display())),
        "zsh was: {zsh}"
    );
}

#[test]
fn scope_relocate_keeps_a_symlinked_directory_the_way_the_shell_names_it() {
    let home = TempDir::new().unwrap();
    let real = home.path().join("actual/proj");
    std::fs::create_dir_all(&real).unwrap();
    let link = home.path().join("proj");
    std::os::unix::fs::symlink(&real, &link).unwrap();

    cmd_in(&home, &link)
        .args(["add", "dep", "-c", "make deploy", "--desc", "d", "--here"])
        .assert()
        .success();

    cmd_in(&home, &link)
        .args(["scope", "relocate", "proj", link.to_str().unwrap()])
        .assert()
        .success();

    let zsh = std::fs::read_to_string(store_dir(&home).join("shell/cmd-man.zsh")).unwrap();
    assert!(
        zsh.contains(&format!("_cmdman_dir_1='{}'", link.display())),
        "zsh was: {zsh}"
    );
}

#[test]
fn scope_relocate_rejects_a_reserved_or_unsafe_id() {
    let home = TempDir::new().unwrap();
    let proj = home.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();

    cmd_in(&home, home.path())
        .args(["scope", "relocate", "global", proj.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("reserved"));

    cmd_in(&home, home.path())
        .args([
            "scope",
            "relocate",
            "bad\nalias pwned=oops #",
            proj.to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not a valid scope id"));

    cmd_in(&home, home.path())
        .args(["scope", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No scopes."));
}

#[test]
fn capture_of_a_scoped_entry_runs_in_the_scope_directory() {
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();

    cmd_in(&home, &api)
        .args([
            "add",
            "where",
            "-c",
            "pwd",
            "--desc",
            "where am i",
            "--here",
        ])
        .assert()
        .success();

    // Run capture from somewhere else entirely.
    cmd_in(&home, home.path())
        .args(["capture", "where", "--in", "api"])
        .assert()
        .success();

    let toml = std::fs::read_to_string(store_dir(&home).join("categories/general.toml")).unwrap();
    // Sanitization rewrites the home prefix, so the scope-relative tail is what
    // distinguishes a run inside the scope from one in the process directory.
    assert!(toml.contains("work/api"), "expected work/api in: {toml}");
}

#[test]
fn import_does_not_duplicate_a_scoped_name_as_a_global() {
    // The scan spawns an interactive zsh; without one there is nothing to scan.
    if !have_zsh() {
        return;
    }
    let home = TempDir::new().unwrap();
    let api = home.path().join("work/api");
    std::fs::create_dir_all(&api).unwrap();

    cmd_in(&home, &api)
        .args(["add", "dep", "-c", "echo scoped", "--desc", "s", "--here"])
        .assert()
        .success();

    // Install the rc block first, and point zsh's rc lookup at the temp home,
    // so the shell that import scans reports the scoped alias when it starts
    // inside the scope.
    cmd(&home).args(["init"]).assert().success();
    cmd_in(&home, &api)
        .env("ZDOTDIR", home.path())
        .args(["import", "--yes"])
        .assert()
        .success();

    let all = app_entries(&home);
    assert_eq!(all.iter().filter(|e| e.name == "dep").count(), 1);
    assert_eq!(
        all.iter()
            .find(|e| e.name == "dep")
            .unwrap()
            .scope
            .as_deref(),
        Some("api")
    );
    // The generated file autoloads `add-zsh-hook` to register the dispatcher,
    // so the scanned shell reports it as a defined function.
    assert!(all.iter().all(|e| e.name != "add-zsh-hook"));
}
