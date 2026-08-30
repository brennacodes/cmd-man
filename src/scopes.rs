//! The directory scope registry stored in `scopes.toml`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::ScopeDefault;
use crate::paths::{self, Paths};

/// Ids reserved by the shell dispatcher and by the placement keywords, so an
/// auto-generated id can never shadow one of them.
const RESERVED_IDS: [&str; 3] = ["global", "all", "here"];

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
        while is_reserved_id(&id) || taken.contains(id.as_str()) {
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
                    one_line(&scope.path)
                ));
                continue;
            }
            if !is_valid_id(&scope.id) {
                issues.push(format!(
                    "scope id '{}' is not usable and is ignored: use letters, digits, '-' and '_'",
                    one_line(&scope.id)
                ));
                continue;
            }
            if !is_valid_path(&scope.path) {
                issues.push(format!(
                    "scope '{}' has an unusable directory and is ignored",
                    scope.id
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
                    one_line(&scope.path),
                    scope.id
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

/// Whether an id is one of the placement keywords. Such an id would be
/// unreachable through `--in` and `--move-to`, which read those words as
/// keywords rather than as scope ids.
pub fn is_reserved_id(id: &str) -> bool {
    RESERVED_IDS.contains(&id)
}

/// Whether an id is safe to store. Anything outside this character class
/// would be carried verbatim into the generated shell file, where a newline
/// or a quote turns a scope comment into live code.
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Whether a stored scope directory is safe to keep. The path cannot be
/// narrowed to a character class the way an id can, since it is whatever the
/// user's filesystem holds, but a control character in it would end the
/// generated file's scope comment line early and turn the rest into code.
pub fn is_valid_path(path: &str) -> bool {
    !path.is_empty() && !path.chars().any(char::is_control)
}

/// `text` with its control characters replaced by spaces, so a registry value
/// echoed into the generated shell file's scope comment or into a load notice
/// cannot end the line it sits on.
pub fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Make `dir` absolute against `base` and resolve `.` and `..` textually.
/// Never canonicalized: a stored scope path has to keep the shape the shell
/// gives `$PWD`, which is also lexical.
pub fn normalize_dir(base: &Path, dir: &Path) -> PathBuf {
    let joined = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        base.join(dir)
    };
    let absolute = joined.is_absolute();
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() && !absolute {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
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

/// What the user asked for when placing an entry. Distinct from
/// `Entry::scope`, which can only say "global" or "this scope" and has no way
/// to say "the user said nothing".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// Nothing was requested; the configured default decides.
    Default,
    Global,
    /// The directory the command was run from, anchored at its repo root.
    Here,
    Scope(String),
}

impl Placement {
    /// Parses a user-supplied placement string: an empty string is
    /// [`Placement::Default`], `global` and `here` are their keywords, and
    /// anything else is a scope id.
    pub fn parse(text: &str) -> Placement {
        let trimmed = text.trim();
        match trimmed.to_lowercase().as_str() {
            "" => Placement::Default,
            "global" => Placement::Global,
            "here" => Placement::Here,
            _ => Placement::Scope(trimmed.to_string()),
        }
    }
}

/// A resolved placement. `NewAt` is a directory that has no scope yet; only
/// `App::add` turns one into a registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Global,
    Existing(String),
    NewAt(PathBuf),
}

/// Resolve a placement against the registry without mutating anything. Fails
/// only when an explicitly named scope does not exist.
pub fn resolve_placement(
    placement: &Placement,
    mode: ScopeDefault,
    registry: &ScopeRegistry,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<Target> {
    match placement {
        Placement::Global => Ok(Target::Global),
        Placement::Scope(id) => match registry.find(id) {
            Some(scope) => Ok(Target::Existing(scope.id.clone())),
            None => bail!("unknown scope '{id}'"),
        },
        Placement::Here => Ok(target_for_dir(registry, &anchor(cwd, home))),
        Placement::Default => match mode {
            ScopeDefault::Global => Ok(Target::Global),
            ScopeDefault::Repo => {
                let Some(root) = repo_root(cwd, home) else {
                    return Ok(Target::Global);
                };
                if home.is_some_and(|h| same_dir(h, &root)) || root.parent().is_none() {
                    return Ok(Target::Global);
                }
                Ok(target_for_dir(registry, &root))
            }
        },
    }
}

/// Walk up from `start` looking for `.git`, as a directory or as a worktree
/// file, stopping at `home` or the filesystem root.
pub fn repo_root(start: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        if Some(dir) == home {
            return None;
        }
        cur = dir.parent();
    }
    None
}

/// The directory `--here` means: the enclosing repo root when there is one,
/// otherwise the working directory itself. Never canonicalized: the generated
/// dispatcher matches the shell's logical `$PWD`, so a directory reached
/// through a symlink has to be stored the way the shell names it.
fn anchor(cwd: &Path, home: Option<&Path>) -> PathBuf {
    repo_root(cwd, home).unwrap_or_else(|| cwd.to_path_buf())
}

fn target_for_dir(registry: &ScopeRegistry, dir: &Path) -> Target {
    match registry.find_by_path(dir) {
        Some(scope) => Target::Existing(scope.id.clone()),
        None => Target::NewAt(dir.to_path_buf()),
    }
}

/// Whether two paths name the same directory, resolving symlinks so `/var` and
/// `/private/var` compare equal on macOS. A path that does not exist compares
/// as written rather than being an error.
pub fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || canonical(a) == canonical(b)
}

/// A path with its symlinks resolved, or the path as written when it does not
/// exist.
pub fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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

    /// `plan_create` under a fixed timestamp.
    fn plan(reg: &ScopeRegistry, dir: &str) -> Scope {
        reg.plan_create(Path::new(dir), "now")
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
        assert_eq!(plan(&reg, "/b/api").id, "api-2");

        let reg = registry(vec![scope("api", "/a/api"), scope("api-2", "/b/api")]);
        assert_eq!(plan(&reg, "/c/api").id, "api-3");

        let reg = registry(vec![]);
        assert_eq!(plan(&reg, "/x/global").id, "global-2");
        assert_eq!(plan(&reg, "/x/all").id, "all-2");
        assert_eq!(plan(&reg, "/x/here").id, "here-2");
    }

    #[test]
    fn plan_create_does_not_mutate() {
        let mut reg = registry(vec![]);
        let planned = plan(&reg, "/x/api");
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
    fn normalize_dir_absolutizes_and_folds_dot_segments() {
        let base = Path::new("/w/api");
        assert_eq!(
            normalize_dir(base, Path::new("src")),
            Path::new("/w/api/src")
        );
        assert_eq!(
            normalize_dir(base, Path::new("./src")),
            Path::new("/w/api/src")
        );
        assert_eq!(
            normalize_dir(base, Path::new("../other")),
            Path::new("/w/other")
        );
        assert_eq!(
            normalize_dir(base, Path::new("/tmp/./x/..")),
            Path::new("/tmp")
        );
        assert_eq!(normalize_dir(base, Path::new("/../..")), Path::new("/"));
    }

    #[test]
    fn id_validation_rejects_keywords_and_shell_metacharacters() {
        assert!(is_reserved_id("global"));
        assert!(!is_reserved_id("api"));
        assert!(is_valid_id("api"));
        assert!(is_valid_id("api_2-x"));
        assert!(!is_valid_id(""));
        assert!(!is_valid_id("bad\nalias pwned=oops #"));
        assert!(!is_valid_id("a b"));
        assert!(!is_valid_id("a'b"));
    }

    #[test]
    fn path_validation_rejects_control_characters() {
        assert!(is_valid_path("$HOME/work/api"));
        assert!(is_valid_path("/a dir/with spaces & 'quotes'"));
        assert!(!is_valid_path(""));
        assert!(!is_valid_path("/w/zz\nalias pwned='echo OWNED'\n#"));
        assert!(!is_valid_path("/a\rb"));
        assert_eq!(one_line("a\nb\tc"), "a b c");
    }

    #[test]
    fn a_scope_that_could_inject_shell_code_is_dropped_at_load() {
        let injected_id = "api\nalias pwned='echo PWNED'\nfoo";
        let injected_path = "/w/zz\nalias pwned='echo PWNED'\n#";
        let reg = registry(vec![
            scope(injected_id, "/w/one"),
            scope("two", injected_path),
            scope("ok", "/w/three"),
        ]);

        let rendered: Vec<_> = reg.render_order().iter().map(|s| s.id.as_str()).collect();
        assert_eq!(rendered, vec!["ok"]);
        let issues = reg.issues();
        assert!(
            issues
                .iter()
                .any(|i| i.contains("is not usable and is ignored")),
            "issues were: {issues:?}"
        );
        assert!(
            issues
                .iter()
                .any(|i| i.contains("has an unusable directory")),
            "issues were: {issues:?}"
        );
        // A notice is one line, so a value quoted into it cannot fake a second.
        assert!(
            issues.iter().all(|i| !i.contains('\n')),
            "issues were: {issues:?}"
        );
        // Kept in memory so a save after a load never drops a hand-edited line.
        assert_eq!(reg.scopes().len(), 3);
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

    #[test]
    fn repo_root_finds_a_git_dir_or_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("proj");
        let deep = root.join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(repo_root(&deep, None).unwrap(), root);

        let wt = dir.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(repo_root(&wt, None).unwrap(), wt);

        let bare = dir.path().join("nothing");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(repo_root(&bare, Some(dir.path())).is_none());
    }

    #[test]
    fn repo_root_stops_at_home() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join("home");
        let inside = home.join("proj");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        assert!(repo_root(&inside, Some(&home)).is_none());
    }

    #[test]
    fn global_mode_default_is_global_even_inside_a_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let reg = ScopeRegistry::default();
        let target =
            resolve_placement(&Placement::Default, ScopeDefault::Global, &reg, &root, None)
                .unwrap();
        assert_eq!(target, Target::Global);
    }

    #[test]
    fn repo_mode_default_targets_the_repo_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("proj");
        let deep = root.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let reg = ScopeRegistry::default();
        let target =
            resolve_placement(&Placement::Default, ScopeDefault::Repo, &reg, &deep, None).unwrap();
        assert_eq!(target, Target::NewAt(root));
    }

    #[test]
    fn repo_mode_default_falls_back_to_global_outside_a_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg = ScopeRegistry::default();
        let target = resolve_placement(
            &Placement::Default,
            ScopeDefault::Repo,
            &reg,
            dir.path(),
            Some(dir.path()),
        )
        .unwrap();
        assert_eq!(target, Target::Global);
    }

    #[test]
    fn repo_mode_never_scopes_to_home_or_root() {
        let dir = tempfile::TempDir::new().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".git")).unwrap();
        let reg = ScopeRegistry::default();
        let target = resolve_placement(
            &Placement::Default,
            ScopeDefault::Repo,
            &reg,
            &home,
            Some(&home),
        )
        .unwrap();
        assert_eq!(target, Target::Global);
    }

    #[test]
    fn here_reuses_an_existing_scope_and_ignores_the_home_guard() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join("proj");
        let deep = root.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let reg = ScopeRegistry::default();
        assert_eq!(
            resolve_placement(&Placement::Here, ScopeDefault::Global, &reg, &deep, None).unwrap(),
            Target::NewAt(root.clone())
        );

        let reg = ScopeRegistry::from_scopes(vec![Scope {
            id: "proj".into(),
            path: root.display().to_string(),
            created_at: "t".into(),
        }]);
        assert_eq!(
            resolve_placement(&Placement::Here, ScopeDefault::Global, &reg, &deep, None).unwrap(),
            Target::Existing("proj".into())
        );
    }

    #[test]
    fn here_uses_cwd_outside_a_repo() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg = ScopeRegistry::default();
        assert_eq!(
            resolve_placement(
                &Placement::Here,
                ScopeDefault::Global,
                &reg,
                dir.path(),
                Some(dir.path())
            )
            .unwrap(),
            Target::NewAt(dir.path().to_path_buf())
        );
    }

    #[test]
    fn placement_keeps_the_symlinked_path_the_shell_would_report() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("actual/proj");
        std::fs::create_dir_all(real.join(".git")).unwrap();
        let link = dir.path().join("proj");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let reg = ScopeRegistry::default();
        assert_eq!(
            resolve_placement(&Placement::Here, ScopeDefault::Global, &reg, &link, None).unwrap(),
            Target::NewAt(link.clone())
        );
        assert_eq!(
            resolve_placement(&Placement::Default, ScopeDefault::Repo, &reg, &link, None).unwrap(),
            Target::NewAt(link)
        );
    }

    #[test]
    fn repo_mode_home_guard_sees_through_a_symlinked_home() {
        let dir = tempfile::TempDir::new().unwrap();
        let real = dir.path().join("actual/home");
        std::fs::create_dir_all(real.join(".git")).unwrap();
        let link = dir.path().join("home");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let reg = ScopeRegistry::default();
        assert_eq!(
            resolve_placement(
                &Placement::Default,
                ScopeDefault::Repo,
                &reg,
                &link,
                Some(&real)
            )
            .unwrap(),
            Target::Global
        );
    }

    #[test]
    fn explicit_flags_win_in_both_modes() {
        let dir = tempfile::TempDir::new().unwrap();
        let reg = ScopeRegistry::from_scopes(vec![Scope {
            id: "api".into(),
            path: "/w/api".into(),
            created_at: "t".into(),
        }]);
        for mode in [ScopeDefault::Global, ScopeDefault::Repo] {
            assert_eq!(
                resolve_placement(&Placement::Global, mode, &reg, dir.path(), None).unwrap(),
                Target::Global
            );
            assert_eq!(
                resolve_placement(
                    &Placement::Scope("api".into()),
                    mode,
                    &reg,
                    dir.path(),
                    None
                )
                .unwrap(),
                Target::Existing("api".into())
            );
        }
        assert!(
            resolve_placement(
                &Placement::Scope("nope".into()),
                ScopeDefault::Global,
                &reg,
                dir.path(),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn placement_parses_from_a_string() {
        assert_eq!(Placement::parse(""), Placement::Default);
        assert_eq!(Placement::parse("  "), Placement::Default);
        assert_eq!(Placement::parse("global"), Placement::Global);
        assert_eq!(Placement::parse("here"), Placement::Here);
        assert_eq!(Placement::parse("api"), Placement::Scope("api".into()));
    }
}
