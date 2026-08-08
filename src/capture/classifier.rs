//! Static safety classification for entries before running them.

use crate::model::{CapturePolicy, Entry, Kind};

/// Commands that destroy or irreversibly change state.
const DESTRUCTIVE_BINARIES: &[&str] = &[
    "rm", "rmdir", "mv", "dd", "mkfs", "kill", "killall", "pkill", "shutdown", "reboot", "halt",
    "poweroff", "sudo", "doas", "chmod", "chown", "chgrp", "truncate", "shred", "srm", "trash",
    "unlink", "fdisk", "parted", "mkswap", "wipefs", "format", "diskutil", "chflags",
];

/// Verbs the user called out as destructive, plus close synonyms. Matched as
/// whole tokens (a command name or a subcommand).
const DESTRUCTIVE_VERBS: &[&str] = &[
    "delete", "remove", "rename", "move", "cut", "destroy", "drop", "prune", "purge", "erase",
    "reset", "clean", "clobber", "rmi", "kill",
];

/// Interpreters that run arbitrary code passed inline, hiding their behavior.
const INTERPRETERS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "fish", "python", "python2", "python3", "perl", "ruby",
    "node", "deno", "php",
];

/// Verbs that are not destructive but have outward side effects, so capturing
/// them automatically is undesirable.
const SIDE_EFFECT_VERBS: &[&str] = &[
    "push",
    "publish",
    "deploy",
    "upload",
    "send",
    "commit",
    "install",
    "uninstall",
    "release",
    "sync",
    "stop",
    "disable",
    "start",
    "restart",
];

/// Flags that indicate a forceful or irreversible operation.
const DANGEROUS_FLAGS: &[&str] = &[
    "-f",
    "--force",
    "--hard",
    "-rf",
    "-fr",
    "-Rf",
    "-fR",
    "-D",
    "-delete",
    "--delete",
    "--prune",
    "-exec",
    "-execdir",
    "--no-preserve-root",
];

/// The result of classifying an entry for capture safety.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assessment {
    pub policy: CapturePolicy,
    pub destructive: bool,
    pub reasons: Vec<String>,
}

impl Assessment {
    pub fn needs_confirmation(&self) -> bool {
        matches!(self.policy, CapturePolicy::Confirm)
    }

    pub fn is_blocked(&self) -> bool {
        matches!(self.policy, CapturePolicy::Never)
    }
}

/// Classify an entry, honoring an explicit `Never` override the user has set.
pub fn classify(entry: &Entry) -> Assessment {
    if entry.capture_policy == CapturePolicy::Never {
        return Assessment {
            policy: CapturePolicy::Never,
            destructive: entry.destructive,
            reasons: vec!["capture disabled for this entry".into()],
        };
    }

    let scan = scan_text(&entry.command);
    let mut reasons = scan.reasons;
    let mut destructive = scan.destructive;

    // Functions carry more hidden behavior; an unparseable body is treated as
    // needing confirmation rather than assumed safe.
    if entry.kind == Kind::Function && scan.unparseable {
        reasons.push("function body could not be fully parsed".into());
    }

    if entry.destructive {
        destructive = true;
        if !reasons.iter().any(|r| r.contains("flagged")) {
            reasons.push("previously flagged destructive".into());
        }
    }

    let policy = if destructive || scan.side_effecting || scan.unparseable {
        CapturePolicy::Confirm
    } else {
        CapturePolicy::Auto
    };

    Assessment {
        policy,
        destructive,
        reasons,
    }
}

struct Scan {
    destructive: bool,
    side_effecting: bool,
    unparseable: bool,
    reasons: Vec<String>,
}

fn scan_text(text: &str) -> Scan {
    let mut destructive = false;
    let mut side_effecting = false;
    let mut unparseable = false;
    let mut reasons = Vec::new();

    if text.trim().is_empty() {
        return Scan {
            destructive: false,
            side_effecting: true,
            unparseable: true,
            reasons: vec!["empty command".into()],
        };
    }

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // Constructs that hide code from token inspection: run substitution,
        // process substitution, and `eval`. These cannot be reasoned about
        // statically, so they always require confirmation.
        if line.contains("$(") || line.contains('`') || line.contains("<(") || line.contains(">(") {
            unparseable = true;
            reasons.push("uses command substitution".into());
        }

        let Some(tokens) = shlex::split(line) else {
            unparseable = true;
            reasons.push(format!("could not parse: {line}"));
            continue;
        };

        if runs_inline_code(&tokens) {
            unparseable = true;
            reasons.push("runs inline interpreter code".into());
        }

        for token in &tokens {
            let base = basename(token);
            let lower = base.to_ascii_lowercase();

            if DESTRUCTIVE_BINARIES.contains(&lower.as_str()) {
                destructive = true;
                reasons.push(format!("uses destructive command '{lower}'"));
            }
            if DESTRUCTIVE_VERBS.contains(&lower.as_str()) {
                destructive = true;
                reasons.push(format!("contains destructive verb '{lower}'"));
            }
            if SIDE_EFFECT_VERBS.contains(&lower.as_str()) {
                side_effecting = true;
                reasons.push(format!("has outward side effect '{lower}'"));
            }
            if DANGEROUS_FLAGS.contains(&token.as_str()) {
                destructive = true;
                reasons.push(format!("uses forceful flag '{token}'"));
            }
            // Any output redirect overwrites or appends to a file, including the
            // no-space form `hi>file`. Matching anywhere in the token also
            // catches `2>` and `>>`; erring toward confirmation is safe.
            if token.contains('>') {
                destructive = true;
                reasons.push("redirects/overwrites output".into());
            }
        }
    }

    reasons.sort();
    reasons.dedup();

    Scan {
        destructive,
        side_effecting,
        unparseable,
        reasons,
    }
}

