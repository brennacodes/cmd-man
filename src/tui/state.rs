//! Pure UI state and update logic for the TUI (no terminal or IO).

use std::collections::HashMap;

use crate::app::App;
use crate::model::{Entry, EntryKey, Kind};
use crate::scopes::{Placement, Scope};
use crate::search::{Filter, ScopeFilter, search};

/// An operation the run loop must carry out against the App.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Quit,
    /// Open the add (edit == false) or edit (edit == true) form.
    OpenForm {
        edit: bool,
    },
    Reload,
    Backup,
    /// Advance the scope filter; needs the App for the scope registry.
    CycleScope,
    Delete(EntryKey),
    /// User asked to capture; the run loop decides if confirmation is needed.
    CaptureRequest(EntryKey),
    /// Capture confirmed in a popup.
    CaptureConfirmed(EntryKey),
    /// Submit an add (original == None) or edit (original == Some(old_key)).
    Submit {
        original: Option<EntryKey>,
        entry: Box<Entry>,
        /// Where an added entry should land. Ignored on an edit.
        placement: Placement,
        /// Whether to auto-fill empty help fields before saving.
        autofill: bool,
    },
}

/// What a confirmation popup is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmKind {
    Delete(EntryKey),
    Capture(EntryKey),
}

/// The active interaction mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Browse,
    Search,
    Confirm { kind: ConfirmKind, message: String },
    Help,
}

/// Full TUI state.
pub struct UiState {
    entries: Vec<Entry>,
    filtered: Vec<usize>,
    selected: usize,
    pub query: String,
    pub kind_filter: Option<Kind>,
    pub category_filter: Option<String>,
    categories: Vec<String>,
    scope_filter: ScopeFilter,
    /// The registered scopes, in the order the cycle key walks them.
    scopes: Vec<Scope>,
    /// The ids of the scopes containing the App's working directory.
    active: Vec<String>,
    pub mode: Mode,
    pub status: String,
}

impl UiState {
    pub fn new(app: &App) -> Self {
        let mut state = UiState {
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            query: String::new(),
            kind_filter: None,
            category_filter: None,
            categories: Vec::new(),
            scope_filter: ScopeFilter::All,
            scopes: Vec::new(),
            active: Vec::new(),
            mode: Mode::Browse,
            status: "? for help".to_string(),
        };
        state.set_entries(app);
        state
    }

    /// Reread entries and scopes from the App (after a mutation), preserving
    /// the selected item by key.
    pub fn set_entries(&mut self, app: &App) {
        let current = self.selected_entry().map(EntryKey::of);
        self.entries = app.store.entries().to_vec();
        self.sync_scopes(app);
        self.rebuild_categories();
        // Drop a category filter that no longer exists.
        if let Some(cat) = &self.category_filter
            && !self.categories.contains(cat)
        {
            self.category_filter = None;
        }
        // Same for a scope the registry no longer knows.
        if let ScopeFilter::Only(Some(id)) = &self.scope_filter
            && !self.scopes.iter().any(|s| &s.id == id)
        {
            self.scope_filter = ScopeFilter::All;
        }
        self.refilter();
        if let Some(key) = current
            && let Some(pos) = self
                .filtered
                .iter()
                .position(|&i| EntryKey::of(&self.entries[i]) == key)
        {
            self.selected = pos;
        }
    }

    fn sync_scopes(&mut self, app: &App) {
        self.scopes = app.scopes.scopes().to_vec();
        self.active = app
            .scopes
            .active(&app.cwd)
            .iter()
            .map(|s| s.id.clone())
            .collect();
        // The active-in-cwd filter is derived from that list, so a mutation
        // that registers a scope for the working directory must widen it.
        if matches!(self.scope_filter, ScopeFilter::ActiveIn(_)) {
            self.scope_filter = ScopeFilter::ActiveIn(self.active.clone());
        }
    }

    fn rebuild_categories(&mut self) {
        let mut cats: Vec<String> = self.entries.iter().map(|e| e.category.clone()).collect();
        cats.sort();
        cats.dedup();
        self.categories = cats;
    }

    /// Recompute the filtered/ordered index list from query + filters.
    pub fn refilter(&mut self) {
        let filter = Filter {
            category: self.category_filter.clone(),
            kind: self.kind_filter,
            scope: self.scope_filter.clone(),
        };
        let index: HashMap<EntryKey, usize> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (EntryKey::of(e), i))
            .collect();
        let ordered = search(&self.entries, &self.query, &filter);
        self.filtered = ordered
            .iter()
            .filter_map(|e| index.get(&EntryKey::of(e)).copied())
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    pub fn filtered_entries(&self) -> Vec<&Entry> {
        self.filtered.iter().map(|&i| &self.entries[i]).collect()
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        self.filtered.get(self.selected).map(|&i| &self.entries[i])
    }

