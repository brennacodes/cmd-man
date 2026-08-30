//! Service layer shared by the CLI and TUI: loads state and centralizes every
//! mutation so the store and generated shell files stay in sync.

use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::capture::{self, Assessment, CaptureResult};
use crate::config::Config;
use crate::model::{CapturePolicy, Entry, EntryKey};
use crate::paths::{self, Paths};
use crate::scopes::{self, Placement, ScopeRegistry, Target};
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
    /// it landed under. The only place a scope is created: the store accepts
    /// the entry first, so a rejected entry leaves no scope behind.
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
                let scope = self.scopes.plan_create(&dir, &now_timestamp());
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
    /// The other place a scope is created, under the same rule as `add`: the
    /// store accepts the entry first, so a rejected move leaves no scope
    /// behind.
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
                    let scope = self.scopes.plan_create(&dir, &now_timestamp());
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

    /// Remove an entry and persist.
    pub fn remove(&mut self, key: &EntryKey) -> Result<Entry> {
        let removed = self.store.remove(key)?;
        self.persist()?;
        Ok(removed)
    }

    /// Search the store.
    pub fn search(&self, query: &str, filter: &Filter) -> Vec<&Entry> {
        search(self.store.entries(), query, filter)
    }

    /// Classify an entry for capture safety.
    pub fn assess(&self, entry: &Entry) -> Assessment {
        capture::classify(entry)
    }

    /// Run the capture pipeline for an entry (does not persist).
    pub fn capture(&self, entry: &Entry) -> Result<CaptureResult> {
        capture::run_capture(&self.config.capture, &entry.command)
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
}
