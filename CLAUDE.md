# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

cmd-man is a Rust CLI/TUI (edition 2024) for creating, browsing, and managing shell aliases and functions. Stored entries are compiled into real `alias`/function definitions that the user's shell sources, so a stored `gitsw` behaves like a hand-written alias. Runs on macOS and Linux only.

## Commands

```sh
cargo build                 # debug build
cargo run                   # launch the interactive TUI (no subcommand)
cargo run -- <subcommand>   # exercise a CLI subcommand (add, list, capture, backup, ...)
cargo install --path .      # install the binary locally

cargo test                  # unit tests + tests/cli.rs end-to-end suite
cargo test <name>           # run a single test by substring match
cargo test --test cli       # only the integration tests

cargo fmt                   # format (no custom rustfmt config)
cargo clippy --all-targets  # lint
```

There is no CI config; run fmt, clippy, and tests locally before finishing.

## Architecture

The dependency spine is: `cli`/`tui` -> `app::App` -> (`store`, `search`, `shell`, `capture`, `backup`), all rooted at `paths::Paths`.

- **`app::App` is the service layer and the only correct place to mutate state.** It holds `paths`, `config`, `store`, the `ScopeRegistry`, and the working directory scope decisions are made against. Every mutation (`add`/`update`/`remove`/`record_capture`) calls `persist()`, which saves the store AND the scope registry, regenerates the shell definition files so the three never drift, AND kicks off a background sync (`backup::spawn_sync`). Both the CLI (`cli.rs`) and the TUI (`tui/`) go through `App` rather than touching `Store`, `ScopeRegistry`, or `shell` directly. If you add a mutation path, route it through `App`.

- **`store`** loads all `categories/*.toml` files (each a list of `[[entry]]` tables) into one flat `Vec<Entry>`. Entry identity is `(scope, name)`, not the name alone: a name may exist once globally and once per scope, and `model::EntryKey` is what every lookup, update, and delete is keyed on. `model::Entry`/`Kind`/`CapturePolicy` are the core data types.

