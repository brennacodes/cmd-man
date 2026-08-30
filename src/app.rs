//! Service layer shared by the CLI and TUI: loads state and centralizes every
//! mutation so the store and generated shell files stay in sync.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::capture::{self, Assessment, CaptureResult};
use crate::config::Config;
use crate::model::{self, CapturePolicy, Entry, EntryKey};
use crate::paths::{self, Paths};
use crate::scopes::{self, Placement, Scope, ScopeRegistry, Target, one_line};
use crate::search::{Filter, search};
use crate::shell;
use crate::store::Store;

/// How a command names which entry it means when a name exists more than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selection<'a> {
    /// Prefer the entry the shell would run right now: the deepest active
    /// scope, then global, then a lone candidate anywhere.
    Auto,
    /// A named scope id, or `global` for the unscoped entry.
    In(&'a str),
    /// Only a scope covering the working directory.
    CurrentScope,
}

/// A registered scope as `cmd-man scope list` reports it.
#[derive(Debug, Clone)]
pub struct ScopeSummary {
    pub id: String,
    /// The directory as stored, `$HOME`-relative when it lives under home.
    pub path: String,
    pub resolved: PathBuf,
    pub entries: usize,
    pub exists: bool,
    pub active: bool,
    /// The working directory this scope contains only once the path is
    /// resolved, whether through a symlink or through a case difference on a
    /// case-insensitive volume. The dispatcher matches the shell's logical
    /// `$PWD` verbatim, so such a scope stays inactive even while the user
    /// stands inside it.
    pub inactive_until_resolved: Option<PathBuf>,
}

/// Where an entry's example output is captured from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureCwd {
    /// Run in the process working directory.
    Inherit,
    /// Run in this directory.
    In(PathBuf),
    /// The scope directory does not exist here; run in the process working
    /// directory.
    MissingScopeDir(PathBuf),
}

/// Loaded application state.
pub struct App {
    pub paths: Paths,
    pub config: Config,
    pub store: Store,
    pub scopes: ScopeRegistry,
    /// The working directory scope decisions are made against. A field rather
    /// than a `current_dir()` call at each use site so placement, selection,
    /// and filtering stay testable without mutating process state.
    pub cwd: PathBuf,
    /// The home directory scope walks stop at. A field for the same reason
    /// `cwd` is one: a test can point it at its own temporary tree instead of
    /// depending on where the real home happens to be.
    pub home: Option<PathBuf>,
    /// Messages worth showing the user once, collected while loading state.
    pub notices: Vec<String>,
}

impl App {
    /// Load configuration, store, and scope registry from disk.
    pub fn load() -> Result<Self> {
        let paths = Paths::resolve()?;
        let config = Config::load(&paths)?;
        let store = Store::load(&paths)?;
        let scopes = ScopeRegistry::load(&paths)?;
        let app = App {
            paths,
            config,
            store,
            scopes,
            cwd: process_cwd(),
            home: paths::home_dir(),
            notices: Vec::new(),
        };
        Ok(app.finish_load())
    }

    /// Collect the non-fatal problems in the state just loaded. A bad scope
    /// registry, an unknown `scope.default`, and an entry pointing at a scope
    /// that no longer exists are all reachable from a hand edit or a sync
    /// merge, and none of them may make the store unloadable.
    fn finish_load(mut self) -> Self {
        self.notices.extend(self.scopes.issues().iter().cloned());
        if let (_, Some(notice)) = self.config.scope.resolve() {
            self.notices.push(notice);
        }
        let mut unknown: Vec<String> = self
            .store
            .entries()
            .iter()
            .filter_map(|e| e.scope.as_ref())
            .filter(|id| self.scopes.find(id).is_none())
            .cloned()
            .collect();
        unknown.sort();
        unknown.dedup();
        for id in unknown {
            self.notices.push(format!(
                "entries reference unknown scope '{id}'; they are inactive until you \
                 re-create that scope or move them out of it"
            ));
        }
        // Judged against the shells actually being generated: a body only zsh
        // reads is carried nowhere once the zsh file is turned off, and that is
        // a silent drop rather than a per-shell exclusion.
        let shells = self.enabled_shells();
        let mut unusable: Vec<String> = self
            .store
            .entries()
            .iter()
            .filter_map(|e| {
                let problem =
                    model::name_problem(&e.name).or_else(|| e.command_problem_among(&shells))?;
                Some(format!(
                    "alias '{}' is not defined: {problem}",
                    one_line(&e.name)
                ))
            })
            .collect();
        unusable.sort();
        unusable.dedup();
        self.notices.append(&mut unusable);
        self
    }

    /// Non-fatal problems found while loading. Printed once by the CLI.
    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    /// Construct from explicit parts (used in tests).
    pub fn with(paths: Paths, config: Config, store: Store) -> Self {
        App {
            paths,
            config,
            store,
            scopes: ScopeRegistry::default(),
            cwd: process_cwd(),
            home: paths::home_dir(),
            notices: Vec::new(),
        }
    }

    /// Replace the scope registry.
    pub fn with_scopes(mut self, scopes: ScopeRegistry) -> Self {
        self.scopes = scopes;
        self
    }

    /// Replace the working directory scoping is resolved against.
    pub fn with_cwd(mut self, cwd: PathBuf) -> Self {
        self.cwd = cwd;
        self
    }

    /// Replace the home directory scope walks stop at.
    pub fn with_home(mut self, home: PathBuf) -> Self {
        self.home = Some(home);
        self
    }

    /// Persist the store and the scope registry, regenerate the enabled shell
    /// files, then kick off a background sync so the change is committed and
    /// pushed.
    pub fn persist(&self) -> Result<()> {
        self.store.save(&self.paths)?;
        self.scopes.save(&self.paths)?;
        self.regenerate_shells()?;
        crate::backup::spawn_sync(&self.paths);
        Ok(())
    }

    /// Regenerate shell definition files for enabled shells.
    pub fn regenerate_shells(&self) -> Result<()> {
        shell::regenerate(
            &self.paths,
            self.store.entries(),
            &self.scopes,
            self.config.shells.zsh,
            self.config.shells.bash,
        )
    }

