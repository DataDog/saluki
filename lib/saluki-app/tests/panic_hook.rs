//! Checks that a panic reaches the configured log file, and that standard error still gets the usual panic output.
//!
//! The panic hook is process-wide, so the panicking half runs in a child process: the parent re-runs this test binary
//! with only `child_panics_after_bootstrap` selected and [`CHILD_LOG_FILE_ENV`] set to the log file to use.

use std::{path::Path, process::Command};

use saluki_app::{bootstrap::AppBootstrapper, logging::LoggingConfiguration};

/// Set in the child process to the path of the log file to write. Unset in a normal test run.
const CHILD_LOG_FILE_ENV: &str = "SALUKI_APP_PANIC_HOOK_TEST_LOG_FILE";

/// The panic message that the child uses, and that the parent looks for.
const PANIC_MESSAGE: &str = "panic hook test sentinel";

#[test]
fn panic_is_logged_to_file_and_standard_error() {
    let (log, stderr) = run_child(None);

    let panic_line = log
        .lines()
        .find(|line| line.contains(PANIC_MESSAGE))
        .unwrap_or_else(|| panic!("log file should contain the panic\nlog:\n{log}\nstderr:\n{stderr}"));
    assert!(
        panic_line.contains("ERROR"),
        "panic should be logged at ERROR: {panic_line}"
    );
    assert!(
        panic_line.contains("panic_hook.rs"),
        "panic line should include the panic location: {panic_line}"
    );
    assert!(
        !log.contains("stack backtrace:"),
        "no backtrace should be logged without RUST_BACKTRACE\nlog:\n{log}"
    );

    // The previous hook, here the default one, still runs.
    assert!(
        stderr.contains("panicked at") && stderr.contains(PANIC_MESSAGE),
        "standard error should keep the default panic output\nstderr:\n{stderr}"
    );
}

#[test]
fn panic_backtrace_is_logged_like_standard_error() {
    let (log, stderr) = run_child(Some("1"));

    assert!(
        log.contains("Application panicked.\nstack backtrace:\n"),
        "the backtrace should start on its own line after the message\nlog:\n{log}"
    );

    let logged = backtrace_frames(&log);
    let printed = backtrace_frames(&stderr);
    assert!(!logged.is_empty(), "log should contain backtrace frames\nlog:\n{log}");
    assert_eq!(
        logged, printed,
        "logged frames should match the default hook's\nlog:\n{log}\nstderr:\n{stderr}"
    );
    assert!(
        !logged.iter().any(|frame| frame.contains("saluki_app::logging::panic")),
        "the hook's own frames should be left out\nlog:\n{log}"
    );
}

/// Runs [`child_panics_after_bootstrap`] in a child process, and returns its log file and standard error.
///
/// `backtrace` sets `RUST_BACKTRACE` in the child. `None` unsets both backtrace variables.
fn run_child(backtrace: Option<&str>) -> (String, String) {
    let dir = tempfile::tempdir().expect("create temporary directory");
    let log_file = dir.path().join("app.log");

    let mut command = Command::new(std::env::current_exe().expect("find test binary"));
    command
        .args([
            "child_panics_after_bootstrap",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_LOG_FILE_ENV, &log_file)
        .env_remove("RUST_LIB_BACKTRACE");
    match backtrace {
        Some(value) => command.env("RUST_BACKTRACE", value),
        None => command.env_remove("RUST_BACKTRACE"),
    };
    let output = command.output().expect("run child test");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    assert!(
        !output.status.success(),
        "child test should fail because it panics\nstderr:\n{stderr}"
    );

    (read_log(&log_file), stderr)
}

/// Returns the frame header lines, such as `0: core::panicking::panic_fmt`, of the first backtrace in `text`.
fn backtrace_frames(text: &str) -> Vec<String> {
    let Some((_, backtrace)) = text.split_once("stack backtrace:\n") else {
        return Vec::new();
    };
    backtrace
        .lines()
        .take_while(|line| !line.starts_with("note:"))
        .map(str::trim_start)
        .filter(|line| {
            line.split_once(": ")
                .is_some_and(|(index, _)| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
        })
        .map(str::to_string)
        .collect()
}

/// The child half of the tests above. Does nothing unless [`CHILD_LOG_FILE_ENV`] is set.
#[tokio::test]
async fn child_panics_after_bootstrap() {
    let Some(log_file) = std::env::var_os(CHILD_LOG_FILE_ENV) else {
        return;
    };

    let mut config = LoggingConfiguration::simple();
    config.log_to_console = false;
    config.log_file = log_file.to_string_lossy().into_owned();

    // Hold the guard, as an application does. Unwinding from the panic drops it, which flushes the log file.
    let _bootstrap = AppBootstrapper::new()
        .with_logging_configuration(config)
        .bootstrap()
        .await
        .expect("bootstrap");

    panic!("{PANIC_MESSAGE}");
}

fn read_log(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read log file {}: {e}", path.display()))
}
