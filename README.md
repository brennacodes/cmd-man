# Command Man (cmd-man)

Command Man is an interactive CLI for creating, browsing, and managing your shell
aliases and functions. Entries you add become real shell definitions, so a stored
`gitsw` works exactly like an alias you wrote by hand: you type `gitsw`, not
`cmd-man gitsw`.

## Features

- Full-screen keyboard TUI: browse categories, page through entries, fuzzy search,
  and filter by kind (alias or function) or category.
- Live shell integration for zsh and bash, so aliases and functions behave natively.
- Aliases and functions are both first-class, tagged by kind so you can parse your
  catalog by type.
- Function bodies are checked before they are saved. One your shell could not read
  is refused with the reason, so a broken entry never reaches your shell.
- Directory scopes: an entry can belong to a project directory, so that project's
  `deploy` is live only inside it and shadows any global entry of the same name.
- Categories, descriptions, usage, options, examples, and tags on every entry.
- Description, usage, options, and examples auto-populate from the command's
  `--help`/`man` output when possible, and are always editable by hand.
- Safe example-output capture: commands run sandboxed and hard-timeboxed, and the
  output is sanitized before it is stored.
- Automatic background sync, plus manual backup and restore, to a private GitHub
  `cmd-man-backup` repository.

## Requirements

- macOS or Linux.
- `git` for backups. Optionally the GitHub CLI (`gh`) for the simplest backup path.

## Install

Install the latest prebuilt binary for macOS or Linux (x86_64 or arm64):

```sh
curl -fsSL https://raw.githubusercontent.com/brennacodes/cmd-man/main/dist/install.sh | sh
```

