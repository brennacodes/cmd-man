//! Integration tests that run the generated shell files through real shells.
//!
//! Every test returns early when its shell is missing rather than failing, so
//! the suite stays green on a machine that only has one of them.
//!
//! Two harness limitations are worth naming, because both are properties of
//! `-c` scripts rather than of the feature:
//!
//! - bash never runs `PROMPT_COMMAND` in a non-interactive shell, so the bash
//!   cases call `_cmdman_apply` explicitly after each `cd`. zsh's `chpwd` hook
//!   does fire, so the zsh cases just `cd`.
//! - zsh parses a whole `-c` script before running any of it, so an alias a
//!   sourced file defines at runtime is not expanded on a later line. The zsh
//!   cases run a managed name through `eval`, which resolves it at run time the
//!   way an interactive shell would.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use cmd_man::model::{Entry, EntryKey, Kind};
use cmd_man::paths::{Paths, portable_path_with};
use cmd_man::scopes::ScopeRegistry;
use cmd_man::store::Store;
use tempfile::TempDir;

/// True when the shell is installed. Probing by spawning avoids depending on
/// any particular lookup tool being present.
fn have(shell: &str) -> bool {
    std::process::Command::new(shell)
        .arg("-c")
        .arg("exit 0")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run a script in `shell`, returning stdout and stderr. `prelude` runs before
/// the generated file is sourced, `script` after it. A prelude is how a test
/// reproduces an rc file that sets a shell option ahead of the cmd-man block.
fn run_shell_with_prelude(shell: &str, prelude: &str, file: &Path, script: &str) -> String {
    let full = format!("{prelude}source '{}'\n{script}", file.display());
    let out = std::process::Command::new(shell)
        .arg("-c")
        .arg(full)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {shell}: {e}"));
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// Run a script in `shell`, returning stdout and stderr. The generated file is
/// sourced first.
fn run_shell(shell: &str, file: &Path, script: &str) -> String {
    run_shell_with_prelude(shell, "", file, script)
}

/// A throwaway `$HOME` holding a cmd-man store.
///
/// Entries and scopes are seeded through the library so each case can state the
/// exact store it needs without going through placement resolution. The shell
/// files are still produced by the real binary, via `cmd-man reload`, so the
/// generation path under test is the shipped one.
struct Fixture {
    _home: TempDir,
    home: PathBuf,
    paths: Paths,
    store: Store,
    scopes: ScopeRegistry,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let home = temp.path().canonicalize().unwrap();
        let paths = Paths::with_root(home.join("store"));
        paths.ensure_dirs().unwrap();
        Fixture {
            _home: temp,
            home,
            paths,
            store: Store::default(),
            scopes: ScopeRegistry::default(),
        }
    }

    /// A directory under the temp home, created and canonicalized so it matches
    /// the `$PWD` a shell reports after `cd`.
    fn dir(&self, rel: &str) -> PathBuf {
        let dir = self.home.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn add_global(&mut self, name: &str, kind: Kind, command: &str) {
        self.store
            .add(Entry::new(name, kind, command, "d"))
            .unwrap();
    }

    fn add_scoped(&mut self, dir: &Path, name: &str, kind: Kind, command: &str) {
        let mut entry = Entry::new(name, kind, command, "d");
        entry.scope = Some(self.scope_id(dir));
        self.store.add(entry).unwrap();
    }

    /// The id of the scope for `dir`, registering it on first use.
    ///
    /// The stored path is made portable against the fixture's home rather than
    /// the real one, so it lands in `scopes.toml` as `$HOME/...` and the binary,
    /// which runs with `HOME` pointing at the fixture, expands it back to `dir`.
    /// Deriving it from the real home instead would break as soon as `TMPDIR`
    /// itself lived under `$HOME`.
    fn scope_id(&mut self, dir: &Path) -> String {
        let path = portable_path_with(dir, Some(&self.home));
        if let Some(scope) = self.scopes.scopes().iter().find(|s| s.path == path) {
            return scope.id.clone();
        }
        let mut scope = self.scopes.plan_create(dir, "2026-08-29T00:00:00Z");
        scope.path = path;
        let id = scope.id.clone();
        self.scopes.insert(scope);
        id
    }

    /// Remove an entry with the real binary, naming its scope with `--in`, and
    /// keep the fixture's own store in step. The binary regenerates the shell
    /// files itself, so no `generate` call is needed afterwards.
    fn remove(&mut self, key: &EntryKey) {
        let scope = key.scope.as_deref().unwrap_or("global");
        self.run(&["rm", key.name.as_str(), "--in", scope]);
        self.store.remove(key).unwrap();
    }

    /// Save the store and regenerate both shell files with the real binary.
    fn generate(&self) {
        self.store.save(&self.paths).unwrap();
        self.scopes.save(&self.paths).unwrap();
        self.run(&["reload"]);
    }

    /// Run the real binary against the fixture's home and store.
    fn run(&self, args: &[&str]) {
        Command::cargo_bin("cmd-man")
            .unwrap()
            .env("HOME", &self.home)
            .env("CMD_MAN_HOME", self.paths.root())
            .env("CMD_MAN_DISABLE_SYNC", "1")
            .args(args)
            .assert()
            .success();
    }

    fn file(&self, shell: &str) -> PathBuf {
        self.paths
            .root()
            .join("shell")
            .join(format!("cmd-man.{shell}"))
    }
}

/// The property every other case in this file leans on: `scopes.toml` holds the
/// portable form and the binary expands it, so the shells below are matching
/// against a real directory rather than a literal `$HOME`.
#[test]
fn scope_path_is_stored_home_relative_and_expanded_when_rendered() {
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_scoped(&api, "dep", Kind::Alias, "echo dep");
    fx.generate();

    let stored = std::fs::read_to_string(fx.paths.scopes_file()).unwrap();
    assert!(stored.contains("$HOME/work/api"), "stored was: {stored}");

    for shell in ["zsh", "bash"] {
        let rendered = std::fs::read_to_string(fx.file(shell)).unwrap();
        assert!(
            rendered.contains(&format!("_cmdman_dir_1='{}'", api.display())),
            "{shell} file was: {rendered}"
        );
    }
}

#[test]
fn zsh_scoped_alias_activates_and_deactivates_on_cd() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_global("ll", Kind::Alias, "ls -la");
    fx.add_scoped(&api, "ll", Kind::Alias, "ls -lah");
    fx.generate();

    let out = run_shell(
        "zsh",
        &fx.file("zsh"),
        &format!("cd '{}'\ntype ll\ncd /\ntype ll\n", api.display()),
    );
    let lines: Vec<_> = out.lines().filter(|l| l.contains("ll")).collect();
    assert_eq!(lines.len(), 2, "out was: {out}");
    assert!(lines[0].contains("ls -lah"), "out was: {out}");
    assert!(
        lines[1].contains("ls -la") && !lines[1].contains("ls -lah"),
        "out was: {out}"
    );
}

#[test]
fn zsh_scoped_function_is_shadowed_by_a_scoped_alias_of_the_same_name() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_global("dep", Kind::Function, "echo global-dep");
    fx.add_scoped(&api, "dep", Kind::Alias, "echo scoped-dep");
    fx.generate();

    let out = run_shell(
        "zsh",
        &fx.file("zsh"),
        &format!("cd '{}'\neval dep\ncd /\neval dep\n", api.display()),
    );
    assert!(out.contains("scoped-dep"), "out was: {out}");
    assert!(out.contains("global-dep"), "out was: {out}");
}

