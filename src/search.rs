//! Searching and filtering over stored entries.

use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization, Pattern};
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

/// Filters narrowing the candidate set before any matching happens.
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

/// Which field a hit came from. `rank` tries the fields in this order and stops
/// at the first that matches, and `search` sorts on this before the score, so a
/// name hit outranks a body hit however the two happen to score.
type Rank = u8;

const RANK_NAME: Rank = 0;
const RANK_TAGS: Rank = 1;
const RANK_DESCRIPTION: Rank = 2;
/// The category or the scope id, whichever scores higher.
const RANK_PLACEMENT: Rank = 3;
/// Every searchable field at once, the command body included.
const RANK_REST: Rank = 4;

/// The query compiled once per kind of field, because the fields are not the
/// same kind of text and must not be matched the same way.
///
/// Short identifiers (name, each tag, category, scope) get subsequence
/// matching: that is what makes `laud` find `list-app-updates-disabled`. Prose
/// and code get literal substring matching, because a subsequence over a long
/// command body is satisfied by letters scattered across unrelated words -
/// `devin` is a subsequence of `2>/dev/null | xargs -I {} basename {} .plist` -
/// and a hit like that is never what was typed.
///
/// Both patterns are built from one `Pattern::parse`, never from a second
/// constructor over the same string, because `Pattern::new` does not read
/// nucleo's query syntax and the two would then disagree about what was typed:
/// `'foo` would ask for the substring `foo` in one and for the four literal
/// characters `'foo` in the other. Only the matching algorithm is rewritten, so
/// an atom spelled `'foo`, `^foo` or `foo$` keeps the kind it asked for in
/// every field.
struct Query {
    /// Positive atoms as typed.
    fuzzy: Pattern,
    /// The same atoms with plain subsequence matching downgraded to a literal
    /// substring.
    substring: Pattern,
    /// Negated atoms (`!foo`). These exclude rather than rank, so they are
    /// applied once against every field at once: `!foo` has to mean the entry
    /// does not mention `foo` anywhere, not that its name does not.
    ///
    /// They skip the rewrite below only because nucleo already downgrades an
    /// inverted fuzzy atom to a substring when it parses one. Without that,
    /// negation would be the scattered-letter match in reverse and `!devin`
    /// would hide every entry redirecting to `/dev/null`.
    excluded: Pattern,
}

fn pattern_of(atoms: Vec<Atom>) -> Pattern {
    // `Pattern` is `#[non_exhaustive]`, so it cannot be built from a literal.
    let mut pattern = Pattern::default();
    pattern.atoms = atoms;
    pattern
}

impl Query {
    fn new(raw: &str) -> Self {
        let (excluded, positive): (Vec<Atom>, Vec<Atom>) =
            Pattern::parse(raw, CaseMatching::Ignore, Normalization::Smart)
                .atoms
                .into_iter()
                .partition(|atom| atom.negative);
        let substring = positive
            .iter()
            .cloned()
            .map(|mut atom| {
                if atom.kind == AtomKind::Fuzzy {
                    atom.kind = AtomKind::Substring;
                }
                atom
            })
            .collect();
        Self {
            fuzzy: pattern_of(positive),
            substring: pattern_of(substring),
            excluded: pattern_of(excluded),
        }
    }
}

fn score(pattern: &Pattern, text: &str, matcher: &mut Matcher, buf: &mut Vec<char>) -> Option<u32> {
    pattern.score(Utf32Str::new(text, buf), matcher)
}

