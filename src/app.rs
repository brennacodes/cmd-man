//! Service layer shared by the CLI and TUI: loads state and centralizes every
//! mutation so the store and generated shell files stay in sync.

use anyhow::Result;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::capture::{self, Assessment, CaptureResult};
use crate::config::Config;
use crate::model::{CapturePolicy, Entry};
use crate::paths::Paths;
use crate::search::{Filter, search};
use crate::shell;
use crate::store::Store;

/// Loaded application state.
pub struct App {
    pub paths: Paths,
    pub config: Config,
    pub store: Store,
}

impl App {
    /// Load configuration and store from disk.
    pub fn load() -> Result<Self> {
        let paths = Paths::resolve()?;
        let config = Config::load(&paths)?;
        let store = Store::load(&paths)?;
        Ok(App {
            paths,
            config,
            store,
        })
    }

    /// Construct from explicit parts (used in tests).
    pub fn with(paths: Paths, config: Config, store: Store) -> Self {
        App {
            paths,
            config,
            store,
        }
    }

    /// Persist the store and regenerate the enabled shell files, then kick off a
    /// background sync so the change is committed and pushed.
    pub fn persist(&self) -> Result<()> {
        self.store.save(&self.paths)?;
        self.regenerate_shells()?;
        crate::backup::spawn_sync(&self.paths);
        Ok(())
    }

    /// Regenerate shell definition files for enabled shells.
    pub fn regenerate_shells(&self) -> Result<()> {
        shell::regenerate(
            &self.paths,
            self.store.entries(),
            self.config.shells.zsh,
            self.config.shells.bash,
        )
    }

    /// Add an entry and persist.
    pub fn add(&mut self, entry: Entry) -> Result<()> {
        self.store.add(entry)?;
        self.persist()
    }

    /// Update an entry (matched by `name`) and persist.
    pub fn update(&mut self, name: &str, entry: Entry) -> Result<()> {
        self.store.update(name, entry)?;
        self.persist()
    }

    /// Remove an entry and persist.
    pub fn remove(&mut self, name: &str) -> Result<Entry> {
        let removed = self.store.remove(name)?;
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
    pub fn record_capture(&mut self, name: &str, output: String, destructive: bool) -> Result<()> {
        let mut entry = self
            .store
            .find(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no entry named '{name}'"))?;
        entry.example_output = output;
        if destructive {
            entry.destructive = true;
            if entry.capture_policy == CapturePolicy::Auto {
                entry.capture_policy = CapturePolicy::Confirm;
            }
        }
        self.update(name, entry)
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
        app.remove("gitsw").unwrap();
        let zsh = std::fs::read_to_string(dir.path().join("shell/cmd-man.zsh")).unwrap();
        assert!(!zsh.contains("gitsw"));
    }

    #[test]
    fn record_capture_sets_output_and_flags() {
        let (_dir, mut app) = temp_app();
        app.add(Entry::new("gitsw", Kind::Alias, "git switch", "switch"))
            .unwrap();
        app.record_capture("gitsw", "Switched to branch 'main'".into(), true)
            .unwrap();
        let e = app.store.find("gitsw").unwrap();
        assert_eq!(e.example_output, "Switched to branch 'main'");
        assert!(e.destructive);
        assert_eq!(e.capture_policy, CapturePolicy::Confirm);
    }
}
