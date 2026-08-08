//! Parse `--help` / `man` output into structured sections, and fetch it safely.

use crate::config::CaptureConfig;

use super::runner::run_capture;

/// Sections extracted from a command's help text.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HelpSections {
    pub usage: String,
    pub description: String,
    pub options: String,
    pub examples: String,
}

impl HelpSections {
    pub fn is_empty(&self) -> bool {
        self.usage.is_empty()
            && self.description.is_empty()
            && self.options.is_empty()
            && self.examples.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Usage,
    Description,
    Options,
    Examples,
}

/// Parse help text into sections by recognizing common headers. Only content
/// under a recognized header is captured, so unrelated output yields nothing.
pub fn parse_help(text: &str) -> HelpSections {
    let mut usage: Vec<String> = Vec::new();
    let mut description: Vec<String> = Vec::new();
    let mut options: Vec<String> = Vec::new();
    let mut examples: Vec<String> = Vec::new();
    let mut current: Option<Section> = None;

    for line in text.lines() {
        if let Some((section, inline, header)) = header_of(line) {
            current = Some(section);
            let bucket = match section {
                Section::Usage => &mut usage,
                Section::Description => &mut description,
                Section::Options => &mut options,
                Section::Examples => &mut examples,
            };
            // Preserve non-primary option sub-headers (e.g. "Runtime options:").
            if section == Section::Options && header != "options" {
                bucket.push(line.trim_end().to_string());
            }
            if !inline.is_empty() {
                bucket.push(inline.to_string());
            }
            continue;
        }
        if let Some(section) = current {
            let bucket = match section {
                Section::Usage => &mut usage,
                Section::Description => &mut description,
                Section::Options => &mut options,
                Section::Examples => &mut examples,
            };
            bucket.push(line.trim_end().to_string());
        }
    }

    // Git-style help lists options as indented flag lines directly under the
    // usage line, with no "Options:" header. Split those into the options
    // field when a dedicated options section was not found.
    if options.is_empty()
        && let Some(pos) = usage.iter().position(|l| l.trim_start().starts_with('-'))
    {
        options = usage.split_off(pos);
    }

    HelpSections {
        usage: joined(usage),
        description: joined(description),
        options: joined(options),
        examples: joined(examples),
    }
}

/// If `line` starts a recognized section, return its section, any inline
/// content after the colon, and the normalized header word.
fn header_of(line: &str) -> Option<(Section, &str, String)> {
    let trimmed = line.trim();
    let colon = trimmed.find(':')?;
    // A header is a short label ending in a colon, not a body line that merely
    // contains a colon (those are usually indented and longer).
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let label = trimmed[..colon].trim().to_ascii_lowercase();
    let inline = trimmed[colon + 1..].trim();
    let section = if label == "usage" {
        Section::Usage
    } else if label == "description" {
        Section::Description
    } else if label == "examples" || label == "example" {
        Section::Examples
    } else if label == "options" || label.ends_with(" options") || label == "arguments" {
        Section::Options
    } else {
        return None;
    };
    Some((section, inline, label))
}

fn joined(lines: Vec<String>) -> String {
    // Drop leading/trailing blank lines while preserving internal structure.
    let start = lines.iter().position(|l| !l.trim().is_empty());
    let end = lines.iter().rposition(|l| !l.trim().is_empty());
    match (start, end) {
        (Some(s), Some(e)) => lines[s..=e].join("\n"),
        _ => String::new(),
    }
}

/// Fetch and parse help for a command, trying a few safe invocations.
///
/// Each invocation runs through the sandboxed, timeboxed capture pipeline with
/// only the command/subcommand and a help flag (no user arguments), so nothing
/// destructive runs. Sections are merged, preferring earlier invocations.
pub fn fetch_help(cfg: &CaptureConfig, command: &str) -> HelpSections {
    // Bound each help call tightly so a slow command cannot stall the caller.
    let help_cfg = CaptureConfig {
        timeout_secs: cfg.timeout_secs.clamp(1, 5),
        sandbox: cfg.sandbox,
    };
    let mut merged = HelpSections::default();
    for invocation in help_invocations(command).into_iter().take(3) {
        if let Ok(result) = run_capture(&help_cfg, &invocation) {
            let parsed = parse_help(&result.output);
            merge_empty(&mut merged, parsed);
            if !merged.usage.is_empty()
                && !merged.description.is_empty()
                && !merged.options.is_empty()
                && !merged.examples.is_empty()
            {
                break;
            }
        }
    }
    merged
}

/// Candidate help invocations for a command, most specific first.
pub fn help_invocations(command: &str) -> Vec<String> {
    let tokens = shlex::split(command).unwrap_or_default();
    let mut out = Vec::new();
    if let Some(first) = tokens.first() {
        // command + subcommand (a leading non-flag second token)
        let subject = match tokens.get(1) {
            Some(second) if !second.starts_with('-') => format!("{first} {second}"),
            _ => first.clone(),
        };
        for flag in ["-h", "--help"] {
            out.push(format!("{subject} {flag}"));
        }
        if subject != *first {
            out.push(format!("{first} --help"));
        }
    }
    out.dedup();
    out
}

fn merge_empty(into: &mut HelpSections, from: HelpSections) {
    if into.usage.is_empty() {
        into.usage = from.usage;
    }
    if into.description.is_empty() {
        into.description = from.description;
    }
    if into.options.is_empty() {
        into.options = from.options;
    }
    if into.examples.is_empty() {
        into.examples = from.examples;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rails_style_sections() {
        let text = "Usage:\n  rails new APP_PATH [options]\n\nOptions:\n  -r, [--ruby=PATH]  # Ruby path\n\nRuntime options:\n  -f, [--force]  # overwrite\n\nDescription:\n    Creates a new Rails app.\n\nExamples:\n    rails new ~/foo\n";
        let h = parse_help(text);
        assert!(h.usage.contains("rails new APP_PATH"));
        assert!(h.options.contains("--ruby=PATH"));
        assert!(h.options.contains("Runtime options:"));
        assert!(h.options.contains("--force"));
        assert!(h.description.contains("Creates a new Rails app."));
        assert!(h.examples.contains("rails new ~/foo"));
    }

    #[test]
    fn parses_git_style_inline_usage() {
        let text = "usage: git switch [<options>] [<branch>]\n\n    -c, --create <branch>   create and switch\n    -q, --quiet             suppress output\n";
        let h = parse_help(text);
        assert!(h.usage.contains("git switch [<options>]"));
        // Indented flag lines are split into options, not left in usage or
        // misfiled as description.
        assert!(h.options.contains("--create"));
        assert!(!h.usage.contains("--create"));
        assert!(h.description.is_empty());
    }

    #[test]
    fn unrelated_output_yields_nothing() {
        let h = parse_help("total 8\ndrwxr-xr-x 2 user staff 64 file\n");
        assert!(h.is_empty());
    }

    #[test]
    fn help_invocations_prefer_subcommand() {
        let inv = help_invocations("git switch");
        assert_eq!(inv[0], "git switch -h");
        assert!(inv.contains(&"git switch --help".to_string()));
        assert!(inv.contains(&"git --help".to_string()));

        let inv = help_invocations("ls -la");
        assert_eq!(inv[0], "ls -h");
        assert!(inv.contains(&"ls --help".to_string()));
    }
}
