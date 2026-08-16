//! Small process-spawning helpers shared across the crate.

use std::process::Command;

/// Make a command start its own process group so its whole subtree can be
/// signalled together and it is detached from the parent's controlling group.
#[cfg(unix)]
pub fn own_process_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(not(unix))]
pub fn own_process_group(_cmd: &mut Command) {}
