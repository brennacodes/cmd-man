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

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use cmd_man::model::{Entry, EntryKey, Kind};
use cmd_man::paths::{Paths, portable_path_with};
use cmd_man::scopes::{Scope, ScopeRegistry};
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
        let mut scope = self
            .scopes
            .plan_create(dir, "2026-08-29T00:00:00Z", &HashSet::new());
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

/// `scopes.toml` is what backup and sync carry, so a hand edit, a bad merge or
/// a hostile backup repo can put anything in it. Neither an id nor a directory
/// out of it may become code in a file every new shell sources.
#[test]
fn an_injected_scope_registry_defines_nothing_in_either_shell() {
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_scoped(&api, "dep", Kind::Alias, "echo dep");

    let injected_id = "bad\nalias pwned='echo PWNED'\nfoo";
    let one = fx.dir("work/one");
    fx.scopes.insert(Scope {
        id: injected_id.into(),
        path: portable_path_with(&one, Some(&fx.home)),
        created_at: "2026-08-29T00:00:00Z".into(),
    });
    let two = fx.dir("work/two");
    fx.scopes.insert(Scope {
        id: "two".into(),
        path: format!(
            "{}\nalias owned='echo OWNED'\n#",
            portable_path_with(&two, Some(&fx.home))
        ),
        created_at: "2026-08-29T00:00:00Z".into(),
    });
    for (name, scope) in [("one", injected_id), ("two", "two")] {
        let mut entry = Entry::new(name, Kind::Alias, "echo scoped", "d");
        entry.scope = Some(scope.into());
        fx.store.add(entry).unwrap();
    }
    fx.generate();

    for shell in ["zsh", "bash"] {
        let rendered = std::fs::read_to_string(fx.file(shell)).unwrap();
        assert!(!rendered.contains("PWNED"), "{shell} file was: {rendered}");
        assert!(!rendered.contains("OWNED"), "{shell} file was: {rendered}");
        if !have(shell) {
            continue;
        }
        // The healthy scope still gets its block, which means the file parsed
        // all the way through rather than dying at an injected line.
        let out = run_shell(
            shell,
            &fx.file(shell),
            &format!(
                "type pwned\ntype owned\ncd '{}'\n_cmdman_apply\neval dep\n",
                api.display()
            ),
        );
        assert!(!out.contains("PWNED"), "{shell} out was: {out}");
        assert!(!out.contains("OWNED"), "{shell} out was: {out}");
        assert!(out.contains("dep"), "{shell} out was: {out}");
    }
}

/// A function body is the one stored value written into the generated file as
/// code rather than as a quoted string, and category files travel over sync the
/// same way `scopes.toml` does. A body that closes its own definition would put
/// its tail at top level, where a scope block runs it on every `cd`.
#[test]
fn an_injected_function_body_defines_nothing_in_either_shell() {
    let mut fx = Fixture::new();
    let api = fx.dir("work/api");
    fx.add_scoped(&api, "dep", Kind::Alias, "echo dep");
    fx.add_global("ll", Kind::Alias, "ls -la");
    fx.add_global("kruby", Kind::Function, "echo safe");
    fx.add_scoped(&api, "scoped", Kind::Function, "echo safe-scoped");
    // Sorted after each bad body, and functions because bash does not expand
    // aliases in a non-interactive shell.
    fx.add_global("survivor", Kind::Function, "echo SURVIVED");
    fx.add_scoped(&api, "survivor_here", Kind::Function, "echo SURVIVED-HERE");

    // The store accepts both bodies, so put the malformed one in the way a bad
    // merge would: straight into the category file, behind `Entry::validate`.
    fx.store.save(&fx.paths).unwrap();
    fx.scopes.save(&fx.paths).unwrap();
    let file = fx.paths.categories_dir().join("general.toml");
    let text = std::fs::read_to_string(&file).unwrap();
    std::fs::write(
        &file,
        text.replace(
            "command = \"echo safe\"",
            "command = \"}\\nalias pwned='echo PWNED'\"",
        )
        .replace(
            "command = \"echo safe-scoped\"",
            "command = \"}\\nalias owned='echo OWNED'\"",
        ),
    )
    .unwrap();
    fx.run(&["reload"]);

    for shell in ["zsh", "bash"] {
        let rendered = std::fs::read_to_string(fx.file(shell)).unwrap();
        assert!(!rendered.contains("PWNED"), "{shell} file was: {rendered}");
        assert!(!rendered.contains("OWNED"), "{shell} file was: {rendered}");
        if !have(shell) {
            continue;
        }
        // The healthy entries still resolve, which means the file parsed all
        // the way through rather than dying where the bad body was dropped.
        let out = run_shell(
            shell,
            &fx.file(shell),
            &format!(
                "type pwned\ntype owned\nsurvivor\ncd '{}'\n_cmdman_apply\nsurvivor_here\n",
                api.display()
            ),
        );
        assert!(!out.contains("PWNED"), "{shell} out was: {out}");
        assert!(!out.contains("OWNED"), "{shell} out was: {out}");
        assert!(out.contains("SURVIVED"), "{shell} out was: {out}");
        assert!(out.contains("SURVIVED-HERE"), "{shell} out was: {out}");
    }
}