#[test]
fn zsh_re_source_inside_a_scope_keeps_the_scoped_definition() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_global("ll", Kind::Alias, "ls -la");
    fx.add_scoped(&api, "ll", Kind::Alias, "ls -lah");
    fx.generate();

    let file = fx.file("zsh");
    let out = run_shell(
        "zsh",
        &file,
        &format!(
            "cd '{}'\nsource '{}'\ntype ll\n",
            api.display(),
            file.display()
        ),
    );
    assert!(
        out.contains("ls -lah"),
        "re-source dropped the scope: {out}"
    );
}

#[test]
fn zsh_shell_started_inside_a_nested_scope_primes_correctly() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let outer = fx.dir("work");
    let inner = fx.dir("work/api");
    fx.add_scoped(&outer, "x", Kind::Alias, "echo outer");
    fx.add_scoped(&inner, "x", Kind::Alias, "echo inner");
    fx.generate();

    let file = fx.file("zsh");
    let out = std::process::Command::new("zsh")
        .arg("-c")
        .arg(format!("source '{}'\ntype x\n", file.display()))
        .current_dir(&inner)
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.contains("echo inner"), "out was: {out}");
}

#[test]
fn zsh_cd_out_of_two_nested_scopes_at_once_restores_the_global() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let outer = fx.dir("work");
    let inner = fx.dir("work/api");
    fx.add_global("x", Kind::Alias, "echo global");
    fx.add_scoped(&outer, "x", Kind::Alias, "echo outer");
    fx.add_scoped(&inner, "x", Kind::Alias, "echo inner");
    fx.generate();

    let out = run_shell(
        "zsh",
        &fx.file("zsh"),
        &format!("cd '{}'\neval x\ncd /\neval x\n", inner.display()),
    );
    assert!(out.contains("inner"), "out was: {out}");
    assert!(out.contains("global"), "out was: {out}");
    assert!(
        !out.contains("outer"),
        "cd out of two scopes ran the wrong teardown: {out}"
    );
}