    /// One line per enabled shell whose generated file has to leave `entry`
    /// out, saying which shell and why.
    ///
    /// Empty for the ordinary entry. `Entry::validate` only refuses a body no
    /// shell can carry, so a body one of them reads is stored and simply misses
    /// the other's file. That is worth saying once, at the save that caused it.
    pub fn exclusion_notes(&self, key: &EntryKey) -> Vec<String> {
        let Some(entry) = self.store.find(key) else {
            return Vec::new();
        };
        self.enabled_shells()
            .into_iter()
            .filter_map(|s| {
                let problem = entry.command_problem_in(s)?;
                Some(format!(
                    "'{}' is left out of the {} file: {problem}.",
                    one_line(&entry.name),
                    s.as_str()
                ))
            })
            .collect()
    }

    fn enabled_shells(&self) -> Vec<shell::Shell> {
        shell::Shell::all()
            .into_iter()
            .filter(|s| match s {
                shell::Shell::Zsh => self.config.shells.zsh,
                shell::Shell::Bash => self.config.shells.bash,
            })
            .collect()
    }

    /// Resolve a placement against the configured default, the registry, and
    /// the app's working directory, without creating anything.
    pub fn resolve_target(&self, placement: &Placement) -> Result<Target> {
        let (mode, _) = self.config.scope.resolve();
        scopes::resolve_placement(
            placement,
            mode,
            &self.scopes,
            &self.cwd,
            self.home.as_deref(),
        )
    }

    /// The registered scope ids for an error message, or `none` when empty.
    pub fn scope_ids_label(&self) -> String {
        let mut ids: Vec<&str> = self.scopes.scopes().iter().map(|s| s.id.as_str()).collect();
        if ids.is_empty() {
            return "none".to_string();
        }
        ids.sort_unstable();
        ids.join(", ")
    }

    /// How a resolved key reads to the user: `global`, or the scope id
    /// followed by its directory.
    pub fn placement_label(&self, key: &EntryKey) -> String {
        let Some(id) = &key.scope else {
            return "global".to_string();
        };
        match self.scopes.resolved_path(id) {
            Some(dir) => format!("{id} ({})", paths::portable_path(&dir)),
            None => id.clone(),
        }
    }

    /// Resolve a name the user typed into the single entry a command should
    /// act on. Ambiguity is settled the way the shell settles it, so the CLI
    /// and the live definitions can never disagree about which entry is meant.
    pub fn resolve_key(&self, name: &str, selection: Selection<'_>) -> Result<EntryKey> {
        let candidates = self.store.find_all_named(name);
        if candidates.is_empty() {
            bail!("no entry named '{name}'");
        }
        match selection {
            Selection::In("global") => {
                pick(&candidates, None).ok_or_else(|| anyhow!("no global entry named '{name}'"))
            }
            Selection::In(id) => pick(&candidates, Some(id))
                .ok_or_else(|| anyhow!("no entry named '{name}' in scope '{id}'")),
            Selection::CurrentScope => {
                for scope in self.scopes.active(&self.cwd).iter().rev() {
                    if let Some(key) = pick(&candidates, Some(&scope.id)) {
                        return Ok(key);
                    }
                }
                bail!("no scope in this directory has an entry named '{name}'")
            }
            Selection::Auto => {
                for scope in self.scopes.active(&self.cwd).iter().rev() {
                    if let Some(key) = pick(&candidates, Some(&scope.id)) {
                        return Ok(key);
                    }
                }
                if let Some(key) = pick(&candidates, None) {
                    return Ok(key);
                }
                if candidates.len() == 1 {
                    return Ok(EntryKey::of(candidates[0]));
                }
                let list = candidates
                    .iter()
                    .map(|e| EntryKey::of(e).label())
                    .collect::<Vec<_>>()
                    .join(", ");
                bail!("'{name}' is ambiguous: {list}. Choose one with --in <id|global>")
            }
        }
    }

    /// Add an entry at the requested placement and persist, returning the key
    /// it landed under. One of the two places a placement creates a scope, the
    /// other being [`App::update`]: the store accepts the entry first, so a
    /// rejected entry leaves no scope behind.
    pub fn add(&mut self, mut entry: Entry, placement: &Placement) -> Result<EntryKey> {
        let pending = match self.resolve_target(placement)? {
            Target::Global => {
                entry.scope = None;
                None
            }
            Target::Existing(id) => {
                entry.scope = Some(id);
                None
            }
            Target::NewAt(dir) => {
                let scope = self.plan_scope(&dir)?;
                entry.scope = Some(scope.id.clone());
                Some(scope)
            }
        };
        let key = EntryKey::of(&entry);
        self.store.add(entry)?;
        if let Some(scope) = pending {
            self.scopes.insert(scope);
        }
        self.persist()?;
        Ok(key)
    }

    /// Update an entry (matched by `key`) and persist, returning the key it
    /// now lives under. A `placement` of `None` keeps the entry where it is.
    /// Creates a scope under the same rule as [`App::add`]: the store accepts
    /// the entry first, so a rejected move leaves no scope behind.
    pub fn update(
        &mut self,
        key: &EntryKey,
        mut entry: Entry,
        placement: Option<&Placement>,
    ) -> Result<EntryKey> {
        let pending = match placement {
            None => {
                entry.scope = key.scope.clone();
                None
            }
            Some(placement) => match self.resolve_target(placement)? {
                Target::Global => {
                    entry.scope = None;
                    None
                }
                Target::Existing(id) => {
                    entry.scope = Some(id);
                    None
                }
                Target::NewAt(dir) => {
                    let scope = self.plan_scope(&dir)?;
                    entry.scope = Some(scope.id.clone());
                    Some(scope)
                }
            },
        };
        let new_key = EntryKey::of(&entry);
        self.store.update(key, entry)?;
        if let Some(scope) = pending {
            self.scopes.insert(scope);
        }
        self.persist()?;
        Ok(new_key)
    }

    /// The scope a directory with no scope yet would get. The id avoids every
    /// id the store still references, not just the registered ones, so a scope
    /// lost from `scopes.toml` cannot have its orphaned entries adopted by an
    /// unrelated directory that happens to slug the same way.
    fn plan_scope(&self, dir: &Path) -> Result<Scope> {
        let path = paths::portable_path(dir);
        if !scopes::is_valid_path(&path) {
            bail!(
                "cannot scope to '{}': a directory name with a control character in it \
                 cannot be written to the generated shell files",
                scopes::one_line(&path)
            );
        }
        let taken: HashSet<&str> = self
            .store
            .entries()
            .iter()
            .filter_map(|e| e.scope.as_deref())
            .collect();
        Ok(self.scopes.plan_create(dir, &now_timestamp(), &taken))
    }

