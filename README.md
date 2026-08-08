# Command Man (cmd-man)

Command Man is an interactive CLI for creating, browsing, and managing your shell
aliases and functions. Entries you add become real shell definitions, so a stored
`gitsw` works exactly like an alias you wrote by hand: you type `gitsw`, not
`cmd-man gitsw`.

## Features

- Full-screen keyboard TUI: browse categories, page through entries, fuzzy search,
  and filter by kind (alias or function) or category.
- Live shell integration for zsh and bash. Every change regenerates a file your
  shell sources, so aliases and functions behave natively.
- Aliases and functions are both first-class, tagged by kind so you can parse your
  catalog by type.
- Categories, descriptions, usage, options, examples, and tags on every entry.
- Description, usage, options, and examples auto-populate from the command's
  `--help`/`man` output when possible, and are always editable by hand.
- Safe example-output capture: commands run in a hard-timeboxed, auto-killed
  process wrapped in a filesystem/network sandbox, and output is sanitized before
  it is stored.
- Regular backup and restore to a private GitHub `cmd-man-backup` repository.

## Requirements

- macOS or Linux.
- `git` for backups. Optionally the GitHub CLI (`gh`) for the simplest backup path.

## Install

Build from source with Cargo:

```sh
cargo install --path .
```

Then wire cmd-man into your shells (adds a small managed block to your rc files and
generates the definition files):

```sh
cmd-man init
```

Open a new shell, or run the `source` command that `init` prints, to load your
entries into the current shell.

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

```sh
# Add an alias
cmd-man add gitsw --command "git switch" --desc "switch branches" --category git

# Add a function
cmd-man add mkcd --kind function --command 'mkdir -p "$1" && cd "$1"' --desc "make and enter a dir"

# Guided creation
cmd-man new

# Add without auto-filling fields from --help
cmd-man add gitsw --command "git switch" --desc "switch branches" --no-help

# Search, list, edit, remove
cmd-man search switch
cmd-man list --kind function
cmd-man edit gitsw --desc "switch or create branches"
cmd-man rm gitsw

# Capture example output for an entry
cmd-man capture gitsw

# Import aliases and functions already active in your shell
cmd-man import

# Regenerate shell files and reload
cmd-man reload

# Back up and restore
cmd-man backup
cmd-man restore
```

## How aliases become live

cmd-man stores entries as human-readable TOML, one file per category, under
`~/.config/cmd-man/categories/`. On every change it regenerates
`~/.config/cmd-man/shell/cmd-man.zsh` and `cmd-man.bash` containing real `alias`
and function definitions. `cmd-man init` adds one managed block to your `.zshrc`
and `.bashrc`/`.bash_profile` that sources the matching file. New shells pick up
changes automatically; for the current shell, run the `source` command cmd-man
prints, or `cmd-man reload`.

## Capturing example output

When you capture output, cmd-man first classifies the command. Safe commands run
automatically. Anything that looks destructive or has outward side effects (for
example `rm`, `reset`, `push`, or verbs like delete, remove, rename, move, cut) is
flagged and requires confirmation. The command runs in a dedicated process with a
hard timeout that is killed when it expires, wrapped in a filesystem/network
sandbox, and the captured output is sanitized (home paths, usernames, IP
addresses, emails, and token-shaped strings are redacted) before being stored.

Capture that depends on your local context runs on your machine so it can see your
real tools and paths; the sandbox restricts writes and blocks network access as a
safety layer.

## Auto-filling from help

When you add or edit an entry, cmd-man reads the command's `--help`/`-h` output
(sandboxed and timeboxed, no pager) and fills any empty description, usage,
options, or examples fields by parsing the standard help sections. It never
overwrites text you already entered. In the TUI add/edit form, press `Ctrl-F` to
fetch on demand and review before saving. Pass `--no-help` to `add`/`edit`, or
leave the fields populated, to skip the lookup. Commands whose help does not
follow recognizable sections simply leave the fields blank for you to fill in.

## Backups

`cmd-man backup` commits your categories and config and pushes them to a private
`cmd-man-backup` repository, choosing the best available method automatically:

1. The GitHub CLI (`gh`) when it is installed and authenticated.
2. A built-in GitHub OAuth device flow, when a client id is configured via
   `CMD_MAN_GITHUB_CLIENT_ID` (the token is stored in your OS keychain).
3. A plain git remote using your existing credentials. If the repository does not
   exist yet, cmd-man prints a pre-filled link to create it.

You are never forced onto a method: you can disable higher tiers or pick one
explicitly in `~/.config/cmd-man/config.toml`. `cmd-man restore` pulls the backup
back down and regenerates your shell files.

## Configuration

`~/.config/cmd-man/config.toml` controls enabled shells, capture timeout and
sandboxing, and backup method and opt-outs. It is created with sensible defaults on
first use.