/// The first token of the first meaningful line, used for `--help` intel.
pub fn primary_binary(text: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(tokens) = shlex::split(line)
            && let Some(first) = tokens.first()
        {
            return Some(basename(first).to_string());
        }
    }
    None
}

fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// True when the tokens invoke `eval` or run an interpreter with inline code
/// (for example `sh -c '...'` or `python -c '...'`).
fn runs_inline_code(tokens: &[String]) -> bool {
    if tokens.iter().any(|t| t == "eval") {
        return true;
    }
    let Some(first) = tokens.first() else {
        return false;
    };
    if INTERPRETERS.contains(&basename(first).to_ascii_lowercase().as_str()) {
        return tokens.iter().any(|t| t == "-c" || t == "-e");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alias(cmd: &str) -> Entry {
        Entry::new("x", Kind::Alias, cmd, "desc")
    }

    #[test]
    fn safe_commands_are_auto() {
        for cmd in ["git status", "ls -la", "echo hi", "pwd", "git switch main"] {
            let a = classify(&alias(cmd));
            assert_eq!(a.policy, CapturePolicy::Auto, "expected auto for '{cmd}'");
            assert!(!a.destructive);
        }
    }

    #[test]
    fn destructive_commands_require_confirmation() {
        for cmd in [
            "rm -rf build",
            "git reset --hard",
            "kill -9 123",
            "sudo reboot",
            "mv a b",
            "chmod +x file",
            "git clean -fd",
            "echo hi > file",
        ] {
            let a = classify(&alias(cmd));
            assert_eq!(
                a.policy,
                CapturePolicy::Confirm,
                "expected confirm for '{cmd}'"
            );
            assert!(a.destructive, "expected destructive for '{cmd}'");
        }
    }

    #[test]
    fn hidden_code_requires_confirmation() {
        for cmd in [
            "echo $(rm ~/notes)",
            "echo `rm x`",
            "sh -c 'rm x'",
            "bash -c \"echo hi\"",
            "python -c 'import os'",
            "eval \"$something\"",
            "cat <(curl example.com)",
        ] {
            let a = classify(&alias(cmd));
            assert_ne!(
                a.policy,
                CapturePolicy::Auto,
                "expected non-auto for '{cmd}'"
            );
        }
    }

    #[test]
    fn no_space_redirect_is_destructive() {
        for cmd in ["echo hi>file", "echo hi>>log", "cat a >|b"] {
            let a = classify(&alias(cmd));
            assert!(a.destructive, "expected destructive for '{cmd}'");
            assert_eq!(a.policy, CapturePolicy::Confirm);
        }
    }

    #[test]
    fn extra_destructive_tools_and_flags() {
        for cmd in [
            "find . -delete",
            "find . -exec rm {} +",
            "git branch -D feature",
            "trash old.txt",
            "docker rmi image",
        ] {
            let a = classify(&alias(cmd));
            assert!(a.destructive, "expected destructive for '{cmd}'");
        }
    }

    #[test]
    fn common_safe_flag_c_is_not_flagged() {
        // `-c` on a non-interpreter (grep count) must stay auto.
        let a = classify(&alias("grep -c foo file"));
        assert_eq!(a.policy, CapturePolicy::Auto);
    }

    #[test]
    fn side_effecting_commands_require_confirmation_but_not_destructive() {
        let a = classify(&alias("git push origin main"));
        assert_eq!(a.policy, CapturePolicy::Confirm);
        assert!(!a.destructive);
    }

    #[test]
    fn user_verbs_are_flagged() {
        for cmd in ["mytool delete thing", "app remove item", "x rename y"] {
            let a = classify(&alias(cmd));
            assert!(a.destructive, "expected '{cmd}' destructive");
        }
    }

    #[test]
    fn never_override_is_respected() {
        let mut e = alias("echo hi");
        e.capture_policy = CapturePolicy::Never;
        assert!(classify(&e).is_blocked());
    }

    #[test]
    fn function_with_destructive_body_confirms() {
        let e = Entry::new(
            "kruby",
            Kind::Function,
            "pid=$(lsof -i:3000)\nkill -9 $pid",
            "kill ruby",
        );
        let a = classify(&e);
        assert_eq!(a.policy, CapturePolicy::Confirm);
        assert!(a.destructive);
    }

    #[test]
    fn primary_binary_extraction() {
        assert_eq!(primary_binary("git switch main").as_deref(), Some("git"));
        assert_eq!(primary_binary("/usr/bin/ls -la").as_deref(), Some("ls"));
        assert_eq!(
            primary_binary("# comment\necho hi").as_deref(),
            Some("echo")
        );
    }
}
