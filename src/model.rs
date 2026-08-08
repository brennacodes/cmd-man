//! Core data types for stored aliases and functions.

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
    /// The shell trigger the user types (e.g. `gitsw`). Unique across the store.
    pub name: String,
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

    /// Validate required fields and the shape of the trigger name.
    ///
    /// The name must be a usable shell trigger: non-empty, no whitespace, and
    /// free of characters that would break an `alias`/function definition.
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("name cannot be empty".into());
        }
        if self.name.chars().any(|c| c.is_whitespace()) {
            return Err(format!("name '{}' cannot contain whitespace", self.name));
        }
        // A trigger name must be a plain shell word. Allow letters, digits, and
        // the small set of punctuation that is safe in an alias/function name.
        if let Some(bad) = self
            .name
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')))
        {
            return Err(format!("name '{}' cannot contain '{bad}'", self.name));
        }
        if self.command.trim().is_empty() {
            return Err(format!("entry '{}' must have a command", self.name));
        }
        if self.description.trim().is_empty() {
            return Err(format!("entry '{}' must have a description", self.name));
        }
        Ok(())
    }

    /// A single-line haystack used for fuzzy searching.
    pub fn search_haystack(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.name,
            self.command,
            self.description,
            self.category,
            self.tags.join(" ")
        )
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
}
