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

The dependency spine is: `cli`/`tui` -> `app::App` -> (`store`, `shell`, `capture`, `backup`), all rooted at `paths::Paths`.

- **`app::App` is the service layer and the only correct place to mutate state.** It holds `paths`, `config`, and `store`. Every mutation (`add`/`update`/`remove`/`record_capture`) calls `persist()`, which saves the store, regenerates the shell definition files so the two never drift, AND kicks off a background sync (`backup::spawn_sync`). Both the CLI (`cli.rs`) and the TUI (`tui/`) go through `App` rather than touching `Store` or `shell` directly. If you add a mutation path, route it through `App`.

- **`store`** loads all `categories/*.toml` files (each a list of `[[entry]]` tables) into one flat `Vec<Entry>` with globally unique names. `model::Entry`/`Kind`/`CapturePolicy` are the core data types.

- **`paths::Paths`** resolves the data root: `CMD_MAN_HOME` (tests and power users) > `XDG_CONFIG_HOME/cmd-man` > `~/.config/cmd-man`. Layout: `config.toml`, `categories/`, and generated `shell/`.

- **`shell`** renders entries into `shell/cmd-man.zsh` and `shell/cmd-man.bash` (marked "Do not edit by hand", regenerated on every change), installs a managed `# >>> cmd-man >>>` block into rc files (`.zshrc`, `.bashrc`/`.bash_profile`), and imports the user's currently active aliases/functions.

- **`capture`** is the safe example-output pipeline: `classifier` statically flags destructive binaries/verbs, interpreters, and side-effecting verbs; `runner` executes; `sanitize` redacts home paths, usernames, IPs, emails, and token-shaped strings. Auto-fill of description/usage/options/examples from a command's `--help`/`man` output lives in `help.rs` and never overwrites populated fields.

- **`backup`** pushes `categories/` and `config.toml` (generated `shell/` is gitignored out) to a private GitHub repo across three tiers chosen by `plan.rs` from config + runtime availability: `gh` CLI, built-in OAuth device flow (token in the OS keyring), or a plain git remote. `cmd-man backup`/`restore` are the manual paths; `backup/sync.rs` adds automatic sync (see below).

## Automatic background sync

`backup/sync.rs` keeps the store backed up with no manual step. Every invocation spawns one short-lived, detached, fire-and-die process (`spawn_sync` -> the hidden `__sync-exec` marker -> `sync_exec_main`) that runs `run_sync`: commit any pending local changes, pull, regenerate shell files if the pull changed anything, and push. It never blocks the foreground command, which always operates on the state it loaded; the background pull lands for the next invocation. Concurrency is deduped by an advisory lock (`.sync.lock`), and the outcome is written to `.sync-state.toml`; a failure is surfaced once (tracked via `.sync-notified`) as a one-line stderr notice on the next command.

- Activation is automatic whenever a remote is resolvable, preferred in order by `resolve_sync_remote`: `backup.remote_url`, then an authenticated `gh`, then a stored OAuth token. The interactive OAuth device flow never runs in the silent process; authorize once via `cmd-man backup`, then sync reuses the token. Opt out with `backup.auto_sync = false`.
- On a fresh machine (empty local store, populated remote reachable) `run_sync` auto-clones the backup and regenerates shells, so aliases travel with zero setup. Bootstrap only clones into an empty store, so it never clobbers local-only entries.
- `run_sync` is synchronous and holds all the logic, so it is tested offline against a local bare repo (`tests/sync.rs`). `spawn_sync` is a no-op under `cfg!(test)` or when `CMD_MAN_DISABLE_SYNC` is set; the `tests/cli.rs` harness sets that env so binary-driven tests never reach a real remote.

## Capture sandbox: the re-exec trick

`main.rs` checks argv for the hidden `__capture-exec` marker BEFORE any clap parsing and, if present, immediately runs `capture::capture_exec_main()`. This matters because the sandbox (the `birdcage` crate: denies network, denies writes outside temp dirs) must be applied to a dedicated, single-threaded process. `runner::run_capture` re-execs the same binary with that marker, passing the command/timeout/sandbox flags via env vars. Every run is hard-timeboxed with `wait_timeout` and the child leads its own process group so the whole subtree is killed on timeout. If sandbox setup fails it exits with a sentinel code (`SANDBOX_UNAVAILABLE_CODE`) and the caller transparently falls back to an unsandboxed timeboxed run. Do not add work before the `__capture-exec` check in `main.rs`, and keep that path single-threaded. The `__sync-exec` marker (background sync) is checked right after it; it is not sandboxed, so it may run after the capture check but must stay before clap parsing.

## Testing notes

`tests/cli.rs` drives the real compiled binary via `assert_cmd`, isolating each run with a `TempDir` and `CMD_MAN_HOME` pointing at it. Use `CMD_MAN_HOME` to point unit or manual runs at a throwaway store instead of the real `~/.config/cmd-man`.
