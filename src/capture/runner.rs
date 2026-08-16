//! Running commands to capture example output: a hidden re-exec helper applies
//! the sandbox in a dedicated process, and every run is hard-timeboxed.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use wait_timeout::ChildExt;

use crate::config::CaptureConfig;
use crate::proc::own_process_group;

use super::sanitize::sanitize;

/// Env var carrying the command line for the capture helper.
pub const CAPTURE_CMD_ENV: &str = "CMD_MAN_CAPTURE_CMD";
/// Env var toggling the sandbox in the capture helper ("1" enables).
pub const CAPTURE_SANDBOX_ENV: &str = "CMD_MAN_CAPTURE_SANDBOX";
/// Env var carrying the inner timeout in seconds.
pub const CAPTURE_TIMEOUT_ENV: &str = "CMD_MAN_CAPTURE_TIMEOUT";
/// Argv marker selecting the hidden capture-exec subcommand.
pub const CAPTURE_EXEC_ARG: &str = "__capture-exec";

/// Exit code used to signal an inner timeout, mirroring `timeout(1)`.
const TIMEOUT_CODE: i32 = 124;
/// Exit code the helper uses when the sandbox itself could not be set up, so the
/// caller can transparently fall back to an unsandboxed run.
const SANDBOX_UNAVAILABLE_CODE: i32 = 63;

/// Outcome of a capture run.
#[derive(Debug, Clone)]
pub struct CaptureResult {
    /// Which backend actually ran the command.
    pub backend: &'static str,
    /// Sanitized combined stdout+stderr.
    pub output: String,
    pub timed_out: bool,
    pub success: bool,
    /// Raw child exit code, when the process exited normally.
    pub exit_code: Option<i32>,
}

/// Capture output for a command line, honoring the configured sandbox + timeout.
pub fn run_capture(cfg: &CaptureConfig, command: &str) -> Result<CaptureResult> {
    let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
    if cfg.sandbox {
        match run_via_helper(command, timeout, true) {
            Ok(result) => Ok(result),
            // Sandbox setup can fail (unsupported platform, re-exec issues);
            // fall back to the always-available timeboxed subprocess.
            Err(_) => run_plain(command, timeout),
        }
    } else {
        run_plain(command, timeout)
    }
}

/// Run a command directly in a timeboxed subprocess (no sandbox).
pub fn run_plain(command: &str, timeout: Duration) -> Result<CaptureResult> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_process_group(&mut cmd);
    let child = cmd.spawn().context("spawning capture subprocess")?;
    finish(child, timeout, "subprocess")
}

fn run_via_helper(command: &str, timeout: Duration, sandbox: bool) -> Result<CaptureResult> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let mut cmd = Command::new(exe);
    cmd.arg(CAPTURE_EXEC_ARG)
        .env(CAPTURE_CMD_ENV, command)
        .env(CAPTURE_SANDBOX_ENV, if sandbox { "1" } else { "0" })
        .env(CAPTURE_TIMEOUT_ENV, timeout.as_secs().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_process_group(&mut cmd);
    let child = cmd.spawn().context("spawning capture helper")?;
    // The helper enforces the real timeout on its child; the parent adds a
    // small grace period as a backstop against a wedged helper.
    let backend = if sandbox { "sandbox" } else { "subprocess" };
    let result = finish(child, timeout + Duration::from_secs(3), backend)?;
    // When the sandbox could not be set up, signal an error so the caller falls
    // back to a plain run rather than storing the helper's error as output.
    if result.exit_code == Some(SANDBOX_UNAVAILABLE_CODE) {
        anyhow::bail!("sandbox unavailable");
    }
    Ok(result)
}

/// Drain a child's output concurrently, apply the timeout, and sanitize.
fn finish(mut child: Child, timeout: Duration, backend: &'static str) -> Result<CaptureResult> {
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_string(&mut buf);
        }
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.read_to_string(&mut buf);
        }
        buf
    });

    let status = child.wait_timeout(timeout).context("waiting on capture")?;
    let (mut timed_out, code, success) = match status {
        Some(s) => (false, s.code(), s.success()),
        None => {
            // The child leads its own process group, so kill the whole group to
            // reap any grandchildren it spawned.
            kill_group(&mut child);
            (true, None, false)
        }
    };
    if code == Some(TIMEOUT_CODE) {
        timed_out = true;
    }

    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();
    let mut combined = stdout;
    if !stderr.is_empty() {
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&stderr);
    }

    Ok(CaptureResult {
        backend,
        output: sanitize(&combined),
        timed_out,
        success: success && !timed_out,
        exit_code: code,
    })
}

