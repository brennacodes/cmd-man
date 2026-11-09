//! Loading and saving the alias/function store as per-category TOML files.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::model::{DEFAULT_CATEGORY, Entry, EntryKey};
use crate::paths::Paths;

const STORE_LOCK_WAIT: Duration = Duration::from_secs(5);

pub(crate) fn try_flock(file: &File, operation: libc::c_int) -> bool {
    // SAFETY: the descriptor stays open for the duration of the call.
    unsafe { libc::flock(file.as_raw_fd(), operation | libc::LOCK_NB) == 0 }
}

/// Held across a reread-then-save and across a sync's commit and merge, so
/// neither can see or overwrite half of the other's work.
pub struct StoreLock {
    file: File,
}

impl StoreLock {
    pub fn acquire(paths: &Paths) -> Result<Self> {
        paths.ensure_dirs()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(paths.store_lock_file())
            .context("opening the store lock")?;
        let deadline = Instant::now() + STORE_LOCK_WAIT;
        while !try_flock(&file, libc::LOCK_EX) {
            if Instant::now() >= deadline {
                bail!("your entries are busy being updated; try again in a moment");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(StoreLock { file })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        try_flock(&self.file, libc::LOCK_UN);
    }
}

/// Replace `path` in one step so a reader never sees a partial file. A symlinked
/// file is written through instead, so it stays a symlink.
pub(crate) fn write_atomic(path: &Path, text: &str) -> Result<()> {
    let is_symlink = std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
    if is_symlink {
        return std::fs::write(path, text).with_context(|| format!("writing {}", path.display()));
    }
    let temp = path.with_file_name(".store-write.tmp");
    std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
    if let Ok(existing) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&temp, existing.permissions());
    }
    std::fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))
}

/// On-disk shape of a single category file: `[[entry]]` tables.
#[derive(Debug, Default, Serialize, Deserialize)]
struct CategoryFile {
    #[serde(default)]
    entry: Vec<Entry>,
}

/// The in-memory store of all managed entries.
#[derive(Debug, Default)]
pub struct Store {
    entries: Vec<Entry>,
}

