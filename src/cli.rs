//! Command-line interface: argument parsing and non-interactive handlers.

use std::io::{IsTerminal, Write};

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::app::{App, now_timestamp};
use crate::backup;
use crate::capture;
use crate::model::{Entry, Kind};
use crate::paths::Paths;
use crate::search::Filter;
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
    /// List stored entries, optionally filtered.
    List {
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        kind: Option<String>,
    },
    /// Edit fields of an existing entry.
    Edit(EditArgs),
    /// Remove an entry.
    Rm { name: String },
    /// Capture example output for an entry.
    Capture {
        name: String,
        /// Skip the confirmation prompt for flagged commands.
        #[arg(long)]
        yes: bool,
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

#[derive(Args)]
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
}

#[derive(Args)]
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
        Command::List { category, kind } => cmd_list(category, kind),
        Command::Edit(args) => cmd_edit(args),
        Command::Rm { name } => cmd_rm(&name),
        Command::Capture { name, yes } => cmd_capture(&name, yes),
        Command::Import { yes } => cmd_import(yes),
        Command::Init => cmd_init(),
        Command::Reload => cmd_reload(),
        Command::Backup => cmd_backup(),
        Command::Restore => cmd_restore(),
    }
}

fn parse_kind(raw: Option<String>) -> Result<Kind> {
    match raw {
        None => Ok(Kind::Alias),
        Some(s) => s.parse().map_err(|e: String| anyhow::anyhow!(e)),
    }
}

fn cmd_add(args: AddArgs) -> Result<()> {
    let mut app = App::load()?;
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

    app.add(entry)?;
    println!("Added '{}'.", args.name);
    print_reload_hint(&app);

    if args.capture {
        do_capture(&mut app, &args.name, false)?;
    }
    Ok(())
}

fn cmd_new() -> Result<()> {
    let mut app = App::load()?;
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

    let mut entry = Entry::new(&name, kind, command, description);
    if !category.trim().is_empty() {
        entry.category = category.trim().to_string();
    }
    entry.tags = split_tags(&tags);
    entry.usage = usage.trim().to_string();

    println!("Reading help for the command...");
    app.fill_from_help(&mut entry);

    app.add(entry)?;
    println!("Added '{name}'.");
    print_reload_hint(&app);
    Ok(())
}

fn cmd_search(query: &str) -> Result<()> {
    let app = App::load()?;
    let results = app.search(query, &Filter::default());
    if results.is_empty() {
        println!("No matches.");
        return Ok(());
    }
    for e in results {
        println!("{}  [{}]  {}", e.name, e.kind, e.command);
        if !e.description.is_empty() {
            println!("    {}", e.description);
        }
    }
    Ok(())
}

fn cmd_list(category: Option<String>, kind: Option<String>) -> Result<()> {
    let app = App::load()?;
    let filter = Filter {
        category: category.map(|c| crate::store::normalize_category(&c)),
        kind: match kind {
            Some(k) => Some(k.parse().map_err(|e: String| anyhow::anyhow!(e))?),
            None => None,
        },
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
        println!("  {}  [{}]  {}", e.name, e.kind, e.command);
    }
    Ok(())
}

fn cmd_edit(args: EditArgs) -> Result<()> {
    let mut app = App::load()?;
    let mut entry = app
        .store
        .find(&args.name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no entry named '{}'", args.name))?;

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

    let target = args.name.clone();
    app.update(&target, entry)?;
    println!("Updated '{target}'.");
    print_reload_hint(&app);
    Ok(())
}

fn cmd_rm(name: &str) -> Result<()> {
    let mut app = App::load()?;
    app.remove(name)?;
    println!("Removed '{name}'.");
    print_reload_hint(&app);
    Ok(())
}

fn cmd_capture(name: &str, yes: bool) -> Result<()> {
    let mut app = App::load()?;
    do_capture(&mut app, name, yes)
}

fn do_capture(app: &mut App, name: &str, yes: bool) -> Result<()> {
    let entry = app
        .store
        .find(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no entry named '{name}'"))?;

    let assessment = app.assess(&entry);
    if assessment.is_blocked() {
        println!("Capture is disabled for '{name}'.");
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
            bail!("'{name}' needs confirmation; re-run with --yes to capture");
        }
        if !confirm(&format!("Run '{name}' to capture output?")) {
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
        name,
        result.output.trim().to_string(),
        assessment.destructive,
    )?;
    println!("Saved example output for '{name}'.");
    Ok(())
}

fn cmd_import(yes: bool) -> Result<()> {
    let mut app = App::load()?;
    let shell = detect_user_shell();
    println!(
        "Scanning active {} aliases and functions...",
        shell.as_str()
    );

    let aliases = shell::collect_active_aliases(shell)?;
    let functions = shell::collect_active_functions(shell)?;
    let known = |n: &str| app.store.find(n).is_some();
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
    let app = App::load()?;
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
    let app = App::load()?;
    app.regenerate_shells()?;
    println!("Regenerated shell files.");
    print_reload_hint(&app);
    Ok(())
}

fn cmd_backup() -> Result<()> {
    let app = App::load()?;
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
    let app = App::load()?;
    let messages = backup::run_restore(&app.paths, &app.config)?;
    for msg in &messages {
        println!("{msg}");
    }
    // Regenerate shell files from the restored store.
    let app = App::load()?;
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
    fn parse_kind_defaults_to_alias() {
        assert_eq!(parse_kind(None).unwrap(), Kind::Alias);
        assert_eq!(parse_kind(Some("function".into())).unwrap(), Kind::Function);
        assert!(parse_kind(Some("nope".into())).is_err());
    }
}
