//! Command-line interface: argument parsing and non-interactive handlers.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{ArgGroup, Args, Parser, Subcommand};

use crate::app::{App, Selection, now_timestamp};
use crate::backup;
use crate::capture;
use crate::model::{Entry, EntryKey, Kind};
use crate::paths::{self, Paths};
use crate::scopes::Placement;
use crate::search::{Filter, ScopeFilter};
use crate::shell::{self, Shell};

#[derive(Parser)]
#[command(
    name = "cmd-man",
    version,
    about = "Interactive manager for shell aliases and functions"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Add a new alias or function.
    Add(AddArgs),
    /// Guided creation of a new alias or function.
    New,
    /// Search stored entries.
    Search {
        /// Words to search for.
        query: Vec<String>,
    },
    /// List stored entries, optionally filtered. Defaults to the entries that
    /// are live in the current directory.
    #[command(group = ArgGroup::new("scope_filter").multiple(false))]
    List {
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        /// List entries from every scope, not just the live ones.
        #[arg(long, group = "scope_filter")]
        all: bool,
        /// List only the entries in this scope id, or `global` for unscoped ones.
        #[arg(long = "in", value_name = "ID", group = "scope_filter")]
        in_scope: Option<String>,
    },
    /// Edit fields of an existing entry.
    Edit(EditArgs),
    /// Remove an entry.
    Rm {
        name: String,
        #[command(flatten)]
        selection: SelectionArgs,
    },
    /// Capture example output for an entry.
    Capture {
        name: String,
        /// Skip the confirmation prompt for flagged commands.
        #[arg(long)]
        yes: bool,
        #[command(flatten)]
        selection: SelectionArgs,
    },
    /// Inspect and repair directory scopes.
    Scope {
        #[command(subcommand)]
        command: ScopeCommand,
    },
    /// Import currently active shell aliases and functions.
    Import {
        #[arg(long)]
        yes: bool,
    },
    /// Install the rc source blocks and generate shell files.
    Init,
    /// Regenerate shell files and print how to reload them.
    Reload,
    /// Back up the store to GitHub.
    Backup,
    /// Restore the store from the GitHub backup.
    Restore,
}

#[derive(Subcommand)]
enum ScopeCommand {
    /// List directory scopes.
    List,
    /// Point a scope at a different directory, creating it if it is unknown.
    Relocate {
        /// The scope id to point somewhere new.
        id: String,
        /// The directory the scope covers from now on.
        path: PathBuf,
    },
    /// Remove a scope. Its entries are promoted to global or deleted.
    #[command(group = ArgGroup::new("disposition").multiple(false).required(true))]
    Rm {
        id: String,
        /// Keep the entries by making them global.
        #[arg(long, group = "disposition")]
        promote_to_global: bool,
        /// Delete the entries along with the scope.
        #[arg(long, group = "disposition")]
        delete_entries: bool,
    },
}

#[derive(Args)]
#[command(group = ArgGroup::new("placement").multiple(false))]
struct AddArgs {
    /// The alias the user will type (e.g. gitsw).
    name: String,
    #[arg(long, short = 'c')]
    command: Option<String>,
    /// alias (default) or function.
    #[arg(long)]
    kind: Option<String>,
    #[arg(long)]
    desc: Option<String>,
    #[arg(long)]
    category: Option<String>,
    #[arg(long = "tag")]
    tags: Vec<String>,
    #[arg(long)]
    usage: Option<String>,
    /// Capture example output immediately after adding.
    #[arg(long)]
    capture: bool,
    /// Do not auto-fill description/usage/options/examples from --help.
    #[arg(long)]
    no_help: bool,
    /// Place the entry in an existing scope by id.
    #[arg(long, group = "placement")]
    scope: Option<String>,
    /// Scope the entry to this directory, creating a scope if needed.
    #[arg(long, group = "placement")]
    here: bool,
    /// Force a global entry regardless of the configured default.
    #[arg(long, group = "placement")]
    global: bool,
}

