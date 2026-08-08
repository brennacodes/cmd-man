//! Redaction of sensitive data from captured command output.

use std::sync::OnceLock;

use regex::Regex;

/// Sanitizer configured with host-specific identifiers to redact.
pub struct Sanitizer {
    home: Option<String>,
    user: Option<String>,
}

impl Sanitizer {
    pub fn new(home: Option<String>, user: Option<String>) -> Self {
        // Avoid redacting very short usernames that collide with common words.
        let user = user.filter(|u| u.len() >= 3);
        Sanitizer { home, user }
    }

    /// Build a sanitizer from the current environment.
    pub fn from_env() -> Self {
        let home = crate::paths::home_dir().map(|p| p.display().to_string());
        let user = std::env::var("USER")
            .ok()
            .or_else(|| std::env::var("LOGNAME").ok());
        Sanitizer::new(home, user)
    }

    pub fn sanitize(&self, text: &str) -> String {
        let mut out = text.to_string();

        if let Some(home) = &self.home
            && !home.is_empty()
        {
            out = out.replace(home, "~");
        }

        // Secret-shaped strings first, before generic hex/token rules.
        out = github_token().replace_all(&out, "<token>").into_owned();
        out = openai_token().replace_all(&out, "<token>").into_owned();
        out = aws_key().replace_all(&out, "<token>").into_owned();
        out = slack_token().replace_all(&out, "<token>").into_owned();
        out = bearer().replace_all(&out, "Bearer <token>").into_owned();
        out = key_value_secret()
            .replace_all(&out, "${key}${sep}<redacted>")
            .into_owned();

        out = email().replace_all(&out, "<email>").into_owned();
        out = ipv4().replace_all(&out, "<ip>").into_owned();
        out = ipv6().replace_all(&out, "<ip>").into_owned();
        out = long_hex().replace_all(&out, "<hash>").into_owned();

        if let Some(user) = &self.user {
            out = word_boundary_replace(&out, user, "<user>");
        }

        out
    }
}

/// Convenience wrapper using the current environment.
pub fn sanitize(text: &str) -> String {
    Sanitizer::from_env().sanitize(text)
}

fn word_boundary_replace(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let pattern = format!(r"\b{}\b", regex::escape(needle));
    match Regex::new(&pattern) {
        Ok(re) => re.replace_all(haystack, replacement).into_owned(),
        Err(_) => haystack.to_string(),
    }
}

macro_rules! cached_regex {
    ($name:ident, $pattern:expr) => {
        fn $name() -> &'static Regex {
            static RE: OnceLock<Regex> = OnceLock::new();
            RE.get_or_init(|| Regex::new($pattern).expect("valid regex"))
        }
    };
}

cached_regex!(github_token, r"gh[pousr]_[A-Za-z0-9]{20,}");
cached_regex!(openai_token, r"sk-[A-Za-z0-9_-]{16,}");
cached_regex!(aws_key, r"AKIA[0-9A-Z]{16}");
cached_regex!(slack_token, r"xox[baprs]-[A-Za-z0-9-]{10,}");
cached_regex!(bearer, r"(?i)bearer\s+[A-Za-z0-9._~+/-]+=*");
cached_regex!(
    key_value_secret,
    r"(?i)(?P<key>token|secret|password|passwd|api[_-]?key|access[_-]?key|auth[_-]?token)(?P<sep>\s*[=:]\s*)\S+"
);
cached_regex!(email, r"[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}");
cached_regex!(ipv4, r"\b(?:\d{1,3}\.){3}\d{1,3}\b");
cached_regex!(ipv6, r"\b(?:[A-Fa-f0-9]{1,4}:){2,7}[A-Fa-f0-9]{1,4}\b");
cached_regex!(long_hex, r"\b[0-9a-fA-F]{32,}\b");

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> Sanitizer {
        Sanitizer::new(
            Some("/Users/brenna".to_string()),
            Some("brenna".to_string()),
        )
    }

    #[test]
    fn redacts_home_and_user() {
        let out = s().sanitize("path /Users/brenna/code owned by brenna");
        assert!(out.contains("~/code"));
        assert!(out.contains("<user>"));
        assert!(!out.contains("brenna"));
    }

    #[test]
    fn redacts_ip_and_email() {
        let out = s().sanitize("host 192.168.1.10 mail a.b@example.com");
        assert!(out.contains("<ip>"));
        assert!(out.contains("<email>"));
        assert!(!out.contains("192.168.1.10"));
    }

    #[test]
    fn redacts_tokens() {
        let gh = s().sanitize("token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");
        assert!(gh.contains("<token>"));
        let bearer = s().sanitize("Authorization: Bearer abc.def.ghi");
        assert!(bearer.contains("Bearer <token>"));
        let kv = s().sanitize("API_KEY=supersecretvalue123");
        assert!(kv.contains("<redacted>"));
        assert!(!kv.contains("supersecretvalue123"));
    }

    #[test]
    fn short_username_is_not_redacted() {
        let san = Sanitizer::new(None, Some("ab".to_string()));
        assert_eq!(san.sanitize("ab cd ab"), "ab cd ab");
    }

    #[test]
    fn leaves_ordinary_output_untouched() {
        let out = s().sanitize("Switched to branch 'main'");
        assert_eq!(out, "Switched to branch 'main'");
    }
}