/// Bodies the lint accepts, in the shapes most likely to fool a scan that reads
/// shell text without parsing it.
const ACCEPTED_BODIES: &[&str] = &[
    "git status",
    "echo {",
    "echo $(date)#tag",
    "grep -q x <<< \"$1\"",
    "echo $((1 << 2))",
    "[ -d .git ] && { git status; git log -1; }",
    "if [ -d x ]; then cd x; else mkdir x; fi",
    "for f in *.rs; do echo \"$f\"; done",
    "case \"$1\" in\n  do) echo d ;;\n  done) echo n ;;\n  *) echo o ;;\nesac",
    "case $1 in (a|b) echo ab ;; (*) echo o ;; esac",
    "inner() {\n  echo hi\n}\ninner",
    "function inner {\n  echo hi\n}",
    "cat <<EOF\n}\nEOF",
    "echo \"$(basename \"$1\")\"",
    "! command -v git > /dev/null",
    "docker ps --format \"{{.Names}}\"",
    "awk '{print $1}' \"$1\"",
    "printf '%s\\n' \"${1:-5}\"",
    "echo pre{a,b}post",
    "find . -name '*.tmp' -exec rm {} \\;",
    "echo a &&\n  echo b",
    "git log --oneline | head -5",
    "until [ -f ready ]; do sleep 1; done",
    "select o in a b; do echo \"$o\"; break; done",
    "case $1 in a) ;; *) echo o ;; esac",
    "sleep 0 &\nwait",
    "time for i in 1; do :; done",
    "if { true; }; then echo a; fi",
    "if case $1 in a) true ;; esac; then echo a; fi",
    "echo ${x//\\}/]}",
    "x=$(cat <<'EOF'\n}\nEOF\n)\necho \"$x\" > /dev/null",
    "trap '}' EXIT",
    "cat <<E}F\nx\nE}F",
    "echo {a,${x}}",
    "case $1 in a) case $2 in b) echo b ;; esac ;; esac",
    "{\n  echo a\n  echo b\n} | cat",
    "for ((i = 1 << 4; i > 0; i >>= 1)); do echo $i; done",
    "for ((;;)); do break; done",
    "for ((i = 0; i < 3; i++)) do echo $i; done",
    "case $1 in esac",
    "echo $()",
    "cat /dev/null 2>&1 | cat",
    "args=()\nargs+=(x)\necho \"${args[@]}\" > /dev/null",
    "echo \"`date +%Y`\" > /dev/null",
    "cat <<\\XEOF\n}\nXEOF",
    "function inner ( ) {\n  echo hi\n}\ninner > /dev/null",
];

/// Bodies the lint refuses because a shell really does break on one.
///
/// The accepted corpus proves the lint is not too strict; this one proves it is
/// not strict for nothing. Every entry here has to damage at least one shell,
/// which is what earns the refusal. The bar is "at least one" rather than
/// "both" on purpose: `ls -l}` closes the definition in zsh and is an argument
/// in bash, and refusing it costs a rewrite while accepting it costs a breakout.
const REFUSED_BODIES: &[&str] = &[
    "}\necho PWNED",
    "ls -l}\necho PWNED",
    "echo ${x}}\necho PWNED",
    "echo $(date)}\necho PWNED",
    "{ echo a }",
    "{ echo a",
    "echo 'oops",
    "cat <<EOF\nhi",
    "echo &&",
    "then echo hi",
    "echo a ;;",
    "do echo hi; done",
    "for f in *; echo \"$f\"",
    "case $1 in a) echo a ;;& b) echo b ;; esac",
    "echo a ;& echo b",
    "echo a ;| echo b",
    "echo a && ; echo b",
    "echo a; ; echo b",
    "| echo a",
    "{ }",
    "( )",
    "if true; then fi",
    "if true; then echo a; else fi",
    "while true; do done",
    "# just a note",
    "foo ( ) { echo hi; }\necho PWNED",
    "echo a\n| grep x",
    "echo a\n; echo b",
    "cat <<EOF\nx\nEOF\n| grep y",
    "echo a && !",
    "if then echo x; fi",
    "while do echo a; done",
    "case $1 in ;; esac",
];