impl Store {
    /// Load every category file under the categories directory.
    pub fn load(paths: &Paths) -> Result<Self> {
        let dir = paths.categories_dir();
        let mut entries = Vec::new();
        if dir.exists() {
            let mut files: Vec<_> = std::fs::read_dir(&dir)
                .with_context(|| format!("reading {}", dir.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
                .collect();
            files.sort();
            for file in files {
                let text = std::fs::read_to_string(&file)
                    .with_context(|| format!("reading {}", file.display()))?;
                let parsed: CategoryFile =
                    toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
                entries.extend(parsed.entry);
            }
        }
        let store = Store { entries };
        store.check_unique()?;
        Ok(store)
    }

    fn check_unique(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for e in &self.entries {
            let key = EntryKey::of(e);
            if !seen.insert(key.clone()) {
                bail!("duplicate entry '{}' in store", key.label());
            }
        }
        Ok(())
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Look up the single entry with this exact `(scope, name)` pair.
    pub fn find(&self, key: &EntryKey) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| e.name == key.name && e.scope == key.scope)
    }

    /// Every entry sharing a name, across all scopes.
    pub fn find_all_named(&self, name: &str) -> Vec<&Entry> {
        self.entries.iter().filter(|e| e.name == name).collect()
    }

    /// Sorted, de-duplicated list of category names in use.
    pub fn categories(&self) -> Vec<String> {
        let mut cats: Vec<String> = self
            .entries
            .iter()
            .map(|e| normalize_category(&e.category))
            .collect();
        cats.sort();
        cats.dedup();
        cats
    }

    /// Add a new entry, enforcing `(scope, name)` uniqueness and stamping
    /// timestamps.
    pub fn add(&mut self, mut entry: Entry) -> Result<()> {
        entry.category = normalize_category(&entry.category);
        entry.validate().map_err(|e| anyhow!(e))?;
        let key = EntryKey::of(&entry);
        if self.find(&key).is_some() {
            bail!("entry '{}' already exists", key.label());
        }
        let now = now_string();
        if entry.created_at.is_empty() {
            entry.created_at = now.clone();
        }
        entry.updated_at = now;
        self.entries.push(entry);
        Ok(())
    }

    /// Replace an existing entry (matched by `key`), preserving `created_at`.
    pub fn update(&mut self, key: &EntryKey, mut entry: Entry) -> Result<()> {
        entry.category = normalize_category(&entry.category);
        entry.validate().map_err(|e| anyhow!(e))?;
        // If the entry moved to a different key, that key must be free.
        let new_key = EntryKey::of(&entry);
        if &new_key != key && self.find(&new_key).is_some() {
            bail!("entry '{}' already exists", new_key.label());
        }
        let idx = self
            .entries
            .iter()
            .position(|e| e.name == key.name && e.scope == key.scope)
            .ok_or_else(|| anyhow!("no entry '{}'", key.label()))?;
        entry.created_at = self.entries[idx].created_at.clone();
        entry.updated_at = now_string();
        self.entries[idx] = entry;
        Ok(())
    }

    /// Remove an entry by key. Returns the removed entry.
    pub fn remove(&mut self, key: &EntryKey) -> Result<Entry> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.name == key.name && e.scope == key.scope)
            .ok_or_else(|| anyhow!("no entry '{}'", key.label()))?;
        Ok(self.entries.remove(idx))
    }

    /// Write the store to disk, one file per category, pruning stale files.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        self.check_unique()?;
        paths.ensure_dirs()?;
        let dir = paths.categories_dir();

        let mut grouped: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
        for e in &self.entries {
            grouped
                .entry(normalize_category(&e.category))
                .or_default()
                .push(e.clone());
        }

        // Written before anything is pruned, so a failed write cannot leave an
        // entry in neither its old category nor its new one.
        for (category, entries) in &mut grouped {
            entries.sort_by(|a, b| a.scope.cmp(&b.scope).then_with(|| a.name.cmp(&b.name)));
            let file = CategoryFile {
                entry: entries.clone(),
            };
            let text = toml::to_string_pretty(&file)
                .with_context(|| format!("serializing category '{category}'"))?;
            write_atomic(&dir.join(format!("{category}.toml")), &text)?;
        }

        for existing in std::fs::read_dir(&dir)?.filter_map(|e| e.ok()) {
            let path = existing.path();
            if path.extension().map(|x| x == "toml").unwrap_or(false) {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                if !grouped.contains_key(&stem) {
                    std::fs::remove_file(&path)
                        .with_context(|| format!("removing {}", path.display()))?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    #[test]
    fn a_write_replaces_the_file_and_leaves_no_temporary_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f.toml");
        write_atomic(&target, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(!dir.path().join(".store-write.tmp").exists());
    }

    #[test]
    fn a_write_keeps_the_files_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("f.toml");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&target, "new").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_failed_save_leaves_a_moved_entry_in_its_old_category() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let entry = |category: &str| {
            let mut e = Entry::new("moved", crate::model::Kind::Alias, "echo hi", "d");
            e.category = category.to_string();
            e
        };
        let mut store = Store::default();
        store.add(entry("zeta")).unwrap();
        store.save(&paths).unwrap();

        let mut moved = Store::default();
        moved.add(entry("alpha")).unwrap();
        let blocker = paths.categories_dir().join(".store-write.tmp");
        std::fs::create_dir(&blocker).unwrap();
        assert!(moved.save(&paths).is_err());

        assert!(paths.categories_dir().join("zeta.toml").exists());
    }

    #[test]
    fn a_symlinked_file_stays_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_atomic(&link, "through").unwrap();
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "through");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
    }

    #[test]
    fn the_store_lock_is_exclusive_until_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        let held = StoreLock::acquire(&paths).unwrap();
        let other = File::open(paths.store_lock_file()).unwrap();
        assert!(!try_flock(&other, libc::LOCK_EX));
        drop(held);
        assert!(try_flock(&other, libc::LOCK_EX));
    }
}

/// Normalize a category name into a filesystem-safe slug.
pub fn normalize_category(category: &str) -> String {
    let slug: String = category
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        DEFAULT_CATEGORY.to_string()
    } else {
        slug
    }
}

