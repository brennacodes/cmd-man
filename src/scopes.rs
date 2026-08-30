//! The directory scope registry stored in `scopes.toml`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::{self, Paths};

/// Ids the shell dispatcher and the CLI reserve for the unscoped set.
const RESERVED_IDS: [&str; 2] = ["global", "all"];

/// A named directory scope. `path` is stored `$HOME`-relative when possible so
/// the registry travels between machines.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Scope {
    pub id: String,
    pub path: String,
    pub created_at: String,
}

/// On-disk shape of `scopes.toml`: `[[scope]]` tables.
#[derive(Debug, Default, Serialize, Deserialize)]
struct ScopesFile {
    #[serde(default)]
    scope: Vec<Scope>,
}

/// The in-memory scope registry.
///
/// Content problems never fail a load. Scopes that cannot be rendered are
/// still held in memory and reported through [`ScopeRegistry::issues`], so
/// saving after loading never silently drops a hand-edited line.
#[derive(Debug, Default, Clone)]
pub struct ScopeRegistry {
    scopes: Vec<Scope>,
    issues: Vec<String>,
}

impl ScopeRegistry {
    /// Read `scopes.toml`, treating an absent file as an empty registry.
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.scopes_file();
        if !file.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&file)
            .with_context(|| format!("reading {}", file.display()))?;
        let parsed: ScopesFile =
            toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))?;
        Ok(Self::from_scopes(parsed.scope))
    }

    /// Build a registry from scopes in file order, collecting issues.
    pub(crate) fn from_scopes(scopes: Vec<Scope>) -> Self {
        let mut registry = Self {
            scopes,
            issues: Vec::new(),
        };
        registry.revalidate();
        registry
    }

    /// Write the registry to `scopes.toml`, even when it is empty.
    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure_dirs()?;
        let file = ScopesFile {
            scope: self.scopes.clone(),
        };
        let text = toml::to_string_pretty(&file).context("serializing scopes")?;
        let path = paths.scopes_file();
        std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Problems found in the loaded registry, for surfacing as notices.
    pub fn issues(&self) -> &[String] {
        &self.issues
    }

    pub fn scopes(&self) -> &[Scope] {
        &self.scopes
    }

    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty()
    }

    pub fn find(&self, id: &str) -> Option<&Scope> {
        self.scopes.iter().find(|s| s.id == id)
    }

    /// The scope registered for exactly this directory, if any.
    pub fn find_by_path(&self, dir: &Path) -> Option<&Scope> {
        self.scopes
            .iter()
            .find(|s| paths::expand_path(&s.path) == dir)
    }

    /// The absolute directory a scope id points at.
    pub fn resolved_path(&self, id: &str) -> Option<PathBuf> {
        self.find(id).map(|s| paths::expand_path(&s.path))
    }

    /// Build the scope a directory would get, without registering it. The id
    /// is the slugged basename, suffixed until it is free and not reserved.
    pub fn plan_create(&self, dir: &Path, now: &str) -> Scope {
        let base = slug_id(dir);
        let taken: HashSet<&str> = self.scopes.iter().map(|s| s.id.as_str()).collect();
        let mut id = base.clone();
        let mut suffix = 1;
        while RESERVED_IDS.contains(&id.as_str()) || taken.contains(id.as_str()) {
            suffix += 1;
            id = format!("{base}-{suffix}");
        }
        Scope {
            id,
            path: paths::portable_path(dir),
            created_at: now.to_string(),
        }
    }

    pub fn insert(&mut self, scope: Scope) {
        self.scopes.push(scope);
        self.revalidate();
    }

    /// Point an existing scope at a different directory.
    pub fn relocate(&mut self, id: &str, dir: &Path) -> Result<()> {
        let path = paths::portable_path(dir);
        let Some(scope) = self.scopes.iter_mut().find(|s| s.id == id) else {
            bail!("unknown scope '{id}'");
        };
        scope.path = path;
        self.revalidate();
        Ok(())
    }

    /// Drop a scope from the registry and return it.
    pub fn remove(&mut self, id: &str) -> Result<Scope> {
        let Some(idx) = self.scopes.iter().position(|s| s.id == id) else {
            bail!("unknown scope '{id}'");
        };
        let scope = self.scopes.remove(idx);
        self.revalidate();
        Ok(scope)
    }

    /// Every scope containing `cwd`, shallowest first. Two scopes that both
    /// contain a directory are necessarily nested, so depth totally orders
    /// them and the last element is the one that wins.
    pub fn active(&self, cwd: &Path) -> Vec<&Scope> {
        let mut active: Vec<(usize, &Scope)> = self
            .render_order()
            .into_iter()
            .filter_map(|s| {
                let dir = paths::expand_path(&s.path);
                path_contains(&dir, cwd).then(|| (dir.components().count(), s))
            })
            .collect();
        active.sort_by_key(|(depth, _)| *depth);
        active.into_iter().map(|(_, s)| s).collect()
    }

    /// The innermost scope containing `cwd`.
    pub fn deepest_active(&self, cwd: &Path) -> Option<&Scope> {
        self.active(cwd).pop()
    }

    /// Renderable scopes ordered so a parent always precedes its descendants,
    /// with duplicate ids and duplicate directories dropped.
    pub fn render_order(&self) -> Vec<&Scope> {
        self.resolve().0
    }

    fn revalidate(&mut self) {
        self.issues = self.resolve().1;
    }

    fn resolve(&self) -> (Vec<&Scope>, Vec<String>) {
        let mut ordered: Vec<&Scope> = self.scopes.iter().collect();
        ordered.sort_by_key(|s| paths::expand_path(&s.path));

        let mut kept: Vec<&Scope> = Vec::new();
        let mut issues = Vec::new();
        let mut ids: HashSet<&str> = HashSet::new();
        let mut dirs: HashSet<PathBuf> = HashSet::new();
        for scope in ordered {
            if scope.id.trim().is_empty() {
                issues.push(format!(
                    "scope for '{}' has an empty id and is ignored",
                    scope.path
                ));
                continue;
            }
            if ids.contains(scope.id.as_str()) {
                issues.push(format!(
                    "duplicate scope id '{}'; only the first is used",
                    scope.id
                ));
                continue;
            }
            let dir = paths::expand_path(&scope.path);
            if dirs.contains(&dir) {
                issues.push(format!(
                    "duplicate scope path '{}' for scope '{}'; only the first is used",
                    scope.path, scope.id
                ));
                continue;
            }
            ids.insert(scope.id.as_str());
            dirs.insert(dir);
            kept.push(scope);
        }
        (kept, issues)
    }
}

