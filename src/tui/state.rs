//! Pure UI state and update logic for the TUI (no terminal or IO).

use std::collections::HashMap;

use crate::model::{Entry, Kind};
use crate::search::{Filter, search};

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
    Delete(String),
    /// User asked to capture; the run loop decides if confirmation is needed.
    CaptureRequest(String),
    /// Capture confirmed in a popup.
    CaptureConfirmed(String),
    /// Submit an add (original == None) or edit (original == Some(old_name)).
    Submit {
        original: Option<String>,
        entry: Box<Entry>,
        /// Whether to auto-fill empty help fields before saving.
        autofill: bool,
    },
}

/// What a confirmation popup is about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmKind {
    Delete(String),
    Capture(String),
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
    pub mode: Mode,
    pub status: String,
}

impl UiState {
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut state = UiState {
            entries,
            filtered: Vec::new(),
            selected: 0,
            query: String::new(),
            kind_filter: None,
            category_filter: None,
            categories: Vec::new(),
            mode: Mode::Browse,
            status: "? for help".to_string(),
        };
        state.rebuild_categories();
        state.refilter();
        state
    }

    /// Replace entries (after a mutation), preserving the selected item by name.
    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        let current_name = self.selected_entry().map(|e| e.name.clone());
        self.entries = entries;
        self.rebuild_categories();
        // Drop a category filter that no longer exists.
        if let Some(cat) = &self.category_filter
            && !self.categories.contains(cat)
        {
            self.category_filter = None;
        }
        self.refilter();
        if let Some(name) = current_name
            && let Some(pos) = self
                .filtered
                .iter()
                .position(|&i| self.entries[i].name == name)
        {
            self.selected = pos;
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
        };
        let index: HashMap<&str, usize> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.as_str(), i))
            .collect();
        let ordered = search(&self.entries, &self.query, &filter);
        self.filtered = ordered
            .iter()
            .filter_map(|e| index.get(e.name.as_str()).copied())
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
        format!("kind:{kind}  category:{cat}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut s = UiState::new(entries());
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
        let mut s = UiState::new(entries());
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
        let mut s = UiState::new(entries());
        s.cycle_category();
        assert_eq!(s.category_filter.as_deref(), Some("git"));
        s.cycle_category();
        assert_eq!(s.category_filter.as_deref(), Some("ruby"));
        s.cycle_category();
        assert_eq!(s.category_filter, None);
    }

    #[test]
    fn query_filters_and_preserves_selection_by_name() {
        let mut s = UiState::new(entries());
        s.push_query_char('k');
        s.push_query_char('r');
        assert_eq!(s.selected_entry().unwrap().name, "kruby");
    }

    #[test]
    fn set_entries_preserves_selection() {
        let mut s = UiState::new(entries());
        s.move_down();
        let name = s.selected_entry().unwrap().name.clone();
        let mut next = entries();
        next.push(Entry::new("new", Kind::Alias, "echo new", "new"));
        s.set_entries(next);
        assert_eq!(s.selected_entry().unwrap().name, name);
    }
}