- **`search`** matches entries per field rather than over a single joined haystack. Short identifiers (name, each tag individually, category, scope id) use subsequence matching so `laud` finds `list-app-updates-disabled`. Prose and code (the description, and the whole-entry string that carries the command body) use literal substring matching, because a subsequence over a long body is satisfied by letters scattered across unrelated words - `devin` is a subsequence of `2>/dev/null | xargs -I {} basename {} .plist`, which is why searching `devin` used to return every entry redirecting to `/dev/null`. `rank` tries the fields in order (name, tags, description, placement, whole entry) and the first match wins; sorting is by rank then score, so a name hit always outranks a whole-entry hit regardless of score. Tags are scored one at a time, since joining them recreates the long-string problem across unrelated tags.

  Category and scope id stay on subsequence matching deliberately. Almost every entry carries the default category `general`, so a query that is a subsequence of it (`gnl`, `era`) returns every entry in the category, which is the same shape of hit the description and body tiers were narrowed to stop. Narrowing these two to a substring as well was considered and declined; `a_subsequence_of_the_default_category_matches_every_entry_in_it` pins the choice so it is not silently reversed.

  Two consequences of the tiers are worth knowing. Every atom of a multi-word query has to match the *same* field, so a query pairing a subsequence-only name hit with a description word (`gtsw branches`) matches nothing even though each word alone does; only the whole-entry tier spans fields, and only literally. And the fuzzy and substring patterns are both derived from one `Pattern::parse` by rewriting `AtomKind::Fuzzy` to `AtomKind::Substring`, never from a second constructor over the same string, because `Pattern::new` does not read nucleo's query syntax and the two would then disagree about what `'foo` meant. Negated atoms (`!foo`) are partitioned out of that same parse and checked against `Entry::search_haystack()`, so `!foo` excludes entries mentioning foo anywhere rather than merely in their name; a query of nothing but negations returns everything it does not exclude.

- **`scopes`** is the directory scope registry (`scopes.toml`, a list of `[[scope]]` tables mapping a short id to a `$HOME`-relative directory). Placement resolution (`resolve_placement`) is a pure function over a `Placement`, the configured default, the registry, the cwd, and home; it returns a `Target` and mutates nothing. Only `App::add` and `App::update` turn a `Target::NewAt` into a registered scope, and only after the store has accepted the entry, so a rejected entry never leaves a scope behind; `App::relocate_scope` is the one other way a scope enters the registry. Stored paths are never canonicalized, because the generated dispatcher matches the shell's logical `$PWD`.

- **`lint`** guards the one stored value that is emitted as code rather than as data. `body_problem` scans a `Kind::Function` body the way the shell reads it (escapes, single and double quotes, comments, heredocs including `<<-` and quoted delimiters, `$(...)`, backticks, `${...}`, and `$((...))` arithmetic where `<<` is a shift) and rejects one that does not balance: a standalone `}` closes the generated definition early, so its tail runs at source time and on every `cd`, and an unclosed `{`, quote, heredoc, `if`, `case`, or `do` swallows every definition after it. A `{` opens a group only where the shell would read it as the reserved word (command position, with a blank after it, or after the `()` of a definition), because a `{` mistaken for a group gives a later `}` something to balance against and lets that `}` escape; `}` is recognized liberally for the same reason. Everywhere else ambiguity resolves towards refusing. Balance is not the whole test, because bash abandons the entire generated file at the first syntax error, taking every later definition and `_cmdman_apply` with it on every shell start, silently: the scan also refuses a `&&`/`||`/`|` with no command on either side of it, a `then`/`else`/`elif` with no open `if`, a `do` with no loop and a loop with no `do`, a `;;` with no `case` branch to end, and a list that holds no command at all (`{ }`, `if true; then fi`, `if then`, `while do`). Two flags carry those last two: `prev_command` (a command has begun since the last separator, a newline included) gates the operator check, `list_command` (a command has appeared anywhere in the list being read) gates the empty-list check at `}`, `)`, `fi`, `done`, `then`, `do` and end of body. `esac`, `$( )` and a `(` outside command position are exempt, because `case x in esac`, `echo $()` and `arr=()` are valid in both shells. It is still a structural scan and not a parser, so errors outside that set reach the shell, a `case` branch opened by a `)` with no pattern before it included. Alias expansions are not linted; they leave through `shell::single_quote`.

  The scan is per generated file, because the two shells do not read the same language: `body_problem_in(body, shell)` answers for one of them and `body_problem` answers for all of them. Two rules differ today, both zsh-only: `|&` (zsh's `2>&1 |`, which bash has no spelling for) and the `case` fall-through terminator - `;&` and `;|` are zsh's and every bash rejects them, `;;&` needs bash 4 and zsh rejects it, so the bash 3.2 macOS ships reads none of the three. The bash the scan answers for is version 3, statically, because generated files sync across machines. A body only one shell reads is stored rather than refused, written into that shell's file alone by `render`'s `command_problem_in` filter, and reported once by `App::exclusion_notes` at the save that caused it. `Entry::validate` calls `body_problem`, so every save path (CLI, TUI, import) refuses a body no shell can carry; `App::finish_load`'s notice uses `command_problem_among(&enabled_shells())`, so it also catches a body that only a turned-off shell could have read, and never fires for a merely unportable one.

- **`paths::Paths`** resolves the data root: `CMD_MAN_HOME` (tests and power users) > `XDG_CONFIG_HOME/cmd-man` > `~/.config/cmd-man`. Layout: `config.toml`, `scopes.toml`, `categories/`, and generated `shell/`. `portable_path`/`expand_path` convert between absolute paths and their `$HOME`-relative stored form.

- **`shell`** renders entries into `shell/cmd-man.zsh` and `shell/cmd-man.bash` (marked "Do not edit by hand", regenerated on every change), installs a managed `# >>> cmd-man >>>` block into rc files (`.zshrc`, `.bashrc`/`.bash_profile`), and imports the user's currently active aliases/functions. The generated file is more than a flat list of definitions: a clearing preamble at the top resets every managed name, the global entries follow, and then each scope with entries gets an `_cmdman_on_N`/`_cmdman_off_N` pair plus a single `_cmdman_apply` dispatcher wired to zsh's `chpwd` and bash's `PROMPT_COMMAND`. Functions use the `function NAME { }` keyword form because the `NAME() { }` form is a zsh parse error while an alias of that name is live. There are two `shell::regenerate` call sites, `src/app.rs` (`App::regenerate_shells`) and `src/backup/sync.rs` (`regenerate_shells`); missing the second silently wipes scoped definitions after every pull.

