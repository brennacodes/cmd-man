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

/// Start a command in a new session, leaving it with no controlling terminal so
/// nothing it runs can stop to prompt there.
#[cfg(unix)]
pub fn new_session(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: setsid is async-signal-safe and touches no parent state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
pub fn new_session(_cmd: &mut Command) {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;

    #[test]
    fn a_new_session_leads_itself() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        new_session(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        // SAFETY: getsid only reads the session of the child spawned above.
        let session = unsafe { libc::getsid(pid) };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(session, pid);
    }

    #[test]
    fn a_process_group_alone_stays_in_this_session() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30").process_group(0);
        let mut child = cmd.spawn().unwrap();
        let pid = child.id() as libc::pid_t;
        // SAFETY: getsid only reads the session of the child spawned above.
        let session = unsafe { libc::getsid(pid) };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_ne!(session, pid);
    }
}
