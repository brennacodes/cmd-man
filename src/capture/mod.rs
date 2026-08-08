//! Safe example-output capture: classify, sandbox, run, and sanitize.

pub mod classifier;
pub mod help;
pub mod runner;
pub mod sanitize;

pub use classifier::{Assessment, classify, primary_binary};
pub use help::{HelpSections, fetch_help, parse_help};
pub use runner::{
    CAPTURE_EXEC_ARG, CaptureResult, capture_exec_main, gather_intel, run_capture, run_plain,
};
pub use sanitize::{Sanitizer, sanitize};