/// Which entry a command means when a name exists in more than one place.
#[derive(Args)]
#[command(group = ArgGroup::new("selection").multiple(false))]
struct SelectionArgs {
    /// Act on the entry in this scope id, or `global` for the unscoped one.
    #[arg(long = "in", value_name = "ID", group = "selection")]
    in_scope: Option<String>,
    /// Act only on an entry in a scope covering this directory.
    #[arg(long, group = "selection")]
    current_scope: bool,
}

impl SelectionArgs {
    fn selection(&self) -> Selection<'_> {
        match (&self.in_scope, self.current_scope) {
            (Some(id), _) => Selection::In(id),
            (None, true) => Selection::CurrentScope,
            (None, false) => Selection::Auto,
        }
    }
}

#[derive(Args)]
#[command(group = ArgGroup::new("move").multiple(false))]
struct EditArgs {
    name: String,
    #[arg(long, short = 'c')]
    command: Option<String>,
    #[arg(long)]
    kind: Option<String>,
    #[arg(long)]
    desc: Option<String>,
    #[arg(long)]
    category: Option<String>,
    #[arg(long = "tag")]
    tags: Vec<String>,
    #[arg(long)]
    usage: Option<String>,
    /// New alias for the entry.
    #[arg(long)]
    rename: Option<String>,
    /// Do not auto-fill empty description/usage/options/examples from --help.
    #[arg(long)]
    no_help: bool,
    /// Move the entry into this existing scope id.
    #[arg(long = "move-to", value_name = "ID", group = "move")]
    move_to: Option<String>,
    /// Move the entry into this directory's scope, creating one if needed.
    #[arg(long, group = "move")]
    move_here: bool,
    /// Move the entry out of its scope and make it global.
    #[arg(long, group = "move")]
    make_global: bool,
    #[command(flatten)]
    selection: SelectionArgs,
}

impl EditArgs {
    /// The move the flags ask for, or `None` to keep the current scope.
    fn placement(&self) -> Option<Placement> {
        match (&self.move_to, self.move_here, self.make_global) {
            (Some(id), _, _) => Some(Placement::Scope(id.clone())),
            (None, true, _) => Some(Placement::Here),
            (None, false, true) => Some(Placement::Global),
            (None, false, false) => None,
        }
    }
}

/// Parse arguments and run the requested command (or the TUI).
pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::resolve().ok();

    // Report a previous background sync failure once, without blocking anything.
    if let Some(paths) = &paths
        && let Some(notice) = backup::take_failure_notice(paths)
    {
        eprintln!("{notice}");
    }

    let result = match cli.command {
        None => crate::tui::run(),
        Some(cmd) => dispatch(cmd),
    };

    // Kick off this invocation's sync after the command has settled on disk, so a
    // single background pass covers the pull and any pushes from this run.
    if let Some(paths) = &paths {
        backup::spawn_sync(paths);
    }
    result
}

fn dispatch(cmd: Command) -> Result<()> {
    match cmd {
        Command::Add(args) => cmd_add(args),
        Command::New => cmd_new(),
        Command::Search { query } => cmd_search(&query.join(" ")),
        Command::List {
            category,
            kind,
            all,
            in_scope,
        } => cmd_list(category, kind, all, in_scope),
        Command::Edit(args) => cmd_edit(args),
        Command::Rm { name, selection } => cmd_rm(&name, &selection),
        Command::Capture {
            name,
            yes,
            selection,
        } => cmd_capture(&name, yes, &selection),
        Command::Scope { command } => dispatch_scope(command),
        Command::Import { yes } => cmd_import(yes),
        Command::Init => cmd_init(),
        Command::Reload => cmd_reload(),
        Command::Backup => cmd_backup(),
        Command::Restore => cmd_restore(),
    }
}

/// Load state and report anything non-fatal that was found on the way.
fn load_app() -> Result<App> {
    let app = App::load()?;
    for notice in app.notices() {
        eprintln!("cmd-man: {notice}");
    }
    Ok(app)
}

