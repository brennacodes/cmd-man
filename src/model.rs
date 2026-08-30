//! Core data types for stored aliases and functions.

use crate::lint;
use crate::shell::Shell;
use serde::{Deserialize, Serialize};

/// Whether a stored entry is a simple alias or a shell function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Alias,
    Function,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Alias => "alias",
            Kind::Function => "function",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Kind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "alias" => Ok(Kind::Alias),
            "function" | "func" | "fn" => Ok(Kind::Function),
            other => Err(format!(
                "unknown kind '{other}' (expected alias or function)"
            )),
        }
    }
}

/// Controls whether example output may be captured automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum CapturePolicy {
    /// Safe to run without asking.
    Auto,
    /// Requires explicit confirmation before running.
    #[default]
    Confirm,
    /// Never run this entry to capture output.
    Never,
}

/// The default category used when none is provided.
pub const DEFAULT_CATEGORY: &str = "general";

/// A single managed alias or function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The shell alias the user types (e.g. `gitsw`). Unique within its scope.
    pub name: String,
    /// The id of the directory scope this entry belongs to, or `None` when the
    /// entry is global.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub kind: Kind,
    /// For an alias, the expansion. For a function, the body.
    pub command: String,
    pub description: String,
    #[serde(default = "default_category")]
    pub category: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub usage: String,
    #[serde(default)]
    pub options: String,
    #[serde(default)]
    pub examples: String,
    #[serde(default)]
    pub example_output: String,
    #[serde(default)]
    pub destructive: bool,
    #[serde(default)]
    pub capture_policy: CapturePolicy,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

fn default_category() -> String {
    DEFAULT_CATEGORY.to_string()
}

impl Entry {
    /// Build a new entry with sensible defaults for optional fields.
    pub fn new(
        name: impl Into<String>,
        kind: Kind,
        command: impl Into<String>,
        description: impl Into<String>,
    ) -> Self {
        Entry {
            name: name.into(),
            scope: None,
            kind,
            command: command.into(),
            description: description.into(),
            category: DEFAULT_CATEGORY.to_string(),
            tags: Vec::new(),
            usage: String::new(),
            options: String::new(),
            examples: String::new(),
            example_output: String::new(),
            destructive: false,
            capture_policy: CapturePolicy::default(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// Whether the alias is a plain shell word, and so safe to write verbatim
    /// into a generated shell file. Entries reach the renderer straight off
    /// disk, where a hand-edited or synced category file has not been through
    /// [`Entry::validate`].
    pub fn has_usable_name(&self) -> bool {
        is_usable_name(&self.name)
    }

    /// Why the command cannot be written into any generated shell file, or
    /// `None` when at least one can carry it.
    ///
    /// Only a function is linted. Its body is emitted verbatim between
    /// `function NAME {` and `}`, so a malformed one escapes the definition or
    /// swallows the rest of the file; an alias expansion is single-quoted on the
    /// way out and cannot break anything whatever it holds.
    pub fn command_problem(&self) -> Option<String> {
        self.command_problem_among(&Shell::all())
    }

    /// Why none of `shells` can carry the command, or `None` when one of them
    /// can. Which files are generated is a config choice, so an entry only zsh
    /// reads is defined nowhere once the zsh file is turned off.
    pub fn command_problem_among(&self, shells: &[Shell]) -> Option<String> {
        let mut problem = None;
        for &shell in shells {
            let found = self.command_problem_in(shell)?;
            problem.get_or_insert(found);
        }
        problem
    }

    /// Why `shell`'s generated file cannot carry the command, or `None` when it
    /// can. A body the other shell reads is still stored, so this is the check
    /// the renderer makes rather than [`Entry::command_problem`].
    pub fn command_problem_in(&self, shell: Shell) -> Option<String> {
        match self.kind {
            Kind::Function => lint::body_problem_in(&self.command, shell),
            Kind::Alias => None,
        }
    }

    /// Validate required fields, the shape of the trigger alias, and the shape
    /// of a function body.
    ///
    /// Both must be usable on exactly the terms the renderer enforces, so
    /// nothing `validate` accepts is later dropped.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(problem) = name_problem(&self.name) {
            return Err(format!("alias '{}': {problem}", self.name));
        }
        if self.command.trim().is_empty() {
            return Err(format!("alias '{}' must have a command", self.name));
        }
        if self.description.trim().is_empty() {
            return Err(format!("alias '{}' must have a description", self.name));
        }
        if let Some(problem) = self.command_problem() {
            return Err(format!("alias '{}': {problem}", self.name));
        }
        Ok(())
    }

    /// Every searchable field joined into one string; `usage`, `options`,
    /// `examples` and `example_output` are deliberately left out. A
    /// `Kind::Function` command is embedded verbatim, so this is not one line.
    ///
    /// `search` matches this literally rather than as a subsequence, precisely
    /// because it carries that body: a subsequence over it is satisfied by
    /// letters drawn from unrelated words.
    pub fn search_haystack(&self) -> String {
        format!(
            "{} {} {} {} {} {}",
            self.name,
            self.command,
            self.description,
            self.category,
            self.tags.join(" "),
            self.scope.as_deref().unwrap_or_default()
        )
    }
}

/// The prefix the generated shell files reserve for their own helpers, so an
/// entry can never redefine the dispatcher or the clearing function and have
/// its command run on every `cd` or every re-source.
pub const RESERVED_NAME_PREFIX: &str = "_cmdman_";

/// The words the generated shell files use as syntax or invoke as commands.
///
/// An alias is expanded where the shell parses a command word, so an entry
/// taking one of these names rewrites the generated file's own code the moment
/// that file is sourced. `local` is the worst case: it opens `_cmdman_apply`,
/// which runs on every `cd`. Keep this in step with what `shell::render` emits.
const RESERVED_NAMES: &[&str] = &[
    "add-zsh-hook",
    "alias",
    "autoload",
    "case",
    "esac",
    "fi",
    "function",
    "if",
    "in",
    "local",
    "return",
    "then",
    "typeset",
    "unalias",
    "unset",
];

/// Why `name` cannot be written into a generated shell file, or `None` when it
/// can. This is the gate the renderer relies on, since a name reaching it off
/// disk never went through [`Entry::validate`].
pub fn name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("an alias cannot be empty".into());
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
    {
        // Debug-format it: the character may itself be a newline, and the
        // message travels to a terminal one line at a time.
        return Some(format!(
            "{bad:?} is not allowed: use letters, digits, '_', '-' and '.'"
        ));
    }
    if name.starts_with('-') {
        return Some("an alias cannot start with '-'".into());
    }
    if name.starts_with(RESERVED_NAME_PREFIX) {
        return Some(format!(
            "'{RESERVED_NAME_PREFIX}' is reserved for cmd-man's own shell helpers"
        ));
    }
    if RESERVED_NAMES.contains(&name) {
        return Some(format!(
            "'{name}' is shell syntax the generated file itself uses"
        ));
    }
    None
}