    /// Remove an entry and persist.
    pub fn remove(&mut self, key: &EntryKey) -> Result<Entry> {
        let removed = self.store.remove(key)?;
        self.persist()?;
        Ok(removed)
    }

    /// Every registered scope, with what the user needs to tell a healthy
    /// scope from one that can never activate here.
    pub fn scope_summaries(&self) -> Vec<ScopeSummary> {
        self.scopes
            .scopes()
            .iter()
            .map(|scope| {
                let resolved = paths::expand_path(&scope.path);
                let active = scopes::path_contains(&resolved, &self.cwd);
                let reached_by_realpath = scopes::path_contains(
                    &scopes::canonical(&resolved),
                    &scopes::canonical(&self.cwd),
                );
                ScopeSummary {
                    id: scope.id.clone(),
                    path: scope.path.clone(),
                    entries: self.entries_in_scope(&scope.id).len(),
                    exists: resolved.is_dir(),
                    active,
                    inactive_until_resolved: (!active && reached_by_realpath)
                        .then(|| self.cwd.clone()),
                    resolved,
                }
            })
            .collect()
    }

    /// Point a scope at a different directory and persist. Unlike a move, an
    /// unknown id is created rather than rejected: this is how an entry whose
    /// scope went missing is made live again.
    pub fn relocate_scope(&mut self, id: &str, dir: &Path) -> Result<()> {
        let id = id.trim();
        if id.is_empty() {
            bail!("a scope id is required");
        }
        if scopes::is_reserved_id(id) {
            bail!("'{id}' is a reserved scope id");
        }
        if !scopes::is_valid_id(id) {
            bail!("'{id}' is not a valid scope id: use letters, digits, '-' and '_'");
        }
        // Stored the way the shell would name the directory, since the
        // dispatcher matches the logical `$PWD`. Only made absolute, never
        // resolved through symlinks.
        let dir = scopes::normalize_dir(&self.cwd, dir);
        let path = paths::portable_path(&dir);
        if !scopes::is_valid_path(&path) {
            bail!(
                "cannot scope to '{}': a directory name with a control character in it \
                 cannot be written to the generated shell files",
                scopes::one_line(&path)
            );
        }
        // Compared through symlinks rather than by exact text, so a scope
        // stored the way the shell names a directory still counts as the
        // claim on the directory this relocation just resolved.
        if let Some(other) = self
            .scopes
            .scopes()
            .iter()
            .find(|s| s.id != id && scopes::same_dir(&paths::expand_path(&s.path), &dir))
        {
            bail!("'{}' is already the scope for {}", other.id, path);
        }
        if self.scopes.find(id).is_some() {
            self.scopes.relocate(id, &dir)?;
        } else {
            self.scopes.insert(Scope {
                id: id.to_string(),
                path,
                created_at: now_timestamp(),
            });
        }
        self.persist()
    }

    /// Remove a scope, moving its entries out to global, and persist. Every
    /// name is checked first so a collision leaves the scope exactly as it
    /// was rather than promoting half of it.
    pub fn remove_scope_promoting(&mut self, id: &str) -> Result<usize> {
        let entries = self.scope_entries_to_move(id)?;
        let mut collisions: Vec<&str> = entries
            .iter()
            .filter(|e| self.store.find(&EntryKey::global(&e.name)).is_some())
            .map(|e| e.name.as_str())
            .collect();
        if !collisions.is_empty() {
            collisions.sort_unstable();
            bail!(
                "cannot promote scope '{id}': a global entry already exists named {}",
                collisions.join(", ")
            );
        }
        for entry in &entries {
            let mut promoted = entry.clone();
            promoted.scope = None;
            self.store.update(&EntryKey::of(entry), promoted)?;
        }
        self.drop_scope(id);
        self.persist()?;
        Ok(entries.len())
    }

    /// Remove a scope along with every entry in it, and persist.
    pub fn remove_scope_deleting(&mut self, id: &str) -> Result<usize> {
        let entries = self.scope_entries_to_move(id)?;
        for entry in &entries {
            self.store.remove(&EntryKey::of(entry))?;
        }
        self.drop_scope(id);
        self.persist()?;
        Ok(entries.len())
    }

    fn entries_in_scope(&self, id: &str) -> Vec<&Entry> {
        self.store
            .entries()
            .iter()
            .filter(|e| e.scope.as_deref() == Some(id))
            .collect()
    }

    /// The entries a scope removal has to deal with. An id no registry knows
    /// is still accepted while entries reference it, so a scope lost to a
    /// hand edit or a merge can be cleaned up.
    fn scope_entries_to_move(&self, id: &str) -> Result<Vec<Entry>> {
        let entries: Vec<Entry> = self.entries_in_scope(id).into_iter().cloned().collect();
        if entries.is_empty() && self.scopes.find(id).is_none() {
            bail!("unknown scope '{id}'");
        }
        Ok(entries)
    }

    fn drop_scope(&mut self, id: &str) {
        let _ = self.scopes.remove(id);
    }

    /// Search the store.
    pub fn search(&self, query: &str, filter: &Filter) -> Vec<&Entry> {
        search(self.store.entries(), query, filter)
    }

    /// Classify an entry for capture safety.
    pub fn assess(&self, entry: &Entry) -> Assessment {
        capture::classify(entry)
    }

    /// Where an entry's capture runs. A scoped command usually only makes
    /// sense inside its own directory, so that is where it is run from.
    pub fn capture_cwd(&self, entry: &Entry) -> CaptureCwd {
        let Some(dir) = entry
            .scope
            .as_deref()
            .and_then(|id| self.scopes.resolved_path(id))
        else {
            return CaptureCwd::Inherit;
        };
        if dir.is_dir() {
            CaptureCwd::In(dir)
        } else {
            CaptureCwd::MissingScopeDir(dir)
        }
    }

    /// Run the capture pipeline for an entry (does not persist).
    pub fn capture(&self, entry: &Entry) -> Result<CaptureResult> {
        let cwd = match self.capture_cwd(entry) {
            CaptureCwd::In(dir) => Some(dir),
            CaptureCwd::Inherit | CaptureCwd::MissingScopeDir(_) => None,
        };
        capture::run_capture(&self.config.capture, &entry.command, cwd.as_deref())
    }

