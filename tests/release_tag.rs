use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn check(tag: &str, manifest: &str) -> Output {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("Cargo.toml");
    fs::write(&path, manifest).unwrap();
    Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/check-release-tag.sh"))
        .arg(tag)
        .arg(&path)
        .output()
        .unwrap()
}

const MANIFEST: &str = "[package]\nname = \"cmd-man\"\nversion = \"1.2.3\"\n\n\
                        [dependencies]\nclap = { version = \"4.0.0\" }\n";

#[test]
fn a_tag_equal_to_the_package_version_passes() {
    assert!(check("1.2.3", MANIFEST).status.success());
}

#[test]
fn a_release_candidate_tag_passes_when_the_version_matches() {
    let manifest = MANIFEST.replace("1.2.3", "1.2.3-rc.2");
    assert!(check("1.2.3-rc.2", &manifest).status.success());
}

#[test]
fn a_tag_that_differs_from_the_package_version_fails() {
    let output = check("1.2.4", MANIFEST);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not match"));
}

#[test]
fn a_dependency_version_is_not_mistaken_for_the_package_version() {
    let manifest = "[dependencies]\nclap = \"4.0.0\"\n\n[package]\nversion = \"1.2.3\"\n";
    assert!(!check("4.0.0", manifest).status.success());
    assert!(check("1.2.3", manifest).status.success());
}

#[test]
fn non_release_tags_fail() {
    for tag in ["v1.2.3", "1.2", "1.2.3-beta", "1.2.3-rc.1-extra"] {
        let output = check(tag, MANIFEST);
        assert!(!output.status.success(), "{tag} was accepted");
        assert!(String::from_utf8_lossy(&output.stderr).contains("not a release tag"));
    }
}

#[test]
fn the_repository_manifest_has_a_readable_version() {
    let version = env!("CARGO_PKG_VERSION");
    let output = Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/check-release-tag.sh"))
        .arg(version)
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
