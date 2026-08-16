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

    /// Pull the latest from `remote`/`branch`. Always non-interactive: a merge
    /// strategy is forced so a background process never stops for input, and
    /// unrelated histories are allowed so a first multi-machine sync reconciles.
    pub fn pull(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&[
            "pull",
            "--no-rebase",
            "--no-edit",
            "--allow-unrelated-histories",
            remote,
            branch,
        ])?;
        Ok(())
    }

    /// Pull the latest from an explicit URL (used when credentials must be
    /// embedded for a one-shot authenticated fetch). Non-interactive like [`pull`].
    pub fn pull_url(&self, url: &str, branch: &str) -> Result<()> {
        self.run(&[
            "pull",
            "--no-rebase",
            "--no-edit",
            "--allow-unrelated-histories",
            url,
            branch,
        ])?;
        Ok(())
    }

    /// The current `HEAD` commit hash, if the repo has any commits.
    pub fn head_commit(&self) -> Option<String> {
        self.run(&["rev-parse", "HEAD"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Abort an in-progress merge, leaving the working tree as it was.
    pub fn abort_merge(&self) -> Result<()> {
        self.run(&["merge", "--abort"])?;
        Ok(())
    }

    /// Reset the working tree to `HEAD`, restoring tracked files.
    pub fn reset_hard(&self) -> Result<()> {
        self.run(&["reset", "--hard", "HEAD"])?;
        Ok(())
    }
}

/// Whether a remote URL already has any refs (i.e. contains commits). A command
/// failure (missing repo, offline) is reported as "no content" so callers fall
/// back to initializing and pushing rather than treating it as fatal.
pub fn remote_has_content(url: &str) -> bool {
    Command::new("git")
        .args(["ls-remote", url])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
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
    fn remote_has_content_reflects_refs() {
        let bare = tempfile::tempdir().unwrap();
        Command::new("git")
            .args(["init", "--bare", "-b", "main"])
            .arg(bare.path())
            .output()
            .unwrap();
        let bare_url = bare.path().to_str().unwrap();
        // An empty bare repo has no refs.
        assert!(!remote_has_content(bare_url));

        // A missing path is treated as no content, not an error.
        assert!(!remote_has_content("/no/such/repo"));

        // After a push it reports content.
        let work = tempfile::tempdir().unwrap();
        let repo = configured_repo(work.path());
        std::fs::write(work.path().join("a.toml"), "x = 1\n").unwrap();
        repo.commit_all("first").unwrap();
        repo.set_remote("origin", bare_url).unwrap();
        repo.push("origin", "main").unwrap();
        assert!(remote_has_content(bare_url));
    }

    #[test]
    fn reset_hard_restores_tracked_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = configured_repo(dir.path());
        let file = dir.path().join("a.toml");
        std::fs::write(&file, "x = 1\n").unwrap();
        repo.commit_all("first").unwrap();
        std::fs::write(&file, "x = 999\n").unwrap();
        repo.reset_hard().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "x = 1\n");
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