- **`capture`** is the safe example-output pipeline: `classifier` statically flags destructive binaries/verbs, interpreters, and side-effecting verbs; `runner` executes; `sanitize` redacts home paths, usernames, IPs, emails, and token-shaped strings. Auto-fill of description/usage/options/examples from a command's `--help`/`man` output lives in `help.rs` and never overwrites populated fields.

- **`backup`** pushes `categories/`, `scopes.toml`, and `config.toml` (generated `shell/` and the sync bookkeeping files are gitignored out) to a private GitHub repo across three tiers chosen by `plan.rs` from config + runtime availability: `gh` CLI, built-in OAuth device flow (token in the OS keyring), or a plain git remote. `cmd-man backup`/`restore` are the manual paths; `backup/sync.rs` adds automatic sync (see below).

## Automatic background sync

`backup/sync.rs` keeps the store backed up with no manual step. Every invocation spawns one short-lived, detached, fire-and-die process (`spawn_sync` -> the hidden `__sync-exec` marker -> `sync_exec_main`) that runs `run_sync`: commit any pending local changes, pull, regenerate shell files if the pull changed anything, and push. It never blocks the foreground command, which always operates on the state it loaded; the background pull lands for the next invocation. Concurrency is deduped by an advisory lock (`.sync.lock`), and the outcome is written to `.sync-state.toml`; a failure is surfaced once (tracked via `.sync-notified`) as a one-line stderr notice on the next command.

- Activation is automatic whenever a remote is resolvable, preferred in order by `resolve_sync_remote`: `backup.remote_url`, then an authenticated `gh`, then a stored OAuth token. The interactive OAuth device flow never runs in the silent process; authorize once via `cmd-man backup`, then sync reuses the token. Opt out with `backup.auto_sync = false`.
- On a fresh machine (empty local store, populated remote reachable) `run_sync` auto-clones the backup and regenerates shells, so aliases travel with zero setup. Bootstrap only clones into an empty store, so it never clobbers local-only entries.
- `run_sync` is synchronous and holds all the logic, so it is tested offline against a local bare repo (`tests/sync.rs`). `spawn_sync` is a no-op under `cfg!(test)` or when `CMD_MAN_DISABLE_SYNC` is set; the `tests/cli.rs` harness sets that env so binary-driven tests never reach a real remote.

## Capture sandbox: the re-exec trick

`main.rs` checks argv for the hidden `__capture-exec` marker BEFORE any clap parsing and, if present, immediately runs `capture::capture_exec_main()`. This matters because the sandbox (the `birdcage` crate: denies network, denies writes outside temp dirs) must be applied to a dedicated, single-threaded process. `runner::run_capture` re-execs the same binary with that marker, passing the command/timeout/sandbox flags via env vars. Every run is hard-timeboxed with `wait_timeout` and the child leads its own process group so the whole subtree is killed on timeout. If sandbox setup fails it exits with a sentinel code (`SANDBOX_UNAVAILABLE_CODE`) and the caller transparently falls back to an unsandboxed timeboxed run. Do not add work before the `__capture-exec` check in `main.rs`, and keep that path single-threaded. The `__sync-exec` marker (background sync) is checked right after it; it is not sandboxed, so it may run after the capture check but must stay before clap parsing.

## Testing notes

`tests/cli.rs` drives the real compiled binary via `assert_cmd`, isolating each run with a `TempDir` and `CMD_MAN_HOME` pointing at it. Use `CMD_MAN_HOME` to point unit or manual runs at a throwaway store instead of the real `~/.config/cmd-man`.

`tests/shell_integration.rs` sources the generated files in a real `zsh -f`/`bash --norc` and asserts what is actually defined after a `cd`. Nothing else proves the dispatcher works, so any change to `shell::render` belongs there. Each test skips itself when the shell is absent, so a green run on a machine without `bash` proves less than it looks; the zsh cases always run on macOS. `tests/sync.rs` exercises `run_sync` against a local bare repo with `GIT_CONFIG_GLOBAL=/dev/null` and `GIT_CONFIG_NOSYSTEM=1` so the developer's own git config cannot change the result.