    pub fn move_down(&mut self) {
        if !self.filtered.is_empty() && self.selected + 1 < self.filtered.len() {
            self.selected += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn page_down(&mut self, page: usize) {
        if self.filtered.is_empty() {
            return;
        }
        self.selected = (self.selected + page).min(self.filtered.len() - 1);
    }

    pub fn page_up(&mut self, page: usize) {
        self.selected = self.selected.saturating_sub(page);
    }

    /// Cycle the kind filter: all -> alias -> function -> all.
    pub fn cycle_kind(&mut self) {
        self.kind_filter = match self.kind_filter {
            None => Some(Kind::Alias),
            Some(Kind::Alias) => Some(Kind::Function),
            Some(Kind::Function) => None,
        };
        self.refilter();
    }

    /// Cycle the category filter through the known categories and back to all.
    pub fn cycle_category(&mut self) {
        if self.categories.is_empty() {
            return;
        }
        self.category_filter = match &self.category_filter {
            None => self.categories.first().cloned(),
            Some(current) => {
                let pos = self.categories.iter().position(|c| c == current);
                match pos {
                    Some(i) if i + 1 < self.categories.len() => {
                        Some(self.categories[i + 1].clone())
                    }
                    _ => None,
                }
            }
        };
        self.refilter();
    }

    /// Cycle the scope filter: all -> active in the working directory -> each
    /// known scope -> all. A store with no scopes has nothing to cycle.
    pub fn cycle_scope(&mut self, app: &App) {
        self.sync_scopes(app);
        let ids: Vec<String> = self.scopes.iter().map(|s| s.id.clone()).collect();
        self.scope_filter = match &self.scope_filter {
            _ if ids.is_empty() => ScopeFilter::All,
            ScopeFilter::All => ScopeFilter::ActiveIn(self.active.clone()),
            ScopeFilter::ActiveIn(_) => ScopeFilter::Only(Some(ids[0].clone())),
            ScopeFilter::Only(Some(current)) => match ids.iter().position(|id| id == current) {
                Some(i) if i + 1 < ids.len() => ScopeFilter::Only(Some(ids[i + 1].clone())),
                _ => ScopeFilter::All,
            },
            ScopeFilter::Only(None) => ScopeFilter::All,
        };
        self.refilter();
    }

    pub fn scope_filter(&self) -> &ScopeFilter {
        &self.scope_filter
    }

    /// Whether the entry's definition is live in the App's working directory.
    pub fn is_active(&self, entry: &Entry) -> bool {
        match &entry.scope {
            None => true,
            Some(id) => self.active.iter().any(|active| active == id),
        }
    }

    /// The registered directory of a scope id, as stored (`$HOME`-relative).
    pub fn scope_path(&self, id: &str) -> Option<&str> {
        self.scopes
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.path.as_str())
    }

    pub fn push_query_char(&mut self, c: char) {
        self.query.push(c);
        self.refilter();
    }

    pub fn pop_query_char(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
    }

    /// Short label describing the active filters.
    pub fn filter_label(&self) -> String {
        let kind = match self.kind_filter {
            None => "all".to_string(),
            Some(k) => k.to_string(),
        };
        let cat = self.category_filter.clone().unwrap_or_else(|| "all".into());
        let scope = match self.scope_filter() {
            ScopeFilter::All => "all".to_string(),
            ScopeFilter::ActiveIn(_) => "active".to_string(),
            ScopeFilter::Only(None) => "global".to_string(),
            ScopeFilter::Only(Some(id)) => id.clone(),
        };
        format!("kind:{kind}  category:{cat}  scope:{scope}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use tempfile::TempDir;

    use crate::config::Config;
    use crate::paths::Paths;
    use crate::store::Store;

    /// An app holding `entries` and no scopes. Nothing here writes, so the
    /// root path is never touched.
    fn app_over(entries: Vec<Entry>) -> App {
        let mut store = Store::default();
        for entry in entries {
            store.add(entry).unwrap();
        }
        App::with(
            Paths::with_root(PathBuf::from("/cmd-man")),
            Config::default(),
            store,
        )
    }

    /// An app rooted entirely in `dir`, including its home, so scope walks
    /// stop at the temporary tree.
    fn test_app(dir: &TempDir) -> App {
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        App::with(paths, Config::default(), Store::default())
            .with_home(dir.path().canonicalize().unwrap())
    }

    /// The position of `key` in the visible list.
    fn position_of(state: &UiState, key: &EntryKey) -> usize {
        state
            .filtered_entries()
            .iter()
            .position(|e| &EntryKey::of(e) == key)
            .expect("key is not visible")
    }

    fn entries() -> Vec<Entry> {
        vec![
            {
                let mut e = Entry::new("gitsw", Kind::Alias, "git switch", "switch");
                e.category = "git".into();
                e
            },
            {
                let mut e = Entry::new("gst", Kind::Alias, "git status", "status");
                e.category = "git".into();
                e
            },
            {
                let mut e = Entry::new("kruby", Kind::Function, "kill ruby", "kill");
                e.category = "ruby".into();
                e
            },
        ]
    }

    #[test]
    fn navigation_stays_in_bounds() {
        let mut s = UiState::new(&app_over(entries()));
        assert_eq!(s.filtered_entries().len(), 3);
        s.move_up();
        assert_eq!(s.selected_index(), 0);
        s.move_down();
        s.move_down();
        s.move_down();
        assert_eq!(s.selected_index(), 2);
    }

    #[test]
    fn kind_filter_cycles() {
        let mut s = UiState::new(&app_over(entries()));
        s.cycle_kind();
        assert_eq!(s.kind_filter, Some(Kind::Alias));
        assert_eq!(s.filtered_entries().len(), 2);
        s.cycle_kind();
        assert_eq!(s.kind_filter, Some(Kind::Function));
        assert_eq!(s.filtered_entries().len(), 1);
        s.cycle_kind();
        assert_eq!(s.kind_filter, None);
    }

    #[test]
    fn category_filter_cycles_through_and_back() {
        let mut s = UiState::new(&app_over(entries()));
        s.cycle_category();
        assert_eq!(s.category_filter.as_deref(), Some("git"));
        s.cycle_category();
        assert_eq!(s.category_filter.as_deref(), Some("ruby"));
        s.cycle_category();
        assert_eq!(s.category_filter, None);
    }

    #[test]
    fn query_filters_and_preserves_selection_by_name() {
        let mut s = UiState::new(&app_over(entries()));
        s.push_query_char('k');
        s.push_query_char('r');
        assert_eq!(s.selected_entry().unwrap().name, "kruby");
    }

    #[test]
    fn set_entries_preserves_selection() {
        let mut s = UiState::new(&app_over(entries()));
        s.move_down();
        let name = s.selected_entry().unwrap().name.clone();
        let mut next = entries();
        next.push(Entry::new("new", Kind::Alias, "echo new", "new"));
        s.set_entries(&app_over(next));
        assert_eq!(s.selected_entry().unwrap().name, name);
    }

    #[test]
    fn scope_cycle_walks_all_then_active_then_each_scope() {
        let dir = TempDir::new().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app(&dir).with_cwd(api.canonicalize().unwrap());
        app.add(
            Entry::new("s", Kind::Alias, "echo s", "d"),
            &Placement::Here,
        )
        .unwrap();

        let mut state = UiState::new(&app);
        assert_eq!(state.scope_filter(), &ScopeFilter::All);
        state.cycle_scope(&app);
        assert_eq!(
            state.scope_filter(),
            &ScopeFilter::ActiveIn(vec!["api".to_string()])
        );
        state.cycle_scope(&app);
        assert_eq!(state.scope_filter(), &ScopeFilter::Only(Some("api".into())));
        state.cycle_scope(&app);
        assert_eq!(state.scope_filter(), &ScopeFilter::All);
    }

    #[test]
    fn inactive_entries_are_marked_inactive() {
        let dir = TempDir::new().unwrap();
        let api = dir.path().join("work/api");
        std::fs::create_dir_all(&api).unwrap();
        let mut app = test_app(&dir).with_cwd(api.canonicalize().unwrap());
        app.add(
            Entry::new("s", Kind::Alias, "echo s", "d"),
            &Placement::Here,
        )
        .unwrap();
        let app = app.with_cwd(dir.path().canonicalize().unwrap());

        let state = UiState::new(&app);
        let scoped = app.store.find_all_named("s")[0];
        assert!(!state.is_active(scoped));

        let global = Entry::new("g", Kind::Alias, "echo g", "d");
        assert!(state.is_active(&global));
    }

    #[test]
    fn the_active_filter_follows_a_scope_created_while_it_is_on() {
        let dir = TempDir::new().unwrap();
        let api = dir.path().join("work/api");
        let web = dir.path().join("work/web");
        std::fs::create_dir_all(&api).unwrap();
        std::fs::create_dir_all(&web).unwrap();
        let mut app = test_app(&dir).with_cwd(api.canonicalize().unwrap());
        app.add(
            Entry::new("a", Kind::Alias, "echo a", "d"),
            &Placement::Here,
        )
        .unwrap();
        let mut app = app.with_cwd(web.canonicalize().unwrap());

        let mut state = UiState::new(&app);
        state.cycle_scope(&app);
        assert_eq!(state.scope_filter(), &ScopeFilter::ActiveIn(Vec::new()));

        let key = app
            .add(
                Entry::new("w", Kind::Alias, "echo w", "d"),
                &Placement::Here,
            )
            .unwrap();
        state.set_entries(&app);

        assert_eq!(
            state.scope_filter(),
            &ScopeFilter::ActiveIn(vec!["web".to_string()])
        );
        assert!(
            state
                .filtered_entries()
                .iter()
                .any(|e| EntryKey::of(e) == key)
        );
    }

    #[test]
    fn selection_survives_a_scope_filter_change_for_duplicate_names() {
        let dir = TempDir::new().unwrap();
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

        let scoped = EntryKey {
            scope: Some("api".into()),
            name: "ll".into(),
        };
        let mut state = UiState::new(&app);
        let steps = position_of(&state, &scoped);
        for _ in 0..steps {
            state.move_down();
        }
        state.cycle_scope(&app);
        state.set_entries(&app);
        assert_eq!(
            state.selected_entry().map(EntryKey::of).unwrap().scope,
            scoped.scope
        );
    }
}