fn parse_kind(raw: Option<String>) -> Result<Kind> {
    match raw {
        None => Ok(Kind::Alias),
        Some(s) => s.parse().map_err(|e: String| anyhow::anyhow!(e)),
    }
}

fn cmd_add(args: AddArgs) -> Result<()> {
    let mut app = load_app()?;
    let kind = parse_kind(args.kind)?;

    let command = match args.command {
        Some(c) => c,
        None => prompt_required("Command")?,
    };

    let mut entry = Entry::new(&args.name, kind, command, String::new());
    if let Some(cat) = args.category {
        entry.category = cat;
    }
    entry.tags = args.tags;
    if let Some(u) = args.usage {
        entry.usage = u;
    }
    if let Some(d) = args.desc {
        entry.description = d;
    }

    if !args.no_help {
        println!("Reading help for the command...");
        app.fill_from_help(&mut entry);
    }
    // Description is required; help may have supplied it, otherwise prompt.
    if entry.description.trim().is_empty() {
        entry.description = prompt_required("Description")?;
    }

    let placement = if args.here {
        Placement::Here
    } else if args.global {
        Placement::Global
    } else if let Some(id) = args.scope {
        Placement::Scope(id)
    } else {
        Placement::Default
    };

    let key = app.add(entry, &placement)?;
    match &key.scope {
        Some(_) => println!(
            "Added '{}' scoped to {}.",
            key.name,
            app.placement_label(&key)
        ),
        None => println!("Added '{}'.", key.name),
    }
    print_reload_hint(&app);

    if args.capture {
        do_capture(&mut app, &key, false)?;
    }
    Ok(())
}

fn cmd_new() -> Result<()> {
    let mut app = load_app()?;
    let name = prompt_required("Alias")?;
    let kind = loop {
        let raw = prompt("Kind [alias/function] (alias)");
        let raw = if raw.trim().is_empty() {
            "alias".to_string()
        } else {
            raw
        };
        match raw.parse::<Kind>() {
            Ok(k) => break k,
            Err(e) => println!("{e}"),
        }
    };
    let command = prompt_required(if kind == Kind::Function {
        "Function body"
    } else {
        "Command"
    })?;
    let description = prompt_required("Description")?;
    let category = prompt("Category (general)");
    let tags = prompt("Tags (comma separated)");
    let usage = prompt("Usage (optional)");
    let placement = loop {
        let candidate = Placement::parse(&prompt("Scope [global/here/<id>] (default)"));
        match app.resolve_target(&candidate) {
            Ok(_) => break candidate,
            Err(e) => println!("{e}. Known scopes: {}", app.scope_ids_label()),
        }
    };

    let mut entry = Entry::new(&name, kind, command, description);
    if !category.trim().is_empty() {
        entry.category = category.trim().to_string();
    }
    entry.tags = split_tags(&tags);
    entry.usage = usage.trim().to_string();

    println!("Reading help for the command...");
    app.fill_from_help(&mut entry);

    let key = app.add(entry, &placement)?;
    match &key.scope {
        Some(_) => println!(
            "Added '{}' scoped to {}.",
            key.name,
            app.placement_label(&key)
        ),
        None => println!("Added '{}'.", key.name),
    }
    print_reload_hint(&app);
    Ok(())
}

fn cmd_search(query: &str) -> Result<()> {
    let app = load_app()?;
    // Search spans every scope so a name is never impossible to find from the
    // wrong directory.
    let filter = Filter {
        scope: ScopeFilter::All,
        ..Filter::default()
    };
    let results = app.search(query, &filter);
    if results.is_empty() {
        println!("No matches.");
        return Ok(());
    }
    for e in results {
        println!("{}  [{}]  {}", EntryKey::of(e).label(), e.kind, e.command);
        if !e.description.is_empty() {
            println!("    {}", e.description);
        }
    }
    Ok(())
}

