//! Tests for `dist/install.sh`, run under `sh` so a bash-only construct fails here
//! on systems whose `sh` is dash. The network is replaced by a fake `curl` that
//! serves a local archive and records the URL it was asked for.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("dist/install.sh")
}

fn call(function_call: &str) -> Output {
    Command::new("sh")
        .arg("-c")
        .arg(format!(". \"$0\"; {function_call}"))
        .arg(script())
        .env("CMD_MAN_INSTALL_SOURCED", "1")
        .output()
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// Builds a release-shaped archive holding a stand-in `cmd-man` binary.
fn release_archive(dir: &Path) -> PathBuf {
    let staging = dir.join("staging");
    fs::create_dir_all(&staging).unwrap();
    write_executable(
        &staging.join("cmd-man"),
        "#!/bin/sh\necho \"cmd-man 9.9.9\"\n",
    );
    let archive = dir.join("cmd-man.tar.gz");
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&archive)
        .arg("-C")
        .arg(&staging)
        .arg("cmd-man")
        .status()
        .unwrap();
    assert!(status.success());
    archive
}

#[test]
fn supported_platforms_map_to_release_asset_names() {
    for (os, arch, expected) in [
        ("Darwin", "arm64", "darwin-arm64"),
        ("Darwin", "x86_64", "darwin-x64"),
        ("Linux", "x86_64", "linux-x64"),
        ("Linux", "amd64", "linux-x64"),
        ("Linux", "aarch64", "linux-arm64"),
        ("Linux", "arm64", "linux-arm64"),
    ] {
        let output = call(&format!("detect_platform {os} {arch}"));
        assert!(output.status.success(), "{os} {arch}: {}", stderr(&output));
        assert_eq!(stdout(&output), expected, "{os} {arch}");
    }
}

#[test]
fn unsupported_platforms_are_refused() {
    for (os, arch, reason) in [
        ("MINGW64_NT-10.0", "x86_64", "unsupported operating system"),
        ("FreeBSD", "amd64", "unsupported operating system"),
        ("Linux", "armv7l", "unsupported architecture"),
        ("Darwin", "i386", "unsupported architecture"),
    ] {
        let output = call(&format!("platform=$(detect_platform {os} {arch})"));
        assert!(!output.status.success(), "{os} {arch} was accepted");
        assert!(
            stderr(&output).contains(reason),
            "{os} {arch}: {}",
            stderr(&output)
        );
    }
}

#[test]
fn release_url_uses_latest_unless_a_version_is_pinned() {
    let latest = call("release_url linux-x64 ''");
    assert_eq!(
        stdout(&latest),
        "https://github.com/brennacodes/cmd-man/releases/latest/download/cmd-man-linux-x64.tar.gz"
    );

    let pinned = call("release_url darwin-arm64 1.2.3-rc.1");
    assert_eq!(
        stdout(&pinned),
        "https://github.com/brennacodes/cmd-man/releases/download/1.2.3-rc.1/cmd-man-darwin-arm64.tar.gz"
    );
}

#[test]
fn an_archive_without_the_binary_is_refused() {
    let temp = TempDir::new().unwrap();
    let staging = temp.path().join("staging");
    fs::create_dir_all(&staging).unwrap();
    fs::write(staging.join("README"), "not a binary").unwrap();
    let archive = temp.path().join("bad.tar.gz");
    assert!(
        Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(&staging)
            .arg("README")
            .status()
            .unwrap()
            .success()
    );
    let install_dir = temp.path().join("bin");

    let output = call(&format!(
        "install_archive '{}' '{}'",
        archive.display(),
        install_dir.display()
    ));

    assert!(!output.status.success());
    assert!(stderr(&output).contains("does not contain a cmd-man binary"));
    assert!(!install_dir.join("cmd-man").exists());
}

fn run_installer(temp: &Path, version: Option<&str>) -> (Output, PathBuf, String) {
    let archive = release_archive(temp);
    let fake_bin = temp.join("fake-bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let url_log = temp.join("requested-url");
    write_executable(
        &fake_bin.join("curl"),
        &format!(
            "#!/bin/sh\n\
             while [ $# -gt 0 ]; do\n\
             case \"$1\" in\n\
             -o) out=\"$2\"; shift 2 ;;\n\
             -*) shift ;;\n\
             *) echo \"$1\" > '{log}'; shift ;;\n\
             esac\n\
             done\n\
             cp '{archive}' \"$out\"\n",
            log = url_log.display(),
            archive = archive.display(),
        ),
    );
    let install_dir = temp.join("install");
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());

    let mut command = Command::new("sh");
    command
        .arg(script())
        .env("PATH", path)
        .env("CMD_MAN_INSTALL_DIR", &install_dir)
        .env_remove("CMD_MAN_VERSION")
        .env_remove("CMD_MAN_INSTALL_SOURCED");
    if let Some(version) = version {
        command.env("CMD_MAN_VERSION", version);
    }
    let output = command.output().unwrap();
    let requested = fs::read_to_string(&url_log).unwrap_or_default();
    (output, install_dir, requested.trim().to_string())
}

#[test]
fn installs_the_latest_release_into_the_install_dir() {
    let temp = TempDir::new().unwrap();

    let (output, install_dir, requested) = run_installer(temp.path(), None);

    assert!(output.status.success(), "{}", stderr(&output));
    let binary = install_dir.join("cmd-man");
    let mode = fs::metadata(&binary).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o755);
    assert!(requested.contains("/releases/latest/download/cmd-man-"));
    let out = stdout(&output);
    assert!(out.contains("cmd-man 9.9.9"), "{out}");
    assert!(out.contains("Add "), "missing PATH hint: {out}");
}

#[test]
fn installs_a_pinned_version() {
    let temp = TempDir::new().unwrap();

    let (output, install_dir, requested) = run_installer(temp.path(), Some("0.2.0"));

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(install_dir.join("cmd-man").exists());
    assert!(
        requested.starts_with("https://github.com/brennacodes/cmd-man/releases/download/0.2.0/"),
        "{requested}"
    );
}
