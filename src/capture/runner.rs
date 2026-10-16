//! Running commands to capture example output: a hidden re-exec helper applies
//! the sandbox in a dedicated process, and every run is hard-timeboxed.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
/// Env var carrying the working directory the captured command runs in.
pub const CAPTURE_CWD_ENV: &str = "CMD_MAN_CAPTURE_CWD";
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

/// Capture output for a command line, honoring the configured sandbox +
/// timeout. `cwd` is the directory the command runs in, defaulting to the
/// process working directory.
pub fn run_capture(
    cfg: &CaptureConfig,
    command: &str,
    cwd: Option<&Path>,
) -> Result<CaptureResult> {
    let timeout = Duration::from_secs(cfg.timeout_secs.max(1));
    if cfg.sandbox {
        match run_via_helper(command, timeout, true, cwd) {
            Ok(result) => Ok(result),
            // Sandbox setup can fail (unsupported platform, re-exec issues);
            // fall back to the always-available timeboxed subprocess.
            Err(_) => run_plain(command, timeout, cwd),
        }
    } else {
        run_plain(command, timeout, cwd)
    }
}

/// Run a command directly in a timeboxed subprocess (no sandbox).
pub fn run_plain(command: &str, timeout: Duration, cwd: Option<&Path>) -> Result<CaptureResult> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    set_cwd(&mut cmd, cwd);
    own_process_group(&mut cmd);
    let child = cmd.spawn().context("spawning capture subprocess")?;
    finish(child, timeout, "subprocess")
}

/// Point a shell at `cwd`, keeping `PWD` in step the way a real `cd` does so
/// the command sees the same directory however it asks for it.
fn set_cwd(cmd: &mut Command, cwd: Option<&Path>) {
    if let Some(dir) = cwd {
        cmd.current_dir(dir).env("PWD", dir);
    }
}

fn run_via_helper(
    command: &str,
    timeout: Duration,
    sandbox: bool,
    cwd: Option<&Path>,
) -> Result<CaptureResult> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let mut cmd = Command::new(exe);
    cmd.arg(CAPTURE_EXEC_ARG)
        .env(CAPTURE_CMD_ENV, command)
        .env(CAPTURE_SANDBOX_ENV, if sandbox { "1" } else { "0" })
        .env(CAPTURE_TIMEOUT_ENV, timeout.as_secs().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The helper reads the directory back out of the environment and applies it
    // to the shell it spawns; setting it here alone would be a no-op.
    match cwd {
        Some(dir) => {
            cmd.env(CAPTURE_CWD_ENV, dir);
        }
        None => {
            cmd.env_remove(CAPTURE_CWD_ENV);
        }
    }
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
    let deadline = Instant::now() + timeout;
    let stdout = spawn_reader(child.stdout.take());
    let stderr = spawn_reader(child.stderr.take());

    let status = child.wait_timeout(timeout).context("waiting on capture")?;
    // Kill the child's whole process group even after a normal exit, so nothing
    // it backgrounded keeps running or holds the output pipes open.
    kill_group(&mut child);
    let (mut timed_out, code, success) = match status {
        Some(s) => (false, s.code(), s.success()),
        None => (true, None, false),
    };
    if code == Some(TIMEOUT_CODE) {
        timed_out = true;
    }

    let mut combined = collect(stdout, deadline);
    let stderr = collect(stderr, deadline);
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

/// Captured output beyond this is drained but not kept.
const OUTPUT_LIMIT: usize = 1024 * 1024;

struct Reader {
    buffer: Arc<Mutex<Vec<u8>>>,
    finished: Receiver<()>,
    abandoned: Arc<AtomicBool>,
}

fn spawn_reader(source: Option<impl Read + Send + 'static>) -> Reader {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let abandoned = Arc::new(AtomicBool::new(false));
    let (done, finished) = mpsc::channel();
    let sink = Arc::clone(&buffer);
    let stop = Arc::clone(&abandoned);
    std::thread::spawn(move || {
        if let Some(mut source) = source {
            let mut chunk = [0u8; 8192];
            while let Ok(n @ 1..) = source.read(&mut chunk) {
                // Dropping the pipe once abandoned lets its writer hit EPIPE.
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let mut sink = sink.lock().unwrap();
                let room = OUTPUT_LIMIT.saturating_sub(sink.len());
                sink.extend_from_slice(&chunk[..n.min(room)]);
            }
        }
        let _ = done.send(());
    });
    Reader {
        buffer,
        finished,
        abandoned,
    }
}

/// What a reader has gathered once its pipe closes, or by `deadline` if a
/// process outside the killed group still holds the pipe open.
fn collect(reader: Reader, deadline: Instant) -> String {
    let wait = deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(500));
    if reader.finished.recv_timeout(wait).is_err() {
        reader.abandoned.store(true, Ordering::Relaxed);
    }
    String::from_utf8_lossy(&reader.buffer.lock().unwrap()).into_owned()
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
        if let Ok(result) = run_plain(&format!("{binary} {flag}"), Duration::from_secs(5), None) {
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
    let cwd = std::env::var_os(CAPTURE_CWD_ENV)
        .map(PathBuf::from)
        .filter(|dir| dir.is_dir());
    let code = exec_inner(
        &command,
        sandbox,
        Duration::from_secs(timeout.max(1)),
        cwd.as_deref(),
    );
    std::process::exit(code);
}

fn exec_inner(command: &str, sandbox: bool, timeout: Duration, cwd: Option<&Path>) -> i32 {
    if sandbox {
        return exec_sandboxed(command, timeout, cwd);
    }
    let mut cmd = shell_command(command);
    set_cwd(&mut cmd, cwd);
    own_process_group(&mut cmd);
    match cmd.spawn() {
        Ok(child) => wait_exit_code(child, timeout),
        Err(e) => start_failed(e),
    }
}

fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd
}