    /// Fetch parsed help sections for a command.
    pub fn fetch_help(&self, command: &str) -> capture::HelpSections {
        capture::fetch_help(&self.config.capture, command)
    }

    /// Fill an entry's empty description/usage/options/examples from the
    /// command's help. Never overwrites fields that already have content.
    /// Returns true when anything was filled.
    pub fn fill_from_help(&self, entry: &mut Entry) -> bool {
        let sections = self.fetch_help(&entry.command);
        let mut changed = false;
        for (field, value) in [
            (&mut entry.description, sections.description),
            (&mut entry.usage, sections.usage),
            (&mut entry.options, sections.options),
            (&mut entry.examples, sections.examples),
        ] {
            if field.trim().is_empty() && !value.is_empty() {
                *field = value;
                changed = true;
            }
        }
        changed
    }

    /// Record captured output on an entry and persist. Marks the entry
    /// destructive when the assessment says so.
    pub fn record_capture(
        &mut self,
        key: &EntryKey,
        output: String,
        destructive: bool,
    ) -> Result<()> {
        let mut entry = self
            .store
            .find(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no entry '{}'", key.label()))?;
        entry.example_output = output;
        if destructive {
            entry.destructive = true;
            if entry.capture_policy == CapturePolicy::Auto {
                entry.capture_policy = CapturePolicy::Confirm;
            }
        }
        self.update(key, entry, None).map(|_| ())
    }
}

/// The candidate living in `scope`, where `None` means global.
fn pick(candidates: &[&Entry], scope: Option<&str>) -> Option<EntryKey> {
    candidates
        .iter()
        .find(|e| e.scope.as_deref() == scope)
        .map(|e| EntryKey::of(e))
}

/// The working directory as the shell names it. Shells keep `PWD` logical
/// across a `cd` through a symlink while `current_dir` reports the physical
/// path, and the generated dispatcher matches `$PWD`, so the logical form is
/// the one scope paths have to be stored and compared in. `PWD` is only
/// trusted when it still names the directory the process is actually in.
fn process_cwd() -> PathBuf {
    let physical = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match std::env::var_os("PWD").map(PathBuf::from) {
        Some(logical) if logical.is_absolute() && scopes::same_dir(&logical, &physical) => logical,
        _ => physical,
    }
}

/// An RFC 3339 timestamp for the current instant.
pub fn now_timestamp() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    fn temp_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let app = App::with(paths, Config::default(), Store::default());
        (dir, app)
    }

    /// An app rooted entirely in `dir`, including its home, so scope walks stop
    /// at the temporary tree instead of climbing toward wherever `TMPDIR`
    /// points. Canonicalized because `with_cwd` callers canonicalize too and
    /// the walk compares paths exactly.
    fn test_app_with(dir: &tempfile::TempDir, config: Config) -> App {
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        App::with(paths, config, Store::default()).with_home(dir.path().canonicalize().unwrap())
    }

    fn test_app(dir: &tempfile::TempDir) -> App {
        test_app_with(dir, Config::default())
    }

    #[test]
    fn add_persists_store_and_regenerates_shell() {
        let (dir, mut app) = temp_app();
        app.add(
            Entry::new("gitsw", Kind::Alias, "git switch", "switch"),
            &Placement::Global,
        )
        .unwrap();

        assert!(dir.path().join("categories/general.toml").exists());
        let zsh = std::fs::read_to_string(dir.path().join("shell/cmd-man.zsh")).unwrap();
        assert!(zsh.contains("alias gitsw='git switch'"));
    }

    #[test]
    fn remove_updates_generated_files() {
        let (dir, mut app) = temp_app();
        app.add(
            Entry::new("gitsw", Kind::Alias, "git switch", "switch"),
            &Placement::Global,
        )
        .unwrap();
        app.remove(&EntryKey::global("gitsw")).unwrap();
        let zsh = std::fs::read_to_string(dir.path().join("shell/cmd-man.zsh")).unwrap();
        assert!(!zsh.contains("gitsw"));
    }

    #[test]
    fn record_capture_sets_output_and_flags() {
        let (_dir, mut app) = temp_app();
        app.add(
            Entry::new("gitsw", Kind::Alias, "git switch", "switch"),
            &Placement::Global,
        )
        .unwrap();
        app.record_capture(
            &EntryKey::global("gitsw"),
            "Switched to branch 'main'".into(),
            true,
        )
        .unwrap();
        let e = app.store.find(&EntryKey::global("gitsw")).unwrap();
        assert_eq!(e.example_output, "Switched to branch 'main'");
        assert!(e.destructive);
        assert_eq!(e.capture_policy, CapturePolicy::Confirm);
    }

    #[test]
    fn add_here_creates_a_scope_and_returns_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work/api");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut app = test_app(&dir).with_cwd(cwd.canonicalize().unwrap());

        let key = app
            .add(
                Entry::new("ll", Kind::Alias, "ls -lah", "d"),
                &Placement::Here,
            )
            .unwrap();

        assert_eq!(key.scope.as_deref(), Some("api"));
        assert_eq!(app.scopes.scopes().len(), 1);
        assert_eq!(app.store.find(&key).unwrap().command, "ls -lah");
    }

    #[test]
    fn add_here_anchors_at_the_enclosing_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work/api");
        let cwd = root.join("src/deep");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let mut app = test_app(&dir).with_cwd(cwd.canonicalize().unwrap());

        let key = app
            .add(Entry::new("ll", Kind::Alias, "ls", "d"), &Placement::Here)
            .unwrap();

        assert_eq!(key.scope.as_deref(), Some("api"));
        assert_eq!(
            app.scopes.resolved_path("api").unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn add_never_claims_an_id_orphaned_entries_still_reference() {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work/api");
        let other = dir.path().join("other/api");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mut app = test_app(&dir).with_cwd(work.canonicalize().unwrap());

        let key = app
            .add(
                Entry::new("secret", Kind::Alias, "echo prod-token", "d"),
                &Placement::Here,
            )
            .unwrap();
        assert_eq!(key.scope.as_deref(), Some("api"));

        // The registry loses the line, as a hand edit or a merge can do,
        // leaving the entry pointing at an id nothing defines.
        app.scopes = ScopeRegistry::default();
        let mut app = app.with_cwd(other.canonicalize().unwrap());

        let new_key = app
            .add(
                Entry::new("ll", Kind::Alias, "ls -la", "d"),
                &Placement::Here,
            )
            .unwrap();

        assert_eq!(new_key.scope.as_deref(), Some("api-2"));
        assert!(app.scopes.find("api").is_none());
        assert_eq!(
            app.scopes.resolved_path("api-2").unwrap(),
            other.canonicalize().unwrap()
        );
    }

    #[test]
    fn a_scope_directory_the_shell_files_cannot_carry_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let weird = dir.path().join("we\nird");
        std::fs::create_dir_all(&weird).unwrap();
        let mut app = test_app(&dir).with_cwd(weird.canonicalize().unwrap());

        let err = app
            .add(
                Entry::new("dep", Kind::Alias, "make deploy", "d"),
                &Placement::Here,
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("control character"), "err was: {err}");
        assert!(app.scopes.scopes().is_empty());
        assert!(app.store.entries().is_empty());

        let err = app.relocate_scope("weird", &weird).unwrap_err().to_string();
        assert!(err.contains("control character"), "err was: {err}");
        assert!(app.scopes.scopes().is_empty());
    }

    #[test]
    fn add_reuses_an_existing_scope_on_the_same_path() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work/api");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut app = test_app(&dir).with_cwd(cwd.canonicalize().unwrap());

        app.add(
            Entry::new("a", Kind::Alias, "echo a", "d"),
            &Placement::Here,
        )
        .unwrap();
        app.add(
            Entry::new("b", Kind::Alias, "echo b", "d"),
            &Placement::Here,
        )
        .unwrap();

        assert_eq!(app.scopes.scopes().len(), 1);
    }

    #[test]
    fn a_rejected_entry_registers_no_scope() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work/api");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut app = test_app(&dir).with_cwd(cwd.canonicalize().unwrap());
        app.add(Entry::new("ll", Kind::Alias, "ls", "d"), &Placement::Here)
            .unwrap();
        let before = app.scopes.scopes().len();

        assert!(
            app.add(Entry::new("ll", Kind::Alias, "ls", "d"), &Placement::Here)
                .is_err()
        );
        assert_eq!(app.scopes.scopes().len(), before);
    }

    #[test]
    fn add_global_leaves_the_registry_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&dir);

        let key = app
            .add(Entry::new("ll", Kind::Alias, "ls", "d"), &Placement::Global)
            .unwrap();

        assert_eq!(key, EntryKey::global("ll"));
        assert!(app.scopes.is_empty());
    }

    #[test]
    fn update_without_a_placement_keeps_the_scope() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app(&dir).with_cwd(api.canonicalize().unwrap());
        let key = app
            .add(Entry::new("ll", Kind::Alias, "ls", "d"), &Placement::Here)
            .unwrap();

        let mut edited = app.store.find(&key).unwrap().clone();
        edited.command = "ls -lah".into();

        let new_key = app.update(&key, edited, None).unwrap();

        assert_eq!(new_key, key);
        assert_eq!(app.store.find(&key).unwrap().command, "ls -lah");
    }

    #[test]
    fn move_here_creates_a_scope_for_a_global_entry() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app(&dir);
        let key = app
            .add(Entry::new("ll", Kind::Alias, "ls", "d"), &Placement::Global)
            .unwrap();

        let mut app = app.with_cwd(api.canonicalize().unwrap());
        let entry = app.store.find(&key).unwrap().clone();
        let new_key = app.update(&key, entry, Some(&Placement::Here)).unwrap();

        assert_eq!(new_key.scope.as_deref(), Some("api"));
        assert!(app.store.find(&key).is_none());
        assert_eq!(app.scopes.scopes().len(), 1);
    }

    #[test]
    fn make_global_rescues_an_entry_from_an_unknown_scope() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let mut store = Store::default();
        let mut entry = Entry::new("ghost", Kind::Alias, "boo", "d");
        entry.scope = Some("gone".into());
        store.add(entry.clone()).unwrap();
        let mut app = App::with(paths, Config::default(), store);

        let key = EntryKey {
            scope: Some("gone".into()),
            name: "ghost".into(),
        };
        let new_key = app.update(&key, entry, Some(&Placement::Global)).unwrap();

        assert_eq!(new_key, EntryKey::global("ghost"));
    }

    #[test]
    fn move_to_an_unknown_scope_errors_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&dir);
        let key = app
            .add(Entry::new("ll", Kind::Alias, "ls", "d"), &Placement::Global)
            .unwrap();
        let entry = app.store.find(&key).unwrap().clone();

        let err = app
            .update(&key, entry, Some(&Placement::Scope("nope".into())))
            .unwrap_err()
            .to_string();

        assert!(err.contains("unknown scope 'nope'"), "err was: {err}");
        assert!(app.scopes.is_empty());
        assert!(app.store.find(&key).is_some());
    }

    #[test]
    fn moving_onto_an_existing_pair_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app(&dir);
        let global = app
            .add(Entry::new("ll", Kind::Alias, "ls", "d"), &Placement::Global)
            .unwrap();
        let mut app = app.with_cwd(api.canonicalize().unwrap());
        app.add(
            Entry::new("ll", Kind::Alias, "ls -lah", "d"),
            &Placement::Here,
        )
        .unwrap();

        let entry = app.store.find(&global).unwrap().clone();

        assert!(app.update(&global, entry, Some(&Placement::Here)).is_err());
        assert!(app.store.find(&global).is_some());
    }

    #[test]
    fn auto_selection_prefers_the_deepest_active_scope() {
        let dir = tempfile::tempdir().unwrap();
        let outer = dir.path().join("work");
        let inner = outer.join("api");
        std::fs::create_dir_all(&inner).unwrap();
        let mut app = test_app(&dir);
        app.add(
            Entry::new("x", Kind::Alias, "global", "d"),
            &Placement::Global,
        )
        .unwrap();
        app = app.with_cwd(outer.canonicalize().unwrap());
        app.add(Entry::new("x", Kind::Alias, "outer", "d"), &Placement::Here)
            .unwrap();
        app = app.with_cwd(inner.canonicalize().unwrap());
        app.add(Entry::new("x", Kind::Alias, "inner", "d"), &Placement::Here)
            .unwrap();

        assert_eq!(
            app.resolve_key("x", Selection::Auto)
                .unwrap()
                .scope
                .as_deref(),
            Some("api")
        );

        let app = app.with_cwd(outer.canonicalize().unwrap());
        assert_eq!(
            app.resolve_key("x", Selection::Auto)
                .unwrap()
                .scope
                .as_deref(),
            Some("work")
        );

        let app = app.with_cwd(dir.path().canonicalize().unwrap());
        assert_eq!(app.resolve_key("x", Selection::Auto).unwrap().scope, None);
    }

    #[test]
    fn auto_selection_takes_a_single_inactive_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app(&dir).with_cwd(api.canonicalize().unwrap());
        app.add(
            Entry::new("dep", Kind::Alias, "deploy", "d"),
            &Placement::Here,
        )
        .unwrap();

        let app = app.with_cwd(dir.path().canonicalize().unwrap());
        assert_eq!(
            app.resolve_key("dep", Selection::Auto)
                .unwrap()
                .scope
                .as_deref(),
            Some("api")
        );
    }

    #[test]
    fn auto_selection_fails_and_lists_candidates_when_ambiguous() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("work/a");
        let b = dir.path().join("work/b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut app = test_app(&dir).with_cwd(a.canonicalize().unwrap());
        app.add(Entry::new("dep", Kind::Alias, "a", "d"), &Placement::Here)
            .unwrap();
        let mut app = app.with_cwd(b.canonicalize().unwrap());
        app.add(Entry::new("dep", Kind::Alias, "b", "d"), &Placement::Here)
            .unwrap();

        let app = app.with_cwd(dir.path().canonicalize().unwrap());
        let err = app
            .resolve_key("dep", Selection::Auto)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--in"), "err was: {err}");
        assert!(
            err.contains("dep (a)") && err.contains("dep (b)"),
            "err was: {err}"
        );
    }

    #[test]
    fn current_scope_selection_does_not_fall_back_to_global() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = test_app(&dir).with_cwd(dir.path().canonicalize().unwrap());
        app.add(
            Entry::new("x", Kind::Alias, "global", "d"),
            &Placement::Global,
        )
        .unwrap();
        let err = app
            .resolve_key("x", Selection::CurrentScope)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no scope"), "err was: {err}");
    }

    #[test]
    fn in_selects_an_entry_in_an_unknown_scope() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let mut store = Store::default();
        let mut entry = Entry::new("ghost", Kind::Alias, "boo", "d");
        entry.scope = Some("gone".into());
        store.add(entry).unwrap();
        let app = App::with(paths, Config::default(), store);

        let key = app.resolve_key("ghost", Selection::In("gone")).unwrap();
        assert_eq!(key.scope.as_deref(), Some("gone"));
        assert!(app.resolve_key("ghost", Selection::In("global")).is_err());
        assert!(app.resolve_key("nothing", Selection::Auto).is_err());
    }

    #[test]
    fn placement_label_names_the_scope_and_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let app =
            test_app(&dir).with_scopes(ScopeRegistry::from_scopes(vec![crate::scopes::Scope {
                id: "api".into(),
                path: "$HOME/work/api".into(),
                created_at: "t".into(),
            }]));

        assert_eq!(app.placement_label(&EntryKey::global("ll")), "global");
        assert_eq!(
            app.placement_label(&EntryKey {
                scope: Some("api".into()),
                name: "ll".into(),
            }),
            "api ($HOME/work/api)"
        );
        assert_eq!(
            app.placement_label(&EntryKey {
                scope: Some("gone".into()),
                name: "ll".into(),
            }),
            "gone"
        );
    }

    #[test]
    fn an_unknown_scope_id_is_a_notice_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::write(
            paths.categories_dir().join("general.toml"),
            "[[entry]]\nname = \"ll\"\nscope = \"gone\"\nkind = \"alias\"\ncommand = \"ls\"\n\
             description = \"d\"\ncategory = \"general\"\ncreated_at = \"t\"\nupdated_at = \"t\"\n",
        )
        .unwrap();

        let store = Store::load(&paths).unwrap();
        let app = App::with(paths, Config::default(), store).finish_load();

        assert_eq!(app.store.entries().len(), 1);
        assert!(
            app.notices().iter().any(|n| n.contains("gone")),
            "notices were: {:?}",
            app.notices()
        );
    }

    #[test]
    fn an_unknown_scope_default_is_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let mut config = Config::default();
        config.scope.default = "nonsense".into();

        let app = App::with(paths, config, Store::default()).finish_load();

        assert!(
            app.notices().iter().any(|n| n.contains("nonsense")),
            "notices were: {:?}",
            app.notices()
        );
    }

    #[test]
    fn relocate_creates_an_unknown_scope_and_rejects_a_claimed_path() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let mut app = test_app(&dir);

        app.relocate_scope("gone", &a).unwrap();
        assert_eq!(
            app.scopes.find("gone").unwrap().path,
            paths::portable_path(&a)
        );

        app.relocate_scope("other", &b).unwrap();
        let err = app.relocate_scope("other", &a).unwrap_err().to_string();
        assert!(err.contains("already"), "err was: {err}");
        assert!(app.relocate_scope("  ", &a).is_err());
    }

    #[test]
    fn relocate_keeps_the_symlinked_path_the_shell_would_report() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("actual/api");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("api");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut app = test_app(&dir);

        app.relocate_scope("api", &link).unwrap();
        assert_eq!(
            app.scopes.find("api").unwrap().path,
            paths::portable_path(&link)
        );

        app.add(
            Entry::new("dep", Kind::Alias, "deploy", "d"),
            &Placement::Scope("api".into()),
        )
        .unwrap();

        // Relocating onto the directory it already covers is a no-op, not a
        // silent rewrite to the realpath that would stop the scope matching.
        app.relocate_scope("api", &link).unwrap();
        assert_eq!(
            app.scopes.find("api").unwrap().path,
            paths::portable_path(&link)
        );
        let zsh = std::fs::read_to_string(dir.path().join("shell/cmd-man.zsh")).unwrap();
        assert!(
            zsh.contains(&format!("_cmdman_dir_1='{}'", link.display())),
            "zsh was: {zsh}"
        );
    }

    #[test]
    fn relocate_stores_a_relative_directory_against_the_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut app = test_app(&dir).with_cwd(cwd.clone());

        app.relocate_scope("later", Path::new("./notyet")).unwrap();
        assert_eq!(
            app.scopes.find("later").unwrap().path,
            paths::portable_path(&cwd.join("notyet"))
        );
    }

    #[test]
    fn relocate_rejects_a_reserved_or_unsafe_id() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("proj");
        std::fs::create_dir_all(&target).unwrap();
        let mut app = test_app(&dir);

        for id in ["global", "all", "here"] {
            let err = app.relocate_scope(id, &target).unwrap_err().to_string();
            assert!(err.contains("reserved"), "err was: {err}");
        }

        let injected = "bad\nalias pwned=oops #";
        let err = app
            .relocate_scope(injected, &target)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a valid scope id"), "err was: {err}");
        assert!(app.scopes.scopes().is_empty());
    }

    #[test]
    fn relocate_rejects_a_directory_claimed_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("actual/api");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join("api");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut app = test_app(&dir).with_scopes(ScopeRegistry::from_scopes(vec![Scope {
            id: "api".into(),
            path: link.display().to_string(),
            created_at: "t".into(),
        }]));

        let err = app.relocate_scope("other", &link).unwrap_err().to_string();
        assert!(err.contains("'api' is already"), "err was: {err}");
        app.relocate_scope("api", &link).unwrap();
    }

    #[test]
    fn promote_to_global_fails_the_whole_operation_on_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app(&dir);
        app.add(
            Entry::new("ll", Kind::Alias, "global", "d"),
            &Placement::Global,
        )
        .unwrap();
        let mut app = app.with_cwd(api.canonicalize().unwrap());
        app.add(
            Entry::new("ll", Kind::Alias, "scoped", "d"),
            &Placement::Here,
        )
        .unwrap();
        app.add(
            Entry::new("dep", Kind::Alias, "deploy", "d"),
            &Placement::Here,
        )
        .unwrap();

        let err = app.remove_scope_promoting("api").unwrap_err().to_string();
        assert!(err.contains("ll"), "err was: {err}");
        assert!(app.scopes.find("api").is_some());
        assert_eq!(app.store.find_all_named("dep").len(), 1);
        assert_eq!(
            app.store.find_all_named("dep")[0].scope.as_deref(),
            Some("api")
        );
    }

    #[test]
    fn promote_to_global_moves_every_entry_when_there_is_no_collision() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app(&dir).with_cwd(api.canonicalize().unwrap());
        app.add(
            Entry::new("dep", Kind::Alias, "deploy", "d"),
            &Placement::Here,
        )
        .unwrap();
        app.add(
            Entry::new("bld", Kind::Alias, "build", "d"),
            &Placement::Here,
        )
        .unwrap();

        assert_eq!(app.remove_scope_promoting("api").unwrap(), 2);
        assert!(app.scopes.find("api").is_none());
        assert!(app.store.find(&EntryKey::global("dep")).is_some());
        assert!(app.store.find(&EntryKey::global("bld")).is_some());
    }

    #[test]
    fn delete_entries_removes_the_scope_and_its_entries() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app(&dir);
        app.add(
            Entry::new("keep", Kind::Alias, "keep", "d"),
            &Placement::Global,
        )
        .unwrap();
        let mut app = app.with_cwd(api.canonicalize().unwrap());
        app.add(
            Entry::new("dep", Kind::Alias, "deploy", "d"),
            &Placement::Here,
        )
        .unwrap();

        assert_eq!(app.remove_scope_deleting("api").unwrap(), 1);
        assert!(app.scopes.find("api").is_none());
        assert!(app.store.find_all_named("dep").is_empty());
        assert!(app.store.find(&EntryKey::global("keep")).is_some());
    }

    #[test]
    fn removing_a_scope_no_registry_knows_still_rescues_its_entries() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let mut store = Store::default();
        let mut entry = Entry::new("ghost", Kind::Alias, "boo", "d");
        entry.scope = Some("gone".into());
        store.add(entry).unwrap();
        let mut app = App::with(paths, Config::default(), store);

        assert_eq!(app.remove_scope_promoting("gone").unwrap(), 1);
        assert!(app.store.find(&EntryKey::global("ghost")).is_some());
        assert!(app.remove_scope_promoting("gone").is_err());
    }

    #[test]
    fn summaries_report_existence_activity_and_entry_counts() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app(&dir).with_cwd(api.canonicalize().unwrap());
        app.add(
            Entry::new("dep", Kind::Alias, "deploy", "d"),
            &Placement::Here,
        )
        .unwrap();
        app.scopes.insert(Scope {
            id: "elsewhere".into(),
            path: "/no/such/dir".into(),
            created_at: "t".into(),
        });

        let summaries = app.scope_summaries();
        let api = summaries.iter().find(|s| s.id == "api").unwrap();
        assert!(api.exists && api.active && api.entries == 1);
        assert!(api.inactive_until_resolved.is_none());
        let gone = summaries.iter().find(|s| s.id == "elsewhere").unwrap();
        assert!(!gone.exists && !gone.active && gone.entries == 0);
    }

    /// `root` and the realpath and symlink directories under it, with `root`
    /// canonical so a symlinked `TMPDIR` cannot decide the outcome.
    fn symlinked_dirs(dir: &tempfile::TempDir) -> (PathBuf, PathBuf) {
        let root = dir.path().canonicalize().unwrap();
        let real = root.join("actual/api");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("api");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        (real, link)
    }

    #[test]
    fn a_summary_flags_a_scope_the_shell_cannot_match_from_here() {
        let dir = tempfile::tempdir().unwrap();
        let (real, link) = symlinked_dirs(&dir);
        let app = test_app(&dir)
            .with_cwd(link.clone())
            .with_scopes(ScopeRegistry::from_scopes(vec![Scope {
                id: "api".into(),
                path: real.display().to_string(),
                created_at: "t".into(),
            }]));

        let summary = &app.scope_summaries()[0];
        assert!(summary.exists);
        assert!(!summary.active);
        assert_eq!(
            summary.inactive_until_resolved.as_deref(),
            Some(link.as_path())
        );
    }

    #[test]
    fn a_summary_leaves_a_scope_stored_as_the_symlinked_path_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (_, link) = symlinked_dirs(&dir);
        let app = test_app(&dir)
            .with_cwd(link.clone())
            .with_scopes(ScopeRegistry::from_scopes(vec![Scope {
                id: "api".into(),
                path: link.display().to_string(),
                created_at: "t".into(),
            }]));

        let summary = &app.scope_summaries()[0];
        assert!(summary.exists);
        assert!(summary.active);
        assert!(summary.inactive_until_resolved.is_none());
    }

    #[test]
    fn a_function_body_the_shell_files_cannot_carry_is_a_notice() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::write(
            paths.categories_dir().join("general.toml"),
            "[[entry]]\nname = \"kruby\"\nkind = \"function\"\n\
             command = \"}\\necho pwned\"\ndescription = \"d\"\ncategory = \"general\"\n\
             created_at = \"t\"\nupdated_at = \"t\"\n",
        )
        .unwrap();

        let store = Store::load(&paths).unwrap();
        let app = App::with(paths, Config::default(), store).finish_load();

        assert!(
            app.notices()
                .iter()
                .any(|n| n.contains("kruby") && n.contains("closes the function early")),
            "notices were: {:?}",
            app.notices()
        );
    }

    #[test]
    fn a_body_only_one_shell_reads_is_stored_and_reported_once() {
        // zsh reads the `;&` fall-through and bash does not, so the entry is
        // saved, kept out of the bash file, and said so at the save rather than
        // on every load the way a wholly unusable body is.
        let (dir, mut app) = temp_app();
        let body = "case $1 in a) echo a ;& b) echo b ;; esac";
        let key = app
            .add(
                Entry::new("ft", Kind::Function, body, "d"),
                &Placement::Global,
            )
            .unwrap();

        let notes = app.exclusion_notes(&key);
        assert_eq!(notes.len(), 1, "notes were: {notes:?}");
        assert!(
            notes[0].contains("'ft' is left out of the bash file"),
            "note was: {}",
            notes[0]
        );
        assert!(
            app.notices().is_empty(),
            "notices were: {:?}",
            app.notices()
        );
        let bash = std::fs::read_to_string(dir.path().join("shell/cmd-man.bash")).unwrap();
        let zsh = std::fs::read_to_string(dir.path().join("shell/cmd-man.zsh")).unwrap();
        assert!(!bash.contains("function ft {"), "bash was: {bash}");
        assert!(zsh.contains("function ft {"), "zsh was: {zsh}");
    }

    #[test]
    fn a_turned_off_shell_is_not_spoken_for() {
        // With no bash file being generated there is nothing for a zsh-only
        // body to be left out of, so the save says nothing.
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config::default();
        config.shells.bash = false;
        let mut app = test_app_with(&dir, config);
        let key = app
            .add(
                Entry::new("ft", Kind::Function, "echo a |& cat", "d"),
                &Placement::Global,
            )
            .unwrap();

        assert!(
            app.exclusion_notes(&key).is_empty(),
            "notes were: {:?}",
            app.exclusion_notes(&key)
        );
        assert!(!dir.path().join("shell/cmd-man.bash").exists());
        let zsh = std::fs::read_to_string(dir.path().join("shell/cmd-man.zsh")).unwrap();
        assert!(zsh.contains("function ft {"), "zsh was: {zsh}");
    }

    #[test]
    fn a_body_no_enabled_shell_reads_is_a_notice() {
        // The same body with zsh turned off instead is carried nowhere, which
        // is a silent drop rather than a per-shell exclusion.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        std::fs::write(
            paths.categories_dir().join("general.toml"),
            "[[entry]]\nname = \"ft\"\nkind = \"function\"\n\
             command = \"echo a |& cat\"\ndescription = \"d\"\ncategory = \"general\"\n\
             created_at = \"t\"\nupdated_at = \"t\"\n",
        )
        .unwrap();
        let mut config = Config::default();
        config.shells.zsh = false;
        let store = Store::load(&paths).unwrap();
        let app = App::with(paths.clone(), config, store).finish_load();
        assert!(
            app.notices()
                .iter()
                .any(|n| n.contains("ft") && n.contains("'|&' pipe")),
            "notices were: {:?}",
            app.notices()
        );

        // With zsh on again the same entry has a file that carries it.
        let store = Store::load(&paths).unwrap();
        let app = App::with(paths, Config::default(), store).finish_load();
        assert!(
            app.notices().is_empty(),
            "notices were: {:?}",
            app.notices()
        );
    }

    #[test]
    fn an_ordinary_entry_has_nothing_to_report() {
        let (_dir, mut app) = temp_app();
        let key = app
            .add(
                Entry::new("ok", Kind::Function, "echo ok", "d"),
                &Placement::Global,
            )
            .unwrap();
        assert!(app.exclusion_notes(&key).is_empty());
    }

    #[test]
    fn registry_issues_become_notices() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let scopes = ScopeRegistry::from_scopes(vec![
            crate::scopes::Scope {
                id: "api".into(),
                path: "~/work/api".into(),
                created_at: "t".into(),
            },
            crate::scopes::Scope {
                id: "api".into(),
                path: "~/other/api".into(),
                created_at: "t".into(),
            },
        ]);

        let app = App::with(paths, Config::default(), Store::default())
            .with_scopes(scopes)
            .finish_load();

        assert!(
            app.notices().iter().any(|n| n.contains("duplicate scope")),
            "notices were: {:?}",
            app.notices()
        );
    }

    #[test]
    fn capture_cwd_follows_the_scope_directory() {
        let dir = tempfile::tempdir().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let canonical = api.canonicalize().unwrap();
        let mut app = test_app(&dir).with_cwd(canonical.clone());
        let key = app
            .add(Entry::new("dep", Kind::Alias, "pwd", "d"), &Placement::Here)
            .unwrap();
        let entry = app.store.find(&key).unwrap().clone();
        assert_eq!(app.capture_cwd(&entry), CaptureCwd::In(canonical));

        let global = Entry::new("g", Kind::Alias, "pwd", "d");
        assert_eq!(app.capture_cwd(&global), CaptureCwd::Inherit);
    }

    #[test]
    fn capture_cwd_reports_a_missing_scope_directory() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let mut app = App::with(paths, Config::default(), Store::default());
        app.scopes.insert(Scope {
            id: "gone".into(),
            path: "/no/such/dir".into(),
            created_at: "t".into(),
        });
        let mut entry = Entry::new("x", Kind::Alias, "pwd", "d");
        entry.scope = Some("gone".into());
        assert_eq!(
            app.capture_cwd(&entry),
            CaptureCwd::MissingScopeDir(PathBuf::from("/no/such/dir"))
        );
    }
}