/// The highest-priority field this entry matches in, which is not necessarily
/// the highest-scoring one. `None` when the entry matches nowhere or is
/// excluded outright. A query of nothing but negations has no field to match,
/// so every entry it does not exclude comes back at the first tier with a zero
/// score and the ordering falls through to the tiebreak.
fn rank(
    entry: &Entry,
    query: &Query,
    matcher: &mut Matcher,
    buf: &mut Vec<char>,
) -> Option<(Rank, u32)> {
    // Only the negation check and the last tier read the whole entry, so the
    // joined string is built once and only when one of them asks for it.
    let haystack = if query.excluded.atoms.is_empty() {
        None
    } else {
        let haystack = entry.search_haystack();
        // A negated atom scores when it is absent, so this is `None` exactly
        // when one of the excluded terms appears somewhere in the entry.
        score(&query.excluded, &haystack, matcher, buf)?;
        Some(haystack)
    };
    if let Some(s) = score(&query.fuzzy, &entry.name, matcher, buf) {
        return Some((RANK_NAME, s));
    }
    // Per tag rather than over the joined tags, which would be a long string
    // again and would let a match draw its letters from unrelated tags.
    let tags = entry
        .tags
        .iter()
        .filter_map(|tag| score(&query.fuzzy, tag, matcher, buf))
        .max();
    if let Some(s) = tags {
        return Some((RANK_TAGS, s));
    }
    if let Some(s) = score(&query.substring, &entry.description, matcher, buf) {
        return Some((RANK_DESCRIPTION, s));
    }
    let category = score(&query.fuzzy, &entry.category, matcher, buf);
    let scope = match entry.scope.as_deref() {
        Some(id) => score(&query.fuzzy, id, matcher, buf),
        None => None,
    };
    if let Some(s) = category.max(scope) {
        return Some((RANK_PLACEMENT, s));
    }
    // This is the only tier that reaches the command body, and the only one a
    // multi-word query can span, since every atom has to match the same field.
    let haystack = haystack.unwrap_or_else(|| entry.search_haystack());
    score(&query.substring, &haystack, matcher, buf).map(|s| (RANK_REST, s))
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
    let query = Query::new(query);
    let mut buf = Vec::new();
    let mut scored: Vec<(Rank, u32, &Entry)> = Vec::new();
    for entry in candidates {
        if let Some((r, s)) = rank(entry, &query, &mut matcher, &mut buf) {
            scored.push((r, s, entry));
        }
    }
    // Best field first, then higher score; stable tiebreak on (scope, name).
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.2.scope.cmp(&b.2.scope))
            .then_with(|| a.2.name.cmp(&b.2.name))
    });
    scored.into_iter().map(|(_, _, e)| e).collect()
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
        assert_eq!(out[0].name, "gitsw");
    }

    #[test]
    fn an_entry_matching_nowhere_is_left_out() {
        let entries = sample();
        let out = search(&entries, "git", &Filter::default());
        assert!(!out.is_empty());
        assert!(out.iter().all(|e| e.name.starts_with("g")));
    }

    /// Within one tier the better score comes first. The fixture only shows
    /// that while the scores really differ, so it asserts that too: the
    /// alphabetical tiebreak orders these the other way round, so an assertion
    /// on order alone would pass even with the score comparison removed.
    #[test]
    fn a_better_score_comes_first_within_a_tier() {
        let entries = vec![
            Entry::new("a-git-thing", Kind::Alias, "echo hi", "d"),
            Entry::new("git", Kind::Alias, "echo hi", "d"),
        ];
        let query = Query::new("git");
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let buried = rank(&entries[0], &query, &mut matcher, &mut buf).unwrap();
        let whole = rank(&entries[1], &query, &mut matcher, &mut buf).unwrap();

        assert_eq!(buried.0, RANK_NAME);
        assert_eq!(whole.0, RANK_NAME);
        assert!(
            whole.1 > buried.1,
            "fixture no longer separates the scores ({} vs {}), so it cannot \
             show that the score is compared at all",
            whole.1,
            buried.1
        );

        let out = search(&entries, "git", &Filter::default());
        assert_eq!(out[0].name, "git");
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

    // --- field-matching correctness ---

    fn dev_null_entry(name: &str) -> Entry {
        Entry::new(
            name,
            Kind::Function,
            "defaults read com.apple.SoftwareUpdate AutomaticCheckEnabled 2>/dev/null | grep -i true",
            "check software update setting",
        )
    }

    #[test]
    fn letters_scattered_across_a_body_are_not_a_match() {
        // The letters of "devin" are a subsequence of this entry once "2>/dev/null"
        // supplies d, e, v, so the whole-entry fuzzy match the old code did put it
        // alongside the entry actually named "devin".
        let entries = vec![
            dev_null_entry("list-app-updates-disabled"),
            Entry::new("devin", Kind::Alias, "echo devin", "the entry named devin"),
        ];
        let out = search(&entries, "devin", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "devin");
    }

    #[test]
    fn letters_scattered_across_a_description_are_not_a_match() {
        // A description match requires the text to contain the literal substring.
        let entries = vec![
            Entry::new(
                "foo",
                Kind::Alias,
                "echo hi",
                "does interesting various ingested networking",
            ),
            Entry::new("devin", Kind::Alias, "echo hi", "the actual devin entry"),
        ];
        let out = search(&entries, "devin", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "devin");
    }

    #[test]
    fn letters_scattered_across_separate_tags_are_not_a_match() {
        // Tags are scored individually - letters cannot be drawn from different tags.
        let mut e = Entry::new("foo", Kind::Alias, "echo hi", "d");
        e.tags = vec!["dev".into(), "info".into()]; // 'd','e','v' in first; 'i','n' in second
        let entries = vec![e, Entry::new("devin", Kind::Alias, "echo hi", "d")];
        let out = search(&entries, "devin", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "devin");
    }

    #[test]
    fn a_body_is_still_reachable_by_the_text_it_contains() {
        // Literal substring matches in the body should still surface entries.
        let entries = vec![
            dev_null_entry("check-updates"),
            Entry::new("unrelated", Kind::Alias, "echo hi", "d"),
        ];
        let out = search(&entries, "dev/null", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "check-updates");
    }

    /// Rank is compared before score, so a name hit wins even when a hit in a
    /// lower tier scores higher. The fixture only tests that while the scores
    /// really are inverted, so it asserts the inversion rather than trusting it.
    #[test]
    fn a_name_hit_outranks_a_higher_scoring_lower_tier_hit() {
        let entries = vec![
            // Matches on the description, exactly and in full: a high score.
            Entry::new("zz", Kind::Alias, "echo hi", "deploy"),
            // Matches on the name, buried in a long one: a lower score.
            Entry::new(
                "run-the-deployment-for-staging",
                Kind::Alias,
                "echo hi",
                "d",
            ),
        ];
        let query = Query::new("deploy");
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let described = rank(&entries[0], &query, &mut matcher, &mut buf).unwrap();
        let named = rank(&entries[1], &query, &mut matcher, &mut buf).unwrap();

        assert_eq!(described.0, RANK_DESCRIPTION);
        assert_eq!(named.0, RANK_NAME);
        assert!(
            described.1 > named.1,
            "fixture no longer inverts the scores ({} vs {}), so it cannot show \
             that rank is compared first",
            described.1,
            named.1
        );

        let out = search(&entries, "deploy", &Filter::default());
        assert_eq!(out[0].name, "run-the-deployment-for-staging");
    }

    #[test]
    fn a_description_hit_outranks_a_body_hit() {
        let desc_entry = Entry::new(
            "foo",
            Kind::Alias,
            "echo hi",
            "deploy the application to production",
        );
        let body_entry = Entry::new(
            "bar",
            Kind::Function,
            "make deploy && notify-send 'task done'",
            "build and release",
        );
        let entries = vec![body_entry, desc_entry];
        let out = search(&entries, "deploy", &Filter::default());
        assert_eq!(out[0].name, "foo");
    }

    #[test]
    fn names_still_match_as_a_subsequence() {
        // "laud" should find "list-app-updates-disabled" via subsequence on name.
        let entries = vec![
            Entry::new(
                "list-app-updates-disabled",
                Kind::Alias,
                "defaults read ...",
                "list disabled app updates",
            ),
            Entry::new("unrelated", Kind::Alias, "echo hi", "d"),
        ];
        let out = search(&entries, "laud", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "list-app-updates-disabled");
    }

    #[test]
    fn a_tag_still_matches_as_a_subsequence() {
        let mut e = Entry::new("foo", Kind::Alias, "echo hi", "d");
        e.tags = vec!["networking".into()];
        let entries = vec![e];
        let out = search(&entries, "net", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "foo");
    }

    /// Category and scope stay on subsequence matching deliberately, even
    /// though almost every entry carries the default category and a query that
    /// is a subsequence of it therefore returns the lot. Narrowing them to a
    /// substring was considered and declined.
    #[test]
    fn a_subsequence_of_the_default_category_matches_every_entry_in_it() {
        let entries = vec![
            Entry::new("aa", Kind::Alias, "echo hi", "d"),
            Entry::new("bb", Kind::Alias, "echo hi", "d"),
        ];
        assert_eq!(entries[0].category, "general");
        let out = search(&entries, "gnl", &Filter::default());
        assert_eq!(out.len(), 2);
    }

    /// Both halves of the placement tier match as a subsequence. The queries
    /// are subsequences that are not substrings, so this pins the matching
    /// mode and not merely the fact that the two fields are consulted.
    #[test]
    fn a_category_and_a_scope_id_both_match_as_subsequences() {
        let mut cat_entry = Entry::new("foo", Kind::Alias, "echo hi", "d");
        cat_entry.category = "devtools".into();
        let mut scope_entry = scoped("bar", "devops");
        scope_entry.category = "misc".into();
        let entries = vec![cat_entry, scope_entry];

        let out = search(&entries, "dvts", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "foo");

        let out = search(&entries, "dvps", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "bar");
    }

    #[test]
    fn a_query_is_matched_case_insensitively() {
        let entries = vec![Entry::new("MyAlias", Kind::Alias, "echo hi", "d")];
        let out = search(&entries, "myalias", &Filter::default());
        assert_eq!(out.len(), 1);
        let out = search(&entries, "MYALIAS", &Filter::default());
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_negated_term_excludes_on_any_field_including_the_body() {
        let body_entry = dev_null_entry("check-updates");
        let clean_entry = Entry::new("gst", Kind::Alias, "git status", "show git status");
        let entries = vec![body_entry, clean_entry];
        // "!dev/null" should exclude check-updates (it contains "dev/null" in body)
        let out = search(&entries, "!dev/null", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "gst");
    }

    #[test]
    fn a_quoted_atom_still_reaches_the_body() {
        // A `'` prefix asks nucleo for a substring, and that has to survive
        // into both patterns rather than becoming a literal `'` in one of them.
        let entries = vec![
            Entry::new("bar", Kind::Function, "echo foobar", "d"),
            Entry::new("unrelated", Kind::Alias, "echo hi", "d"),
        ];
        let out = search(&entries, "'foobar", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "bar");
    }

    #[test]
    fn an_anchored_atom_matches_the_name() {
        // ^git should match names starting with "git".
        let entries = vec![
            Entry::new("git-sw", Kind::Alias, "git switch", "d"),
            Entry::new("my-git", Kind::Alias, "git ...", "d"),
        ];
        let out = search(&entries, "^git", &Filter::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "git-sw");
    }

    #[test]
    fn the_placement_tier_sits_between_the_description_and_the_body() {
        let mut placed = Entry::new("zz", Kind::Alias, "echo hi", "d");
        placed.category = "deploy".into();
        let entries = vec![
            Entry::new("yy", Kind::Function, "make deploy", "build and release"),
            placed,
            Entry::new("xx", Kind::Alias, "echo hi", "deploy to production"),
        ];
        let names: Vec<_> = search(&entries, "deploy", &Filter::default())
            .iter()
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(names, vec!["xx", "zz", "yy"]);
    }

    #[test]
    fn the_best_scoring_tag_is_the_one_that_counts() {
        // Both tags match; the score kept must be the better of the two, which
        // is the exact one rather than the one it is only a subsequence of.
        let mut e = Entry::new("foo", Kind::Alias, "echo hi", "d");
        e.tags = vec!["deeply-nested-plumbing".into(), "dep".into()];
        let mut single = Entry::new("bar", Kind::Alias, "echo hi", "d");
        single.tags = vec!["dep".into()];

        let query = Query::new("dep");
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let both = rank(&e, &query, &mut matcher, &mut buf).unwrap();
        let one = rank(&single, &query, &mut matcher, &mut buf).unwrap();
        assert_eq!(both.0, RANK_TAGS);
        assert_eq!(both, one, "the weaker tag dragged the score down");
    }

    #[test]
    fn a_negated_term_excludes_literally_and_not_as_a_subsequence() {
        // The scattered-letter match in reverse: "devin" is a subsequence of
        // this entry, so a fuzzy negation would hide it.
        let entries = vec![dev_null_entry("check-updates")];
        let out = search(&entries, "!devin", &Filter::default());
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn a_query_of_only_negations_returns_everything_it_does_not_exclude() {
        let entries = vec![
            dev_null_entry("check-updates"),
            Entry::new("gst", Kind::Alias, "git status", "show git status"),
            Entry::new("gsw", Kind::Alias, "git switch", "switch branches"),
        ];
        let names: Vec<_> = search(&entries, "!dev/null", &Filter::default())
            .iter()
            .map(|e| e.name.clone())
            .collect();
        assert_eq!(names, vec!["gst", "gsw"]);
    }

    #[test]
    fn every_word_of_a_query_has_to_match_the_same_field() {
        // "gitsw" only matches the name and only as a subsequence, while
        // "branches" only appears in the description, so no single field
        // carries both and the entry does not match.
        let entries = vec![Entry::new(
            "gitsw",
            Kind::Alias,
            "git switch",
            "switch branches",
        )];
        assert_eq!(search(&entries, "gtsw", &Filter::default()).len(), 1);
        assert_eq!(search(&entries, "branches", &Filter::default()).len(), 1);
        assert!(search(&entries, "gtsw branches", &Filter::default()).is_empty());
        // The whole-entry tier still carries a multi-word query when every word
        // appears literally somewhere in the entry.
        assert_eq!(
            search(&entries, "gitsw branches", &Filter::default()).len(),
            1
        );
    }
}