/// The other half of the function-body lint: a body it accepts has to survive
/// being rendered. Asserting only that `body_problem` returns `None` cannot see
/// a `{` the scan counted as opening a group that the shell reads as a word,
/// because that mistake shows up as a later `}` closing the definition early.
/// Sourcing the real generated file does see it: the definition after the probe
/// goes missing, and the body's tail runs at source time.
#[test]
fn an_accepted_function_body_neither_escapes_nor_swallows_its_definition() {
    let shells: Vec<&str> = ["zsh", "bash"].into_iter().filter(|s| have(s)).collect();
    if shells.is_empty() {
        return;
    }
    for body in ACCEPTED_BODIES {
        assert_eq!(
            cmd_man::lint::body_problem(body),
            None,
            "body was: {body:?}"
        );
        let mut fx = Fixture::new();
        fx.add_global("probe", Kind::Function, body);
        // A function rather than an alias, because bash does not expand aliases
        // in a non-interactive shell and would report the same text either way.
        fx.add_global("sentinel", Kind::Function, "echo SENTINEL");
        fx.generate();

        for shell in &shells {
            // DEFINED says the body's own definition survived, SENTINEL that the
            // file parsed past it. A body that escapes loses the first, one that
            // swallows loses the second.
            let out = run_shell(
                shell,
                &fx.file(shell),
                "echo QUIET\nsentinel\ntype probe > /dev/null 2>&1 && echo DEFINED\n",
            );
            assert_eq!(
                out, "QUIET\nSENTINEL\nDEFINED\n",
                "{shell} sourced {body:?} and said: {out}"
            );
        }
    }
}

/// A body only zsh reads is written into `cmd-man.zsh` and left out of
/// `cmd-man.bash`, and the bash file is whole without it.
///
/// Rendering it into both would be the worst of the two failures this lint
/// exists for: bash abandons the file at the syntax error, so every later
/// definition and the scope dispatcher would go with it.
#[test]
fn a_zsh_only_case_fall_through_reaches_zsh_and_leaves_bash_intact() {
    let mut fx = Fixture::new();
    fx.add_global(
        "probe",
        Kind::Function,
        "case $1 in a) echo A ;& b) echo B ;; esac",
    );
    fx.add_global("sentinel", Kind::Function, "echo SENTINEL");
    fx.generate();

    if have("zsh") {
        let out = run_shell("zsh", &fx.file("zsh"), "probe a\nsentinel\n");
        assert_eq!(out, "A\nB\nSENTINEL\n", "zsh said: {out}");
    }
    if have("bash") {
        let out = run_shell(
            "bash",
            &fx.file("bash"),
            "sentinel\ntype probe > /dev/null 2>&1 || echo NO-PROBE\n",
        );
        assert_eq!(out, "SENTINEL\nNO-PROBE\n", "bash said: {out}");
    }
}

/// The other zsh-only spelling, and the one a plain `cmd-man add` reaches
/// without any quoting mistake: `|&` is zsh's `2>&1 |`, and bash 3 breaks on it.
#[test]
fn a_zsh_only_pipe_reaches_zsh_and_leaves_bash_intact() {
    let mut fx = Fixture::new();
    fx.add_global("probe", Kind::Function, "echo A |& cat");
    fx.add_global("sentinel", Kind::Function, "echo SENTINEL");
    fx.generate();

    if have("zsh") {
        let out = run_shell("zsh", &fx.file("zsh"), "probe\nsentinel\n");
        assert_eq!(out, "A\nSENTINEL\n", "zsh said: {out}");
    }
    if have("bash") {
        let out = run_shell(
            "bash",
            &fx.file("bash"),
            "sentinel\ntype probe > /dev/null 2>&1 || echo NO-PROBE\n",
        );
        assert_eq!(out, "SENTINEL\nNO-PROBE\n", "bash said: {out}");
    }
}

/// The refusals earn their keep: each body the lint turns away really does break
/// a shell that is handed it.
///
/// This is the only test that can see an over-strict rule, because a refusal is
/// otherwise checked against nothing but the lint's own message. The definition
/// is written by hand rather than by the renderer, which would drop the entry.
#[test]
fn a_refused_function_body_damages_a_real_shell() {
    let shells: Vec<&str> = ["zsh", "bash"].into_iter().filter(|s| have(s)).collect();
    if shells.is_empty() {
        return;
    }
    let dir = TempDir::new().unwrap();
    for (n, body) in REFUSED_BODIES.iter().enumerate() {
        assert!(
            cmd_man::lint::body_problem(body).is_some(),
            "body was: {body:?}"
        );
        let file = dir.path().join(format!("probe{n}.sh"));
        std::fs::write(
            &file,
            format!("function probe {{\n{body}\n}}\nfunction sentinel {{\n  echo SENTINEL\n}}\n"),
        )
        .unwrap();

        let damaged: Vec<&str> = shells
            .iter()
            .copied()
            .filter(|shell| {
                run_shell(shell, &file, "echo QUIET\nsentinel\n") != "QUIET\nSENTINEL\n"
            })
            .collect();
        assert!(
            !damaged.is_empty(),
            "{body:?} was refused but {shells:?} all sourced it cleanly"
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
