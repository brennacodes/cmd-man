fn main() {
    // The hidden capture helper must run before anything else and stay
    // single-threaded so the sandbox can be applied to this process.
    if std::env::args().nth(1).as_deref() == Some(cmd_man::capture::CAPTURE_EXEC_ARG) {
        cmd_man::capture::capture_exec_main();
    }

    if let Err(e) = cmd_man::cli::run() {
        eprintln!("cmd-man: {e:#}");
        std::process::exit(1);
    }
}
