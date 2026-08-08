//! Thin wrapper around the `git` CLI scoped to a working directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

/// A git working directory.
pub struct GitRepo {
    workdir: PathBuf,
}

impl GitRepo {
    pub fn new(workdir: impl Into<PathBuf>) -> Self {
        GitRepo {
            workdir: workdir.into(),
        }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Whether the working directory is already a git repository.
    pub fn is_initialized(&self) -> bool {
        self.workdir.join(".git").exists()
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.workdir)
            .output()
            .with_context(|| format!("running git {}", args.join(" ")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("git {} failed: {}", args.join(" "), stderr.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Initialize the repo on branch `main` if not already initialized.
    pub fn init(&self) -> Result<()> {
        if self.is_initialized() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.workdir)?;
        self.run(&["init", "-b", "main"])?;
        Ok(())
    }

    /// Ensure a `.gitignore` exists containing the given entries.
    pub fn ensure_gitignore(&self, entries: &[&str]) -> Result<()> {
        let path = self.workdir.join(".gitignore");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let mut lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
        let mut changed = false;
        for entry in entries {
            if !lines.iter().any(|l| l.trim() == *entry) {
                lines.push((*entry).to_string());
                changed = true;
            }
        }
        if changed {
            let mut text = lines.join("\n");
            text.push('\n');
            std::fs::write(&path, text)?;
        }
        Ok(())
    }

    /// Stage everything and commit. Returns false when there was nothing to do.
    pub fn commit_all(&self, message: &str) -> Result<bool> {
        self.run(&["add", "-A"])?;
        if !self.has_changes_staged()? {
            return Ok(false);
        }
        self.run(&["commit", "-m", message])?;
        Ok(true)
    }

    fn has_changes_staged(&self) -> Result<bool> {
        // Exit code 1 from `diff --cached --quiet` means there are staged changes.
        let status = Command::new("git")
            .args(["diff", "--cached", "--quiet"])
            .current_dir(&self.workdir)
            .status()
            .context("running git diff --cached")?;
        Ok(!status.success())
    }

    /// The URL of a remote, if configured.
    pub fn remote_url(&self, name: &str) -> Option<String> {
        self.run(&["remote", "get-url", name])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Add or update a remote to point at `url`.
    pub fn set_remote(&self, name: &str, url: &str) -> Result<()> {
        if self.remote_url(name).is_some() {
            self.run(&["remote", "set-url", name, url])?;
        } else {
            self.run(&["remote", "add", name, url])?;
        }
        Ok(())
    }

    /// Push `branch` to `remote`, setting upstream.
    pub fn push(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&["push", "-u", remote, branch])?;
        Ok(())
    }

    /// Pull the latest from `remote`/`branch` (fast-forward only is not enforced).
    pub fn pull(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&["pull", remote, branch])?;
        Ok(())
    }
}

/// Clone `url` into `dest`.
pub fn clone(url: &str, dest: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("clone")
        .arg(url)
        .arg(dest)
        .output()
        .context("running git clone")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git clone failed: {}", stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured_repo(dir: &Path) -> GitRepo {
        let repo = GitRepo::new(dir.to_path_buf());
        repo.init().unwrap();
        // Ensure commits work without relying on ambient git identity.
        repo.run(&["config", "user.email", "test@example.com"])
            .unwrap();
        repo.run(&["config", "user.name", "Test"]).unwrap();
        repo
    }

    #[test]
    fn init_commit_and_remote() {
        let dir = tempfile::tempdir().unwrap();
        let repo = configured_repo(dir.path());
        assert!(repo.is_initialized());

        std::fs::write(dir.path().join("a.toml"), "x = 1\n").unwrap();
        assert!(repo.commit_all("first").unwrap());
        // Nothing to commit the second time.
        assert!(!repo.commit_all("noop").unwrap());

        repo.set_remote("origin", "https://example.com/x.git")
            .unwrap();
        assert_eq!(
            repo.remote_url("origin").as_deref(),
            Some("https://example.com/x.git")
        );
        repo.set_remote("origin", "https://example.com/y.git")
            .unwrap();
        assert_eq!(
            repo.remote_url("origin").as_deref(),
            Some("https://example.com/y.git")
        );
    }

    #[test]
    fn gitignore_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let repo = configured_repo(dir.path());
        repo.ensure_gitignore(&["shell/"]).unwrap();
        repo.ensure_gitignore(&["shell/"]).unwrap();
        let text = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert_eq!(text.matches("shell/").count(), 1);
    }
}