#[test]
fn zsh_pushd_and_popd_toggle_the_scope() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_global("x", Kind::Alias, "echo global");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let out = run_shell(
        "zsh",
        &fx.file("zsh"),
        &format!(
            "cd /\npushd '{}' >/dev/null\neval x\npopd >/dev/null\neval x\n",
            api.display()
        ),
    );
    assert!(out.contains("scoped"), "out was: {out}");
    assert!(out.contains("global"), "out was: {out}");
}

#[test]
fn zsh_sibling_prefix_directory_does_not_activate_the_scope() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/bar");
    let sibling = fx.dir("work/barbaz");
    fx.add_global("x", Kind::Alias, "echo global");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let out = run_shell(
        "zsh",
        &fx.file("zsh"),
        &format!("cd '{}'\neval x\n", sibling.display()),
    );
    assert!(out.contains("global"), "out was: {out}");
    assert!(
        !out.contains("scoped"),
        "prefix matching leaked into a sibling: {out}"
    );
}

#[test]
fn zsh_delete_then_regenerate_then_resource_clears_the_removed_name() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    let scope = fx.scope_id(&api);
    fx.add_scoped(&api, "gone", Kind::Alias, "echo gone");
    fx.generate();
    let stale_file = fx.home.join("stale.zsh");
    std::fs::copy(fx.file("zsh"), &stale_file).unwrap();

    fx.remove(&EntryKey {
        scope: Some(scope),
        name: "gone".into(),
    });

    let file = fx.file("zsh");
    let out = run_shell(
        "zsh",
        &stale_file,
        &format!(
            "cd '{}'\nsource '{}'\ntype gone || echo not-found\n",
            api.display(),
            file.display()
        ),
    );
    assert!(
        out.contains("not-found"),
        "preamble did not clear a removed name: {out}"
    );
}

/// The load-bearing order: an rc file turns `nounset` on before the cmd-man
/// block is sourced, so every parameter the file reads is still unset the first
/// time it is read. This is what the `${var-}` defaults exist for; without them
/// the source itself aborts.
#[test]
fn zsh_dispatcher_survives_nounset_enabled_before_the_source() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_global("x", Kind::Alias, "echo global");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let out = run_shell_with_prelude(
        "zsh",
        "setopt nounset\n",
        &fx.file("zsh"),
        &format!("cd '{}'\neval x\ncd /\neval x\n", api.display()),
    );
    assert!(out.contains("scoped"), "out was: {out}");
    assert!(out.contains("global"), "out was: {out}");
    assert!(
        !out.to_lowercase().contains("parameter not set"),
        "out was: {out}"
    );
}

/// `fpath=()` makes `add-zsh-hook` fail, so the source takes the
/// `chpwd_functions` fallback branch. Combined with `nounset` this is the only
/// order in which `${chpwd_functions[*]-}` is read while still unset.
#[test]
fn zsh_hook_fallback_survives_nounset_enabled_before_the_source() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let out = run_shell_with_prelude(
        "zsh",
        "setopt nounset\nfpath=()\n",
        &fx.file("zsh"),
        &format!("cd '{}'\neval x\n", api.display()),
    );
    assert!(out.contains("scoped"), "out was: {out}");
    assert!(
        !out.to_lowercase().contains("parameter not set"),
        "out was: {out}"
    );
}

#[test]
fn zsh_dispatcher_survives_nounset_enabled_after_the_source() {
    if !have("zsh") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_global("x", Kind::Alias, "echo global");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let out = run_shell(
        "zsh",
        &fx.file("zsh"),
        &format!(
            "setopt nounset\ncd '{}'\neval x\ncd /\neval x\n",
            api.display()
        ),
    );
    assert!(out.contains("scoped"), "out was: {out}");
    assert!(out.contains("global"), "out was: {out}");
    assert!(
        !out.to_lowercase().contains("parameter not set"),
        "out was: {out}"
    );
}

