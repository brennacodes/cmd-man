//! Service layer shared by the CLI and TUI: loads state and centralizes every
//! mutation so the store and generated shell files stay in sync.

use std::path::PathBuf;

use anyhow::Result;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::capture::{self, Assessment, CaptureResult};
use crate::config::Config;
use crate::model::{CapturePolicy, Entry, EntryKey};
use crate::paths::Paths;
use crate::scopes::ScopeRegistry;
use crate::search::{Filter, search};
use crate::shell;
use crate::store::Store;

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

    /// Add an entry and persist.
    pub fn add(&mut self, entry: Entry) -> Result<()> {
        self.store.add(entry)?;
        self.persist()
    }

    /// Update an entry (matched by `key`) and persist.
    pub fn update(&mut self, key: &EntryKey, entry: Entry) -> Result<()> {
        self.store.update(key, entry)?;
        self.persist()
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
        self.update(key, entry)
    }
}

/// The process working directory, falling back to `.` when it is unreadable.
fn process_cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
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

    #[test]
    fn add_persists_store_and_regenerates_shell() {
        let (dir, mut app) = temp_app();
        app.add(Entry::new("gitsw", Kind::Alias, "git switch", "switch"))
            .unwrap();

        assert!(dir.path().join("categories/general.toml").exists());
        let zsh = std::fs::read_to_string(dir.path().join("shell/cmd-man.zsh")).unwrap();
        assert!(zsh.contains("alias gitsw='git switch'"));
    }

    #[test]
    fn remove_updates_generated_files() {
        let (dir, mut app) = temp_app();
        app.add(Entry::new("gitsw", Kind::Alias, "git switch", "switch"))
            .unwrap();
        app.remove(&EntryKey::global("gitsw")).unwrap();
        let zsh = std::fs::read_to_string(dir.path().join("shell/cmd-man.zsh")).unwrap();
        assert!(!zsh.contains("gitsw"));
    }

    #[test]
    fn record_capture_sets_output_and_flags() {
        let (_dir, mut app) = temp_app();
        app.add(Entry::new("gitsw", Kind::Alias, "git switch", "switch"))
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
