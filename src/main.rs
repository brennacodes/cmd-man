fn main() {
    // The hidden capture helper must run before anything else and stay
    // single-threaded so the sandbox can be applied to this process.
    if std::env::args().nth(1).as_deref() == Some(cmd_man::capture::CAPTURE_EXEC_ARG) {
        cmd_man::capture::capture_exec_main();
    }

    // The detached background sync helper. Not sandboxed, so it can run after the
    // capture check, but still before clap so it is never treated as a subcommand.
    if std::env::args().nth(1).as_deref() == Some(cmd_man::backup::SYNC_EXEC_ARG) {
        cmd_man::backup::sync_exec_main();
    }

    if let Err(e) = cmd_man::cli::run() {
        eprintln!("cmd-man: {e:#}");
        std::process::exit(1);
    }
}