fn wait_exit_code(mut child: Child, timeout: Duration) -> i32 {
    let waited = child.wait_timeout(timeout);
    // Kill the command's whole process group, after a normal exit as well, so
    // nothing it backgrounded is left running.
    kill_group(&mut child);
    match waited {
        Ok(Some(status)) => exit_code(status),
        Ok(None) => TIMEOUT_CODE,
        Err(e) => wait_failed(e),
    }
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    let code = status.code().unwrap_or(0);
    // Never let a real child exit code collide with the sandbox-unavailable
    // sentinel, which would otherwise trigger an unsandboxed re-run.
    if code == SANDBOX_UNAVAILABLE_CODE {
        1
    } else {
        code
    }
}

fn start_failed(e: impl std::fmt::Display) -> i32 {
    eprintln!("cmd-man: capture failed to start: {e}");
    125
}

fn wait_failed(e: impl std::fmt::Display) -> i32 {
    eprintln!("cmd-man: capture wait failed: {e}");
    125
}

fn sandbox_unavailable(e: impl std::fmt::Display) -> i32 {
    eprintln!("cmd-man: sandbox unavailable: {e}");
    SANDBOX_UNAVAILABLE_CODE
}

/// Denies networking and all writes outside temporary directories.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn cage() -> Result<birdcage::Birdcage> {
    use birdcage::{Birdcage, Exception, Sandbox};

    let mut cage = Birdcage::new();
    cage.add_exception(Exception::ExecuteAndRead("/".into()))?;
    cage.add_exception(Exception::FullEnvironment)?;
    for path in writable_paths() {
        // Missing paths are not fatal; ignore individual failures.
        let _ = cage.add_exception(Exception::WriteAndRead(path.into()));
    }
    Ok(cage)
}

#[cfg(target_os = "macos")]
fn exec_sandboxed(command: &str, timeout: Duration, cwd: Option<&Path>) -> i32 {
    use birdcage::Sandbox;

    let mut cmd = shell_command(command);
    set_cwd(&mut cmd, cwd);
    own_process_group(&mut cmd);
    match cage().and_then(|cage| Ok(cage.spawn(cmd)?)) {
        Ok(child) => wait_exit_code(child, timeout),
        Err(e) => sandbox_unavailable(e),
    }
}