/// Kill a child that leads its own process group, reaping its descendants.
#[cfg(unix)]
fn kill_group(child: &mut Child) {
    // The child was spawned with `process_group(0)`, so its pgid equals its pid.
    let pgid = child.id() as libc::pid_t;
    unsafe {
        libc::killpg(pgid, libc::SIGKILL);
    }
    let _ = child.wait();
}

#[cfg(not(unix))]
fn kill_group(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Best-effort `--help`/`-h` output for a binary, used as descriptive intel.
pub fn gather_intel(binary: &str) -> Option<String> {
    if binary.is_empty() || which::which(binary).is_err() {
        return None;
    }
    for flag in ["--help", "-h"] {
        if let Ok(result) = run_plain(&format!("{binary} {flag}"), Duration::from_secs(5)) {
            let trimmed = result.output.trim();
            if !trimmed.is_empty() {
                return Some(first_lines(trimmed, 15));
            }
        }
    }
    None
}

fn first_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

/// Entry point for the hidden capture-exec subcommand. Never returns.
pub fn capture_exec_main() -> ! {
    let command = std::env::var(CAPTURE_CMD_ENV).unwrap_or_default();
    let sandbox = std::env::var(CAPTURE_SANDBOX_ENV)
        .map(|v| v == "1")
        .unwrap_or(false);
    let timeout = std::env::var(CAPTURE_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(10);
    let code = exec_inner(&command, sandbox, Duration::from_secs(timeout.max(1)));
    std::process::exit(code);
}

fn exec_inner(command: &str, sandbox: bool, timeout: Duration) -> i32 {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    own_process_group(&mut cmd);

    let spawned = if sandbox {
        match sandboxed_spawn(cmd) {
            Ok(child) => Ok(child),
            Err(e) => {
                eprintln!("cmd-man: sandbox unavailable: {e}");
                return SANDBOX_UNAVAILABLE_CODE;
            }
        }
    } else {
        cmd.spawn().map_err(anyhow::Error::from)
    };

    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            eprintln!("cmd-man: capture failed to start: {e}");
            return 125;
        }
    };

    match child.wait_timeout(timeout) {
        Ok(Some(status)) => {
            let code = status.code().unwrap_or(0);
            // Never let a real child exit code collide with the sandbox-unavailable
            // sentinel, which would otherwise trigger an unsandboxed re-run.
            if code == SANDBOX_UNAVAILABLE_CODE {
                1
            } else {
                code
            }
        }
        Ok(None) => {
            // Kill the command's whole process group so nothing is left running.
            kill_group(&mut child);
            TIMEOUT_CODE
        }
        Err(e) => {
            eprintln!("cmd-man: capture wait failed: {e}");
            125
        }
    }
}

/// Spawn a command inside the filesystem/network sandbox. Denies networking and
/// all writes outside temporary directories.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn sandboxed_spawn(cmd: Command) -> Result<Child> {
    use birdcage::{Birdcage, Exception, Sandbox};

    let mut cage = Birdcage::new();
    cage.add_exception(Exception::ExecuteAndRead("/".into()))?;
    cage.add_exception(Exception::FullEnvironment)?;
    for path in writable_paths() {
        // Missing paths are not fatal; ignore individual failures.
        let _ = cage.add_exception(Exception::WriteAndRead(path.into()));
    }
    // No Exception::Networking is added, so network access is denied.
    let child = cage.spawn(cmd)?;
    Ok(child)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn sandboxed_spawn(_cmd: Command) -> Result<Child> {
    anyhow::bail!("sandboxing is not supported on this platform")
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn writable_paths() -> Vec<String> {
    let mut paths = vec![
        "/tmp".to_string(),
        "/private/tmp".to_string(),
        "/private/var/folders".to_string(),
        "/var/folders".to_string(),
        "/dev/null".to_string(),
        "/dev/tty".to_string(),
        "/dev/stdout".to_string(),
        "/dev/stderr".to_string(),
    ];
    if let Ok(tmp) = std::env::var("TMPDIR")
        && !tmp.is_empty()
    {
        paths.push(tmp);
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_capture_gets_output() {
        let r = run_plain("echo hello", Duration::from_secs(5)).unwrap();
        assert!(r.output.contains("hello"));
        assert!(r.success);
        assert!(!r.timed_out);
        assert_eq!(r.backend, "subprocess");
    }

    #[test]
    fn plain_capture_includes_stderr() {
        let r = run_plain("echo oops 1>&2", Duration::from_secs(5)).unwrap();
        assert!(r.output.contains("oops"));
    }

    #[test]
    fn timeout_kills_long_command() {
        let r = run_plain("sleep 5", Duration::from_secs(1)).unwrap();
        assert!(r.timed_out);
        assert!(!r.success);
    }

    #[test]
    fn unknown_binary_has_no_intel() {
        assert!(gather_intel("definitely-not-a-real-binary-xyz").is_none());
    }
}