fn cmd_list(
    category: Option<String>,
    kind: Option<String>,
    all: bool,
    in_scope: Option<String>,
) -> Result<()> {
    let app = load_app()?;
    let scope = if all {
        ScopeFilter::All
    } else if let Some(id) = in_scope {
        match id.as_str() {
            "global" => ScopeFilter::Only(None),
            _ if app.scopes.find(&id).is_none() => {
                bail!("unknown scope '{id}' (known: {})", app.scope_ids_label())
            }
            _ => ScopeFilter::Only(Some(id)),
        }
    } else {
        ScopeFilter::ActiveIn(
            app.scopes
                .active(&app.cwd)
                .iter()
                .map(|s| s.id.clone())
                .collect(),
        )
    };
    let filter = Filter {
        category: category.map(|c| crate::store::normalize_category(&c)),
        kind: match kind {
            Some(k) => Some(k.parse().map_err(|e: String| anyhow::anyhow!(e))?),
            None => None,
        },
        scope,
    };
    let results = app.search("", &filter);
    if results.is_empty() {
        println!("No entries.");
        return Ok(());
    }
    let mut current = String::new();
    for e in results {
        if e.category != current {
            current = e.category.clone();
            println!("\n{current}");
        }
        println!("  {}  [{}]  {}", EntryKey::of(e).label(), e.kind, e.command);
    }
    Ok(())
}

