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
        .env("SHELL", "/bin/zsh");
    c
}

fn store_dir(home: &TempDir) -> std::path::PathBuf {
    home.path().join("store")
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
    assert!(zsh.contains("hi() {"), "zsh was: {zsh}");
}