The binary goes into `~/.local/bin`; set `CMD_MAN_INSTALL_DIR` to put it
elsewhere, or `CMD_MAN_VERSION` to install a specific release. Every release's
binaries are also attached to its page on
[GitHub Releases](https://github.com/brennacodes/cmd-man/releases).

To build from source instead, install directly from GitHub with Cargo:

```sh
cargo install --git https://github.com/brennacodes/cmd-man.git
```

Or build from a local clone:

```sh
cargo install --path .
```

Then wire cmd-man into your shells:

```sh
cmd-man init
```

Open a new shell, or run the `source` command that `init` prints, to load your
entries into the current shell. After that, every change takes effect in new
shells automatically, or in the current one with `cmd-man reload`.

## Usage

Launch the interactive TUI:

```sh
cmd-man
```

### TUI keys

| Key | Action |
| --- | --- |
| `/` | fuzzy search |
| `j` / `k`, arrows | move selection |
| `PgUp` / `PgDn` | page through the list |
| `Tab` | cycle kind filter (all / alias / function) |
| `g` | cycle category filter |
| `s` | cycle scope filter (all / live here / one scope) |
| `a` | add a new entry |
| `e` | edit the selected entry |
| `d` | delete the selected entry |
| `c` | capture example output |
| `Ctrl-F` (in the add/edit form) | fetch help and fill empty fields |
| `r` | regenerate shell files |
| `b` | back up to GitHub |
| `?` | help |
| `q` / `Esc` | quit |

### Command line

#### Add an alias
```sh
cmd-man add gitsw --command "git switch" --desc "switch branches" --category git
```

#### Add a function
```sh
cmd-man add mkcd --kind function --command 'mkdir -p "$1" && cd "$1"' --desc "make and enter a dir"
```

#### Guided creation
```sh
cmd-man new
```

#### Add without auto-filling fields from --help
```sh
cmd-man add gitsw --command "git switch" --desc "switch branches" --no-help
```

#### Search, list, edit, remove
```sh
cmd-man search switch
cmd-man list --kind function
cmd-man edit gitsw --desc "switch or create branches"
cmd-man rm gitsw
```

#### Scope an entry to the current project
```sh
cmd-man add dep --command "make deploy" --desc "deploy" --here
```

#### Move an existing entry into a scope, or back out of one
```sh
cmd-man edit dep --move-here
cmd-man edit dep --make-global
```

#### Work with a specific entry when a name exists more than once
```sh
cmd-man rm dep --in api
cmd-man list --all
cmd-man list --in global
```

#### Manage scopes
```sh
cmd-man scope list
cmd-man scope relocate api ~/work/api
cmd-man scope rm api --promote-to-global
```

#### Capture example output for an entry
```sh
cmd-man capture <alias>
```

#### Import aliases and functions already active in your shell
```sh
cmd-man import
```

#### Regenerate shell files and reload
```sh
cmd-man reload
```

#### Back up and restore
```sh
cmd-man backup
cmd-man restore
```

## Directory-scoped entries

A scope is a short id attached to one directory, for example `api` for
`~/work/api`. An entry placed in a scope is defined only while your shell's
working directory is inside that directory, and it shadows a global entry of the
same name while it is live. Leave the directory and the global definition comes
back. If scopes are nested, the deepest one containing your working directory
wins.

New entries are global unless you say otherwise. `--here` scopes an entry to the
enclosing git repository, or to the current directory when there is no repository,
creating a scope named after that directory the first time. `--scope <id>` places
it in a scope that already exists, and `--global` forces a global entry. Every add
tells you where the entry landed:

```sh
$ cmd-man add dep --command "make deploy" --desc "deploy" --here
Added 'dep' scoped to api ($HOME/work/api).
```

`cmd-man scope list` shows every scope, its directory, how many entries it holds,
and which one you are standing in. `cmd-man list` shows your globals plus the
entries of the scopes you are inside, so a global and the scoped entry shadowing
it both appear; `cmd-man list --all` shows every entry, and `cmd-man list --in api`
or `--in global` shows one scope's worth.

When a name exists in more than one place, `edit`, `rm`, and `capture` pick the
same entry your shell would run: the innermost scope you are inside, otherwise the
global one. If neither applies, cmd-man lists the candidates and asks you to choose
with `--in <id|global>`.

### Known limitations

- A scoped entry is not available on the same line as the `cd` that entered its
  scope, so `cd ~/work/api && dep` does not work. Use two lines, or `cd` first.
  In zsh a scoped function is the one exception.
- On bash, scoped entries are interactive-only and do not work in scripts.
- A scope matches one exact directory spelling, so reaching the directory through
  a differently-spelled symlink does not activate it, and on a case-insensitive
  macOS volume `cd /users/...` does not activate a scope stored as `/Users/...`.
  `cmd-man scope list` says so when your working directory reaches a scope only
  once symlinks are resolved.
- A scope whose directory does not exist on the machine you are on never
  activates. `cmd-man scope list` marks it `(missing)`, and
  `cmd-man scope relocate <id> <dir>` points it at the right place.
- A hand-typed alias of yours that collides with a cmd-man name is cleared when
  your shell loads cmd-man.

## Capturing example output

`cmd-man capture <name>` runs an entry and stores its output as an example. Safe
commands run automatically; anything destructive or with outward side effects (for
example `rm`, `reset`, `push`) needs confirmation first. Every capture is
sandboxed, hard-timeboxed, and sanitized, so home paths, usernames, IP addresses,
emails, and token-shaped strings never reach your store.

## Auto-filling from help

When you add or edit an entry, cmd-man reads the command's `--help` output and
fills any empty description, usage, options, or examples fields. It never
overwrites text you already entered. Press `Ctrl-F` in the TUI form to fetch on
demand, or pass `--no-help` to `add`/`edit` to skip the lookup.

## Sync and backups

Once a backup remote is reachable, cmd-man keeps your entries synced
automatically, in the background, without ever blocking you. A remote is reachable
when `backup.remote_url` is set, when `gh` is installed and authenticated, or when
a GitHub OAuth token is already stored. On a new machine, the first run clones your
existing backup and sets up your shell files, so your aliases travel with you.

Sync is on by default. Turn it off with `auto_sync = false` under `[backup]` in
your config, or set `CMD_MAN_DISABLE_SYNC=1` for a single run.

`cmd-man backup` and `cmd-man restore` are the manual equivalents. Both use a
private `cmd-man-backup` repository and pick the best available method for you;
you can also choose one explicitly in your config.

## Configuration

`~/.config/cmd-man/config.toml` controls enabled shells, capture timeout and
sandboxing, backup method and opt-outs, and default entry placement. The built-in
defaults apply until you create the file yourself, and any key you leave out keeps
its default. The file is part of your backup.

`scope.default` decides where `cmd-man add` puts a new entry when you pass no
placement flag. It is `global` by default:

```toml
[scope]
default = "repo"
```

In `repo` mode a new entry is scoped to the git repository you are in, and stays
global when there is no repository, or when the repository root would be your home
directory or the filesystem root.

## Contributing

Run cmd-man against a throwaway store instead of your real `~/.config/cmd-man`:
```sh
CMD_MAN_HOME=$(mktemp -d) cargo run -- list
```

CI runs fmt, clippy, and the tests on every pull request and every push to
`main`; run them locally before opening a pull request.

### Debug build

```sh
cargo build
```

### Unit tests and the end-to-end suites

```sh
cargo test
```

### Format

```sh
cargo fmt
```

### Lint

```sh
cargo clippy --all-targets
```