fn cmd_edit(args: EditArgs) -> Result<()> {
    let mut app = load_app()?;
    let key = app.resolve_key(&args.name, args.selection.selection())?;
    let placement = args.placement();
    let mut entry = app
        .store
        .find(&key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no entry '{}'", key.label()))?;

    if let Some(c) = args.command {
        entry.command = c;
    }
    if let Some(k) = args.kind {
        entry.kind = k.parse().map_err(|e: String| anyhow::anyhow!(e))?;
    }
    if let Some(d) = args.desc {
        entry.description = d;
    }
    if let Some(cat) = args.category {
        entry.category = cat;
    }
    if !args.tags.is_empty() {
        entry.tags = args.tags;
    }
    if let Some(u) = args.usage {
        entry.usage = u;
    }
    if let Some(new_name) = args.rename {
        entry.name = new_name;
    }

    if !args.no_help {
        app.fill_from_help(&mut entry);
    }

    let new_key = app.update(&key, entry, placement.as_ref())?;
    println!("Updated '{}'.", new_key.label());
    print_reload_hint(&app);
    Ok(())
}

fn cmd_rm(name: &str, selection: &SelectionArgs) -> Result<()> {
    let mut app = load_app()?;
    let key = app.resolve_key(name, selection.selection())?;
    app.remove(&key)?;
    println!("Removed '{}'.", key.label());
    print_reload_hint(&app);
    Ok(())
}

fn cmd_capture(name: &str, yes: bool, selection: &SelectionArgs) -> Result<()> {
    let mut app = load_app()?;
    let key = app.resolve_key(name, selection.selection())?;
    do_capture(&mut app, &key, yes)
}

fn do_capture(app: &mut App, key: &EntryKey, yes: bool) -> Result<()> {
    let label = key.label();
    let entry = app
        .store
        .find(key)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no entry '{label}'"))?;

    let assessment = app.assess(&entry);
    if assessment.is_blocked() {
        println!("Capture is disabled for '{label}'.");
        return Ok(());
    }
    if assessment.needs_confirmation() && !yes {
        println!("Command: {}", entry.command);
        if !assessment.reasons.is_empty() {
            println!("Flagged: {}", assessment.reasons.join("; "));
        }
        // Gather reference intel about the underlying command to help the user
        // decide, using its --help/-h output.
        if let Some(binary) = capture::primary_binary(&entry.command)
            && let Some(intel) = capture::gather_intel(&binary)
        {
            println!("About '{binary}':");
            println!("{intel}");
        }
        if !std::io::stdin().is_terminal() {
            bail!("'{label}' needs confirmation; re-run with --yes to capture");
        }
        if !confirm(&format!("Run '{label}' to capture output?")) {
            println!("Skipped.");
            return Ok(());
        }
    }

    let result = app.capture(&entry)?;
    if result.timed_out {
        println!("Command timed out; captured partial output.");
    }
    println!("Captured via {} backend.", result.backend);
    app.record_capture(
        key,
        result.output.trim().to_string(),
        assessment.destructive,
    )?;
    println!("Saved example output for '{label}'.");
    Ok(())
}

fn dispatch_scope(cmd: ScopeCommand) -> Result<()> {
    match cmd {
        ScopeCommand::List => cmd_scope_list(),
        ScopeCommand::Relocate { id, path } => cmd_scope_relocate(&id, &path),
        ScopeCommand::Rm {
            id,
            promote_to_global,
            ..
        } => cmd_scope_rm(&id, promote_to_global),
    }
}

fn cmd_scope_list() -> Result<()> {
    let app = load_app()?;
    let summaries = app.scope_summaries();
    if summaries.is_empty() {
        println!("No scopes.");
        return Ok(());
    }
    let width = summaries.iter().map(|s| s.id.len()).max().unwrap_or(0);
    for summary in &summaries {
        let mut line = format!(
            "{:width$}  {}  {}",
            summary.id,
            summary.path,
            count_label(summary.entries)
        );
        if summary.active {
            line.push_str("  (active)");
        }
        if !summary.exists {
            line.push_str("  (missing)");
        }
        // The dispatcher tests the shell's logical `$PWD`, so a scope stored
        // under a resolved path stays inactive in a directory reached through
        // a symlink. Nothing else in the listing explains that.
        if let Some(cwd) = &summary.inactive_via_symlink {
            line.push_str(&format!(
                "  (inactive here: $PWD is {}, which reaches it through a symlink)",
                paths::portable_path(cwd)
            ));
        }
        println!("{line}");
    }
    Ok(())
}

fn cmd_scope_relocate(id: &str, path: &Path) -> Result<()> {
    let mut app = load_app()?;
    let id = id.trim();
    app.relocate_scope(id, path)?;
    let stored = app
        .scopes
        .find(id)
        .map(|s| s.path.clone())
        .unwrap_or_else(|| path.display().to_string());
    println!("Scope '{id}' now covers {stored}.");
    print_reload_hint(&app);
    Ok(())
}

fn cmd_scope_rm(id: &str, promote_to_global: bool) -> Result<()> {
    let mut app = load_app()?;
    if promote_to_global {
        let moved = app.remove_scope_promoting(id)?;
        println!(
            "Removed scope '{id}' and promoted {} to global.",
            count_label(moved)
        );
    } else {
        let deleted = app.remove_scope_deleting(id)?;
        println!("Removed scope '{id}' and deleted {}.", count_label(deleted));
    }
    print_reload_hint(&app);
    Ok(())
}

fn count_label(count: usize) -> String {
    if count == 1 {
        "1 entry".to_string()
    } else {
        format!("{count} entries")
    }
}

fn cmd_import(yes: bool) -> Result<()> {
    let mut app = load_app()?;
    let shell = detect_user_shell();
    println!(
        "Scanning active {} aliases and functions...",
        shell.as_str()
    );

    let aliases = shell::collect_active_aliases(shell)?;
    let functions = shell::collect_active_functions(shell)?;
    let known = |n: &str| !app.store.find_all_named(n).is_empty();
    let mut candidates = shell::importable_entries(&aliases, Kind::Alias, &known);
    candidates.extend(shell::importable_entries(
        &functions,
        Kind::Function,
        &known,
    ));

    if candidates.is_empty() {
        println!("No new aliases or functions to import.");
        return Ok(());
    }
    println!("Found {} unmanaged definition(s).", candidates.len());

    let mut imported = 0;
    for entry in candidates {
        let accept = yes
            || confirm(&format!(
                "Import {} '{}' = {}?",
                entry.kind, entry.name, entry.command
            ));
        if accept {
            let name = entry.name.clone();
            match app.store.add(entry) {
                Ok(()) => imported += 1,
                Err(e) => eprintln!("Skipped '{name}': {e}"),
            }
        }
    }
    if imported > 0 {
        app.persist()?;
    }
    println!("Imported {imported} definition(s).");
    print_reload_hint(&app);
    Ok(())
}

fn cmd_init() -> Result<()> {
    let app = load_app()?;
    app.regenerate_shells()?;
    let report = shell::install_rc(&app.paths, app.config.shells.zsh, app.config.shells.bash)?;
    if report.touched.is_empty() {
        println!("rc files already reference cmd-man.");
    } else {
        for path in &report.touched {
            println!("Updated {}", path.display());
        }
    }
    print_reload_hint(&app);
    Ok(())
}

fn cmd_reload() -> Result<()> {
    let app = load_app()?;
    app.regenerate_shells()?;
    println!("Regenerated shell files.");
    print_reload_hint(&app);
    Ok(())
}

fn cmd_backup() -> Result<()> {
    let app = load_app()?;
    let report = backup::run_backup(&app.paths, &app.config, &now_timestamp())?;
    for msg in &report.messages {
        println!("{msg}");
    }
    if report.committed {
        println!("Committed a new snapshot.");
    } else {
        println!("No changes to commit.");
    }
    if report.pushed {
        println!("Backup complete via {} method.", method_name(report.method));
    }
    Ok(())
}

fn cmd_restore() -> Result<()> {
    let app = load_app()?;
    let messages = backup::run_restore(&app.paths, &app.config)?;
    for msg in &messages {
        println!("{msg}");
    }
    // Regenerate shell files from the restored store.
    let app = load_app()?;
    app.regenerate_shells()?;
    println!("Restored store and regenerated shell files.");
    print_reload_hint(&app);
    Ok(())
}

fn method_name(method: crate::config::BackupMethod) -> &'static str {
    use crate::config::BackupMethod::*;
    match method {
        Gh => "gh CLI",
        Oauth => "OAuth",
        Git => "git remote",
    }
}