/// Whether `name` may be written into a generated shell file. Everything
/// [`Entry::validate`] accepts satisfies this.
pub fn is_usable_name(name: &str) -> bool {
    name_problem(name).is_none()
}

/// Identifies an entry: a name plus the scope it lives in.
///
/// The derived ordering puts `None` (global) before any scoped entry.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryKey {
    pub scope: Option<String>,
    pub name: String,
}

impl EntryKey {
    /// The key of an unscoped entry.
    pub fn global(name: impl Into<String>) -> Self {
        EntryKey {
            scope: None,
            name: name.into(),
        }
    }

    /// The key of an existing entry.
    pub fn of(entry: &Entry) -> Self {
        EntryKey {
            scope: entry.scope.clone(),
            name: entry.name.clone(),
        }
    }

    /// A user-facing label: `ll` when global, `ll (api)` when scoped.
    pub fn label(&self) -> String {
        match &self.scope {
            Some(scope) => format!("{} ({scope})", self.name),
            None => self.name.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips_through_string() {
        assert_eq!("alias".parse::<Kind>().unwrap(), Kind::Alias);
        assert_eq!("Function".parse::<Kind>().unwrap(), Kind::Function);
        assert_eq!("fn".parse::<Kind>().unwrap(), Kind::Function);
        assert!("banana".parse::<Kind>().is_err());
        assert_eq!(Kind::Alias.to_string(), "alias");
    }

    #[test]
    fn capture_policy_defaults_to_confirm() {
        assert_eq!(CapturePolicy::default(), CapturePolicy::Confirm);
    }

    #[test]
    fn validate_requires_command_and_description() {
        let mut e = Entry::new("gitsw", Kind::Alias, "git switch", "switch branches");
        assert!(e.validate().is_ok());

        e.description = "  ".into();
        assert!(e.validate().is_err());

        let e = Entry::new("gitsw", Kind::Alias, "", "desc");
        assert!(e.validate().is_err());
    }

    #[test]
    fn validate_rejects_bad_names() {
        for bad in ["", "git sw", "a=b", "wild*card", "semi;colon", "pipe|it"] {
            let e = Entry::new(bad, Kind::Alias, "echo hi", "desc");
            assert!(e.validate().is_err(), "expected '{bad}' to be rejected");
        }
    }

    #[test]
    fn a_name_validate_accepts_is_always_usable() {
        for good in ["ll", "git-sw", "g_2", "a.b", "x9"] {
            let e = Entry::new(good, Kind::Alias, "echo hi", "desc");
            assert!(e.validate().is_ok(), "expected '{good}' to validate");
            assert!(e.has_usable_name(), "validate accepted unusable '{good}'");
        }
    }

    #[test]
    fn a_name_carrying_shell_syntax_is_not_usable() {
        for bad in [
            "",
            "-a",
            "x\necho pwned; #",
            "a;b",
            "a b",
            "a'b",
            "$(id)",
            "a`id`",
        ] {
            assert!(!is_usable_name(bad), "expected '{bad}' to be unusable");
        }
    }

    #[test]
    fn a_name_claiming_a_generated_helper_is_refused_and_unusable() {
        // Otherwise the generated file redefines its own dispatcher or clearing
        // function with the user's command, which then runs on every cd.
        for bad in ["_cmdman_apply", "_cmdman_forget", "_cmdman_on_1"] {
            let e = Entry::new(bad, Kind::Function, "echo hi", "desc");
            assert!(e.validate().is_err(), "expected '{bad}' to be rejected");
            assert!(!is_usable_name(bad), "expected '{bad}' to be unusable");
        }
        assert!(is_usable_name("_cmdman"));
        assert!(is_usable_name("_myhelper"));
    }

    #[test]
    fn a_name_the_generated_file_uses_as_syntax_is_refused_and_unusable() {
        // An alias is expanded wherever the shell parses a command word, so
        // such a name would rewrite the generated file's own code. `local`
        // opens the dispatcher, which runs on every cd.
        for bad in RESERVED_NAMES {
            let e = Entry::new(*bad, Kind::Alias, "echo pwned", "desc");
            assert!(e.validate().is_err(), "expected '{bad}' to be rejected");
            assert!(!is_usable_name(bad), "expected '{bad}' to be unusable");
        }
        // Only exact matches: these merely contain or extend a reserved word.
        for good in ["locale", "aliases", "my-alias", "Local", "returns"] {
            assert!(is_usable_name(good), "expected '{good}' to stay usable");
        }
    }

    #[test]
    fn validate_rejects_a_leading_hyphen() {
        for bad in ["-a", "-f", "--"] {
            let e = Entry::new(bad, Kind::Alias, "echo hi", "desc");
            assert!(e.validate().is_err(), "expected '{bad}' to be rejected");
        }
        assert!(
            Entry::new("a-b", Kind::Alias, "echo hi", "desc")
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn a_function_body_that_escapes_its_definition_is_refused_and_unusable() {
        // The body is written verbatim between the braces of the definition, so
        // a standalone '}' would end the function and leave the rest as
        // top-level code, run on source and on every cd.
        for bad in ["}\necho pwned", "echo hi\n}\nid", "{ echo hi", "echo 'oops"] {
            let e = Entry::new("kruby", Kind::Function, bad, "desc");
            assert!(e.validate().is_err(), "expected {bad:?} to be rejected");
            assert!(
                e.command_problem().is_some(),
                "expected {bad:?} to be unusable"
            );
        }
    }

    #[test]
    fn a_command_only_one_shell_reads_is_a_problem_for_that_shell_alone() {
        // The gate is "no generated file can carry it", so a body one shell
        // reads is stored; turning that shell's file off leaves it carried
        // nowhere, which is what `command_problem_among` is asked.
        let e = Entry::new("ft", Kind::Function, "echo a |& cat", "desc");
        assert!(e.validate().is_ok());
        assert_eq!(e.command_problem(), None);
        assert_eq!(e.command_problem_in(Shell::Zsh), None);
        assert!(e.command_problem_in(Shell::Bash).is_some());
        assert!(e.command_problem_among(&[Shell::Bash]).is_some());
        assert_eq!(e.command_problem_among(&[Shell::Zsh]), None);
        // With no file generated at all there is nothing to be left out of.
        assert_eq!(e.command_problem_among(&[]), None);
    }

    #[test]
    fn an_ordinary_function_body_still_validates() {
        // The lint sits in front of every save, so a body a user would plausibly
        // type has to survive it.
        for good in [
            "git status",
            "[ -d .git ] && { git log -1; }",
            "case $1 in a) echo a ;; esac",
            "cat <<EOF\n}\nEOF",
            "grep -q x <<< \"$1\"",
            "echo {",
        ] {
            let e = Entry::new("f", Kind::Function, good, "desc");
            assert!(e.validate().is_ok(), "expected {good:?} to validate");
        }
    }

    #[test]
    fn only_a_function_body_is_linted() {
        // An alias expansion is single-quoted on the way into the generated
        // file, so it cannot break out however it is shaped.
        let e = Entry::new("ll", Kind::Alias, "}\necho hi", "desc");
        assert!(e.validate().is_ok());
        assert_eq!(e.command_problem(), None);
        for shell in Shell::all() {
            assert_eq!(e.command_problem_in(shell), None);
        }
    }

    #[test]
    fn scope_is_optional_and_omitted_when_global() {
        let entry = Entry::new("ll", Kind::Alias, "ls -la", "d");
        assert_eq!(entry.scope, None);
        let text = toml::to_string(&entry).unwrap();
        assert!(
            !text.contains("scope"),
            "global entries should not serialize a scope: {text}"
        );
    }

    #[test]
    fn scope_id_is_searchable() {
        let mut entry = Entry::new("ll", Kind::Alias, "ls -la", "d");
        entry.scope = Some("api".into());
        assert!(entry.search_haystack().contains("api"));
    }

    #[test]
    fn entry_key_labels_disambiguate() {
        assert_eq!(EntryKey::global("ll").label(), "ll");
        assert_eq!(
            EntryKey {
                scope: Some("api".into()),
                name: "ll".into()
            }
            .label(),
            "ll (api)"
        );
    }
}