/// Slug a directory basename into a default scope id.
pub fn slug_id(dir: &Path) -> String {
    let base = dir
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let slug = base
        .to_lowercase()
        .trim_start_matches('.')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    if slug.is_empty() {
        "scope".to_string()
    } else {
        slug
    }
}

/// Whether `cwd` is `scope_dir` or lives under it. Compared as text on a
/// component boundary so it matches the generated shell's `case` test exactly.
pub fn path_contains(scope_dir: &Path, cwd: &Path) -> bool {
    let scope_dir = scope_dir.to_string_lossy();
    let scope_dir = scope_dir.trim_end_matches('/');
    let cwd = cwd.to_string_lossy();
    let cwd = cwd.trim_end_matches('/');
    cwd == scope_dir || cwd.starts_with(&format!("{scope_dir}/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    fn scope(id: &str, path: &str) -> Scope {
        Scope {
            id: id.into(),
            path: path.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn registry(scopes: Vec<Scope>) -> ScopeRegistry {
        ScopeRegistry::from_scopes(scopes)
    }

    #[test]
    fn slug_id_uses_the_directory_basename() {
        assert_eq!(slug_id(Path::new("/Users/ada/work/api")), "api");
        assert_eq!(slug_id(Path::new("/Users/ada/work/My Api!")), "my-api");
        assert_eq!(slug_id(Path::new("/Users/ada/.config")), "config");
        assert_eq!(slug_id(Path::new("/")), "scope");
        assert_eq!(slug_id(Path::new("/Users/ada/++")), "scope");
    }

    #[test]
    fn plan_create_dedupes_ids_and_avoids_reserved_words() {
        let reg = registry(vec![scope("api", "/a/api")]);
        assert_eq!(reg.plan_create(Path::new("/b/api"), "now").id, "api-2");

        let reg = registry(vec![scope("api", "/a/api"), scope("api-2", "/b/api")]);
        assert_eq!(reg.plan_create(Path::new("/c/api"), "now").id, "api-3");

        let reg = registry(vec![]);
        assert_eq!(
            reg.plan_create(Path::new("/x/global"), "now").id,
            "global-2"
        );
        assert_eq!(reg.plan_create(Path::new("/x/all"), "now").id, "all-2");
    }

    #[test]
    fn plan_create_does_not_mutate() {
        let mut reg = registry(vec![]);
        let planned = reg.plan_create(Path::new("/x/api"), "now");
        assert!(reg.is_empty());
        reg.insert(planned);
        assert_eq!(reg.scopes().len(), 1);
    }

    #[test]
    fn path_contains_requires_a_component_boundary() {
        assert!(path_contains(Path::new("/foo/bar"), Path::new("/foo/bar")));
        assert!(path_contains(
            Path::new("/foo/bar"),
            Path::new("/foo/bar/baz")
        ));
        assert!(!path_contains(
            Path::new("/foo/bar"),
            Path::new("/foo/barbaz")
        ));
        assert!(!path_contains(Path::new("/foo/bar"), Path::new("/foo")));
        assert!(path_contains(
            Path::new("/foo/bar/"),
            Path::new("/foo/bar/baz")
        ));
    }

    #[test]
    fn active_returns_shallowest_first_and_deepest_wins() {
        let reg = registry(vec![
            scope("api", "/w/api"),
            scope("svc", "/w/api/services/svc"),
            scope("other", "/w/other"),
        ]);
        let cwd = Path::new("/w/api/services/svc/src");
        let ids: Vec<_> = reg.active(cwd).iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["api", "svc"]);
        assert_eq!(reg.deepest_active(cwd).unwrap().id, "svc");
        assert!(reg.active(Path::new("/elsewhere")).is_empty());
        assert!(reg.deepest_active(Path::new("/elsewhere")).is_none());
    }

    #[test]
    fn render_order_sorts_by_path_not_by_id() {
        let reg = registry(vec![scope("zeta", "/aaa"), scope("alpha", "/aaa/bbb")]);
        let ids: Vec<_> = reg.render_order().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["zeta", "alpha"]);
    }

    #[test]
    fn duplicates_are_reported_but_not_dropped() {
        let reg = registry(vec![
            scope("api", "/a/api"),
            scope("api", "/b/api"),
            scope("web", "/a/api"),
        ]);
        assert!(
            reg.issues()
                .iter()
                .any(|i| i.contains("duplicate scope id 'api'"))
        );
        assert!(
            reg.issues()
                .iter()
                .any(|i| i.contains("duplicate scope path"))
        );
        assert_eq!(reg.scopes().len(), 3);
        // `/a/api` is claimed by the first `api`, so `web` is dropped from
        // rendering, and the second `api` is dropped as a duplicate id.
        let rendered: Vec<_> = reg.render_order().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(rendered, vec!["api"]);
    }

    #[test]
    fn relocate_and_remove() {
        let mut reg = registry(vec![scope("api", "$HOME/a")]);
        reg.relocate("api", Path::new("/tmp/moved")).unwrap();
        assert_eq!(reg.find("api").unwrap().path, "/tmp/moved");
        assert!(reg.relocate("nope", Path::new("/tmp/x")).is_err());
        assert_eq!(reg.remove("api").unwrap().id, "api");
        assert!(reg.remove("api").is_err());
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tempfile::TempDir::new().unwrap();
        let paths = Paths::with_root(dir.path().to_path_buf());
        paths.ensure_dirs().unwrap();
        let mut reg = ScopeRegistry::load(&paths).unwrap();
        assert!(reg.is_empty());
        reg.insert(scope("api", "$HOME/work/api"));
        reg.save(&paths).unwrap();
        let again = ScopeRegistry::load(&paths).unwrap();
        assert_eq!(again.find("api").unwrap().path, "$HOME/work/api");
        assert!(again.issues().is_empty());
    }
}