/// On Linux birdcage spawns the command as PID 1 of fresh namespaces through its
/// own process types, which carry no working directory, so the helper moves
/// itself there first and the sandbox inherits it.
#[cfg(target_os = "linux")]
fn exec_sandboxed(command: &str, timeout: Duration, cwd: Option<&Path>) -> i32 {
    use birdcage::Sandbox;
    use birdcage::process::{Command, Stdio};

    if let Some(dir) = cwd {
        if let Err(e) = std::env::set_current_dir(dir) {
            return start_failed(e);
        }
        // SAFETY: the capture helper is single-threaded.
        unsafe { std::env::set_var("PWD", dir) };
    }
    // A sandbox whose setup fails after the spawn reports it only as exit code 1
    // from inside, indistinguishable from the command's own, so prove it first.
    let mut probe = Command::new("sh");
    probe
        .arg("-c")
        .arg("exit 97")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match cage().and_then(|cage| Ok(cage.spawn(probe)?.wait()?)) {
        Ok(status) if status.code() == Some(97) => {}
        Ok(status) => return sandbox_unavailable(format!("setup probe exited with {status}")),
        Err(e) => return sandbox_unavailable(e),
    }

    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = match cage().and_then(|cage| Ok(cage.spawn(cmd)?)) {
        Ok(child) => child,
        Err(e) => return sandbox_unavailable(e),
    };

    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return exit_code(status),
            Ok(None) if std::time::Instant::now() >= deadline => {
                // Killing PID 1 of the sandbox's PID namespace kills everything in it.
                let _ = child.kill();
                let _ = child.wait();
                return TIMEOUT_CODE;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return wait_failed(e);
            }
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn exec_sandboxed(_command: &str, _timeout: Duration, _cwd: Option<&Path>) -> i32 {
    sandbox_unavailable("sandboxing is not supported on this platform")
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
        let r = run_plain("echo hello", Duration::from_secs(5), None).unwrap();
        assert!(r.output.contains("hello"));
        assert!(r.success);
        assert!(!r.timed_out);
        assert_eq!(r.backend, "subprocess");
    }

    #[test]
    fn plain_capture_includes_stderr() {
        let r = run_plain("echo oops 1>&2", Duration::from_secs(5), None).unwrap();
        assert!(r.output.contains("oops"));
    }

    #[test]
    fn timeout_kills_long_command() {
        let r = run_plain("sleep 5", Duration::from_secs(1), None).unwrap();
        assert!(r.timed_out);
        assert!(!r.success);
    }

    #[test]
    fn backgrounded_processes_do_not_outlive_a_normal_exit() {
        let started = Instant::now();
        let r = run_plain("sleep 30 & echo hello", Duration::from_secs(10), None).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(r.output.contains("hello"));
        assert!(r.success);
        assert!(!r.timed_out);
    }

    #[test]
    fn collecting_output_stops_at_the_deadline_when_the_pipe_stays_open() {
        let mut holder = Command::new("sh")
            .arg("-c")
            .arg("echo partial; sleep 30")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let reader = spawn_reader(holder.stdout.take());
        std::thread::sleep(Duration::from_millis(200));

        let started = Instant::now();
        let output = collect(reader, Instant::now());

        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(output, "partial\n");
        let _ = holder.kill();
        let _ = holder.wait();
    }

    #[test]
    fn an_abandoned_reader_stops_reading_so_its_writer_dies() {
        let mut writer = Command::new("yes").stdout(Stdio::piped()).spawn().unwrap();
        let reader = spawn_reader(writer.stdout.take());
        let buffer = Arc::clone(&reader.buffer);

        let output = collect(reader, Instant::now());

        assert!(output.len() <= OUTPUT_LIMIT);
        let exited = (0..100).any(|_| {
            std::thread::sleep(Duration::from_millis(20));
            writer.try_wait().unwrap().is_some()
        });
        if !exited {
            let _ = writer.kill();
        }
        assert!(
            exited,
            "the writer kept running after the reader was abandoned"
        );
        assert!(buffer.lock().unwrap().len() <= OUTPUT_LIMIT);
    }

    #[test]
    fn unknown_binary_has_no_intel() {
        assert!(gather_intel("definitely-not-a-real-binary-xyz").is_none());
    }
}