fn now_string() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    fn temp_paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        (dir, paths)
    }

    fn entry_named(name: &str) -> Entry {
        Entry::new(name, Kind::Alias, "ls -la", "d")
    }

    fn scoped_entry(name: &str, scope: &str) -> Entry {
        let mut entry = entry_named(name);
        entry.scope = Some(scope.to_string());
        entry
    }

    #[test]
    fn add_find_and_uniqueness() {
        let mut store = Store::default();
        store
            .add(Entry::new("gitsw", Kind::Alias, "git switch", "switch"))
            .unwrap();
        assert!(store.find(&EntryKey::global("gitsw")).is_some());
        let dup = store.add(Entry::new("gitsw", Kind::Alias, "git switch", "switch"));
        assert!(dup.is_err());
    }

    #[test]
    fn save_and_reload_round_trips() {
        let (_dir, paths) = temp_paths();
        let mut store = Store::default();
        let mut e = Entry::new("gitsw", Kind::Alias, "git switch", "switch");
        e.category = "Git Stuff".into();
        store.add(e).unwrap();
        store
            .add(Entry::new("c", Kind::Alias, "clear", "clear screen"))
            .unwrap();
        store.save(&paths).unwrap();

        // Category slug is applied on disk.
        assert!(paths.categories_dir().join("git-stuff.toml").exists());

        let reloaded = Store::load(&paths).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert_eq!(
            reloaded.find(&EntryKey::global("gitsw")).unwrap().category,
            "git-stuff"
        );
    }

    #[test]
    fn removing_last_entry_prunes_category_file() {
        let (_dir, paths) = temp_paths();
        let mut store = Store::default();
        store
            .add(Entry::new("c", Kind::Alias, "clear", "clear"))
            .unwrap();
        store.save(&paths).unwrap();
        assert!(paths.categories_dir().join("general.toml").exists());

        store.remove(&EntryKey::global("c")).unwrap();
        store.save(&paths).unwrap();
        assert!(!paths.categories_dir().join("general.toml").exists());
    }

    #[test]
    fn update_preserves_created_at_and_allows_rename() {
        let mut store = Store::default();
        store
            .add(Entry::new("old", Kind::Alias, "echo hi", "greet"))
            .unwrap();
        let created = store
            .find(&EntryKey::global("old"))
            .unwrap()
            .created_at
            .clone();
        let mut renamed = Entry::new("new", Kind::Alias, "echo hi", "greet");
        renamed.description = "greet louder".into();
        store.update(&EntryKey::global("old"), renamed).unwrap();
        assert!(store.find(&EntryKey::global("old")).is_none());
        let updated = store.find(&EntryKey::global("new")).unwrap();
        assert_eq!(updated.created_at, created);
        assert_eq!(updated.description, "greet louder");
    }

    #[test]
    fn same_name_in_two_scopes_is_allowed() {
        let mut store = Store::default();
        store.add(entry_named("ll")).unwrap();
        store.add(scoped_entry("ll", "api")).unwrap();
        store.add(scoped_entry("ll", "web")).unwrap();
        assert!(store.add(scoped_entry("ll", "api")).is_err());
        assert!(store.add(entry_named("ll")).is_err());
        assert_eq!(store.entries().len(), 3);
    }

    #[test]
    fn lookup_is_keyed_on_scope_and_name() {
        let mut store = Store::default();
        store.add(entry_named("ll")).unwrap();
        store.add(scoped_entry("ll", "api")).unwrap();
        assert_eq!(store.find(&EntryKey::global("ll")).unwrap().scope, None);
        assert_eq!(
            store
                .find(&EntryKey {
                    scope: Some("api".into()),
                    name: "ll".into()
                })
                .unwrap()
                .scope,
            Some("api".to_string())
        );
        assert!(
            store
                .find(&EntryKey {
                    scope: Some("gone".into()),
                    name: "ll".into()
                })
                .is_none()
        );
        assert_eq!(store.find_all_named("ll").len(), 2);
    }

    #[test]
    fn update_rejects_a_rename_onto_an_existing_pair() {
        let mut store = Store::default();
        store.add(scoped_entry("a", "api")).unwrap();
        store.add(scoped_entry("b", "api")).unwrap();
        let mut renamed = scoped_entry("a", "api");
        renamed.name = "b".into();
        assert!(
            store
                .update(
                    &EntryKey {
                        scope: Some("api".into()),
                        name: "a".into()
                    },
                    renamed
                )
                .is_err()
        );
    }

    #[test]
    fn update_allows_a_rename_that_collides_only_in_another_scope() {
        let mut store = Store::default();
        store.add(scoped_entry("a", "api")).unwrap();
        store.add(entry_named("b")).unwrap();
        let mut renamed = scoped_entry("a", "api");
        renamed.name = "b".into();
        store
            .update(
                &EntryKey {
                    scope: Some("api".into()),
                    name: "a".into(),
                },
                renamed,
            )
            .unwrap();
        assert!(store.find(&EntryKey::global("b")).is_some());
        assert!(
            store
                .find(&EntryKey {
                    scope: Some("api".into()),
                    name: "b".into()
                })
                .is_some()
        );
    }

    #[test]
    fn update_targets_only_the_keyed_scope() {
        let mut store = Store::default();
        store.add(entry_named("ll")).unwrap();
        store.add(scoped_entry("ll", "api")).unwrap();
        let mut changed = scoped_entry("ll", "api");
        changed.command = "ls -lah".into();
        store
            .update(
                &EntryKey {
                    scope: Some("api".into()),
                    name: "ll".into(),
                },
                changed,
            )
            .unwrap();
        assert_eq!(
            store.find(&EntryKey::global("ll")).unwrap().command,
            "ls -la"
        );
        assert_eq!(
            store
                .find(&EntryKey {
                    scope: Some("api".into()),
                    name: "ll".into()
                })
                .unwrap()
                .command,
            "ls -lah"
        );
    }

    #[test]
    fn remove_only_removes_the_named_pair() {
        let mut store = Store::default();
        store.add(entry_named("ll")).unwrap();
        store.add(scoped_entry("ll", "api")).unwrap();
        store
            .remove(&EntryKey {
                scope: Some("api".into()),
                name: "ll".into(),
            })
            .unwrap();
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].scope, None);

        store.add(scoped_entry("ll", "api")).unwrap();
        store.remove(&EntryKey::global("ll")).unwrap();
        assert_eq!(store.entries().len(), 1);
        assert_eq!(store.entries()[0].scope, Some("api".to_string()));
    }

    #[test]
    fn normalize_category_slugs() {
        assert_eq!(normalize_category("Git Stuff"), "git-stuff");
        assert_eq!(normalize_category("  "), "general");
        assert_eq!(normalize_category("Rails/DB"), "rails-db");
    }
}
