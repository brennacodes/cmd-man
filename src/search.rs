//! Fuzzy searching and filtering over stored entries.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::model::{Entry, Kind};

/// Filters applied before/after fuzzy matching.
#[derive(Debug, Clone, Default)]
pub struct Filter {
    pub category: Option<String>,
    pub kind: Option<Kind>,
}

impl Filter {
    fn accepts(&self, entry: &Entry) -> bool {
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
/// all (filtered) entries sorted by name.
pub fn search<'a>(entries: &'a [Entry], query: &str, filter: &Filter) -> Vec<&'a Entry> {
    let candidates: Vec<&Entry> = entries.iter().filter(|e| filter.accepts(e)).collect();

    if query.trim().is_empty() {
        let mut out = candidates;
        out.sort_by(|a, b| a.name.cmp(&b.name));
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
    // Higher score first; stable tiebreak on name for determinism.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    scored.into_iter().map(|(_, e)| e).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            category: None,
        };
        let out = search(&entries, "", &f);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "kruby");

        let f = Filter {
            kind: None,
            category: Some("ruby".into()),
        };
        let out = search(&entries, "", &f);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "kruby");
    }
}
