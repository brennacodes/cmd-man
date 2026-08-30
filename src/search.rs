//! Fuzzy searching and filtering over stored entries.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::model::{Entry, Kind};

/// Which scopes a result set is allowed to draw from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ScopeFilter {
    /// Every entry regardless of scope.
    #[default]
    All,
    /// Globals plus entries in the listed scope ids.
    ActiveIn(Vec<String>),
    /// Only entries in this scope; `None` means only globals.
    Only(Option<String>),
}

impl ScopeFilter {
    fn accepts(&self, entry: &Entry) -> bool {
        match self {
            ScopeFilter::All => true,
            ScopeFilter::ActiveIn(ids) => match &entry.scope {
                None => true,
                Some(scope) => ids.iter().any(|id| id == scope),
            },
            ScopeFilter::Only(want) => entry.scope.as_deref() == want.as_deref(),
        }
    }
}

/// Filters applied before/after fuzzy matching.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub category: Option<String>,
    pub kind: Option<Kind>,
    pub scope: ScopeFilter,
}

impl Filter {
    fn accepts(&self, entry: &Entry) -> bool {
        if !self.scope.accepts(entry) {
            return false;
        }
        if let Some(cat) = &self.category
            && &entry.category != cat
        {
            return false;
        }
        if let Some(kind) = self.kind
            && entry.kind != kind
        {
            return false;
        }
        true
    }
}

/// Return entries matching `query`, best matches first. An empty query returns
/// all (filtered) entries sorted by scope then name.
pub fn search<'a>(entries: &'a [Entry], query: &str, filter: &Filter) -> Vec<&'a Entry> {
    let candidates: Vec<&Entry> = entries.iter().filter(|e| filter.accepts(e)).collect();

    if query.trim().is_empty() {
        let mut out = candidates;
        out.sort_by(|a, b| a.scope.cmp(&b.scope).then_with(|| a.name.cmp(&b.name)));
        return out;
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, &Entry)> = Vec::new();
    for entry in candidates {
        let hay = entry.search_haystack();
        let utf32 = Utf32Str::new(&hay, &mut buf);
        if let Some(score) = pattern.score(utf32, &mut matcher) {
            scored.push((score, entry));
        }
    }
    // Higher score first; stable tiebreak on (scope, name) for determinism.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.scope.cmp(&b.1.scope))
            .then_with(|| a.1.name.cmp(&b.1.name))
    });
    scored.into_iter().map(|(_, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global(name: &str) -> Entry {
        Entry::new(name, Kind::Alias, "echo hi", "d")
    }

    fn scoped(name: &str, scope: &str) -> Entry {
        let mut entry = global(name);
        entry.scope = Some(scope.into());
        entry
    }

    fn sample() -> Vec<Entry> {
        vec![
            Entry::new("gitsw", Kind::Alias, "git switch", "switch branches"),
            Entry::new("gst", Kind::Alias, "git status", "show status"),
            {
                let mut e = Entry::new("kruby", Kind::Function, "kill ruby", "kill ruby server");
                e.category = "ruby".into();
                e
            },
        ]
    }

    #[test]
    fn empty_query_returns_all_sorted() {
        let entries = sample();
        let out = search(&entries, "", &Filter::default());
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].name, "gitsw"); // g < k, and gitsw vs gst -> "gitsw" < "gst"
    }

    #[test]
    fn fuzzy_matches_rank_relevant_first() {
        let entries = sample();
        let out = search(&entries, "git", &Filter::default());
        assert!(!out.is_empty());
        assert!(out.iter().all(|e| e.name.starts_with("g")));
    }

    #[test]
    fn filter_by_kind_and_category() {
        let entries = sample();
        let f = Filter {
            kind: Some(Kind::Function),
            ..Filter::default()
        };
        let out = search(&entries, "", &f);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "kruby");

        let f = Filter {
            category: Some("ruby".into()),
            ..Filter::default()
        };
        let out = search(&entries, "", &f);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "kruby");
    }

    #[test]
    fn active_in_shows_globals_plus_the_listed_scopes() {
        let entries = vec![global("a"), scoped("b", "api"), scoped("c", "web")];
        let filter = Filter {
            scope: ScopeFilter::ActiveIn(vec!["api".into()]),
            ..Filter::default()
        };
        let names: Vec<_> = search(&entries, "", &filter)
            .iter()
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn only_restricts_to_one_scope_or_to_globals() {
        let entries = vec![global("a"), scoped("b", "api")];
        let only_api = Filter {
            scope: ScopeFilter::Only(Some("api".into())),
            ..Filter::default()
        };
        let names: Vec<_> = search(&entries, "", &only_api)
            .iter()
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(names, vec!["b"]);

        let only_global = Filter {
            scope: ScopeFilter::Only(None),
            ..Filter::default()
        };
        let names: Vec<_> = search(&entries, "", &only_global)
            .iter()
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(names, vec!["a"]);
    }

    #[test]
    fn results_are_ordered_by_scope_then_name() {
        let entries = vec![scoped("ll", "web"), scoped("ll", "api"), global("ll")];
        let out = search(&entries, "", &Filter::default());
        let scopes: Vec<_> = out.iter().map(|e| e.scope.clone()).collect();
        assert_eq!(scopes, vec![None, Some("api".into()), Some("web".into())]);
    }

    #[test]
    fn fuzzy_results_tiebreak_by_scope_then_name() {
        let entries = vec![scoped("ll", "web"), scoped("ll", "api")];
        let out = search(&entries, "ll", &Filter::default());
        assert_eq!(out[0].scope.as_deref(), Some("api"));
        assert_eq!(out[1].scope.as_deref(), Some("web"));
    }
}