fn detect_user_shell() -> Shell {
    match std::env::var("SHELL") {
        Ok(s) if s.contains("bash") => Shell::Bash,
        _ => Shell::Zsh,
    }
}

fn print_reload_hint(app: &App) {
    if let Some(file) = shell_hint_path(app) {
        println!("Run `source {file}` (or open a new shell) to load changes now.");
    }
}

fn shell_hint_path(app: &App) -> Option<String> {
    let shell = detect_user_shell();
    let use_shell = match shell {
        Shell::Zsh => app.config.shells.zsh,
        Shell::Bash => app.config.shells.bash,
    };
    let shell = if use_shell {
        shell
    } else if app.config.shells.zsh {
        Shell::Zsh
    } else if app.config.shells.bash {
        Shell::Bash
    } else {
        return None;
    };
    Some(app.paths.shell_file(shell).display().to_string())
}

fn split_tags(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn prompt(label: &str) -> String {
    print!("{label}: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    line.trim_end_matches(['\n', '\r']).to_string()
}

fn prompt_required(label: &str) -> Result<String> {
    let value = prompt(label);
    if value.trim().is_empty() {
        bail!("{label} is required");
    }
    Ok(value)
}

fn confirm(question: &str) -> bool {
    let answer = prompt(&format!("{question} [y/N]"));
    matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_tags_trims_and_filters() {
        assert_eq!(split_tags("a, b ,,c"), vec!["a", "b", "c"]);
        assert!(split_tags("   ").is_empty());
    }

    #[test]
    fn count_label_pluralizes() {
        assert_eq!(count_label(0), "0 entries");
        assert_eq!(count_label(1), "1 entry");
        assert_eq!(count_label(2), "2 entries");
    }

    #[test]
    fn parse_kind_defaults_to_alias() {
        assert_eq!(parse_kind(None).unwrap(), Kind::Alias);
        assert_eq!(parse_kind(Some("function".into())).unwrap(), Kind::Function);
        assert!(parse_kind(Some("nope".into())).is_err());
    }
}