#[test]
fn bash_scoped_alias_toggles_when_the_hook_runs() {
    if !have("bash") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_global("x", Kind::Alias, "echo global");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let out = run_shell(
        "bash",
        &fx.file("bash"),
        &format!(
            "shopt -s expand_aliases\ncd '{}'\n_cmdman_apply\nx\ncd /\n_cmdman_apply\nx\n",
            api.display()
        ),
    );
    assert!(out.contains("scoped"), "out was: {out}");
    assert!(out.contains("global"), "out was: {out}");
}

#[test]
fn bash_prompt_command_wiring_is_idempotent_and_appends() {
    if !have("bash") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let file = fx.file("bash");
    let out = run_shell(
        "bash",
        &file,
        &format!(
            "source '{}'\nsource '{}'\necho \"PC=[$PROMPT_COMMAND]\"\n",
            file.display(),
            file.display()
        ),
    );
    let pc = out.lines().find(|l| l.starts_with("PC=[")).unwrap();
    assert_eq!(pc.matches("_cmdman_apply").count(), 1, "out was: {out}");
}

#[test]
fn bash_prompt_command_preserves_a_user_hook() {
    if !have("bash") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let file = fx.file("bash");
    // A hook that already ends in `;`, or in trailing whitespace, is common, and
    // a hook that is only whitespace is legal. Joining with a `;` turns each of
    // those into a syntax error bash reports at every prompt while running
    // neither half, so every spelling has to still eval and still activate the
    // scope.
    for hook in ["mine", "mine;", "mine; ", "mine ;", " "] {
        let out = run_shell_with_prelude(
            "bash",
            &format!(
                "shopt -s expand_aliases\nmine() {{ echo ran-mine; }}\nPROMPT_COMMAND='{hook}'\n"
            ),
            &file,
            &format!("cd '{}'\neval \"$PROMPT_COMMAND\"\ntype x\n", api.display()),
        );
        assert!(!out.contains("syntax error"), "hook {hook:?} gave: {out}");
        assert!(out.contains("echo scoped"), "hook {hook:?} gave: {out}");
        if hook.contains("mine") {
            assert!(out.contains("ran-mine"), "hook {hook:?} gave: {out}");
        }
    }
}

#[test]
fn bash_dispatcher_preserves_the_exit_status() {
    if !have("bash") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let out = run_shell(
        "bash",
        &fx.file("bash"),
        "(exit 7)\n_cmdman_apply\necho \"rc=$?\"\n",
    );
    assert!(out.contains("rc=7"), "out was: {out}");
}

/// The load-bearing order, as in the zsh case: `set -u` is already on when the
/// generated file is sourced, so the `${var-}` defaults are what keep the
/// source and the priming call alive.
#[test]
fn bash_dispatcher_survives_set_u_enabled_before_the_source() {
    if !have("bash") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_global("x", Kind::Alias, "echo global");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let out = run_shell_with_prelude(
        "bash",
        "set -u\nshopt -s expand_aliases\n",
        &fx.file("bash"),
        &format!(
            "cd '{}'\n_cmdman_apply\nx\ncd /\n_cmdman_apply\nx\n",
            api.display()
        ),
    );
    assert!(out.contains("scoped"), "out was: {out}");
    assert!(out.contains("global"), "out was: {out}");
    assert!(!out.contains("unbound variable"), "out was: {out}");
}

#[test]
fn bash_dispatcher_survives_set_u_enabled_after_the_source() {
    if !have("bash") {
        return;
    }
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_global("x", Kind::Alias, "echo global");
    fx.add_scoped(&api, "x", Kind::Alias, "echo scoped");
    fx.generate();

    let out = run_shell(
        "bash",
        &fx.file("bash"),
        &format!(
            "set -u\nshopt -s expand_aliases\ncd '{}'\n_cmdman_apply\nx\ncd /\n_cmdman_apply\nx\n",
            api.display()
        ),
    );
    assert!(out.contains("scoped"), "out was: {out}");
    assert!(out.contains("global"), "out was: {out}");
    assert!(!out.contains("unbound variable"), "out was: {out}");
}
