//! Panic logging.

use std::{
    backtrace::{Backtrace, BacktraceStatus},
    panic::PanicHookInfo,
};

use tracing::error;

/// Installs a panic hook that logs each panic through `tracing`, and then calls the hook that was installed before it.
///
/// The standard library's default hook writes panics only to standard error. A supervisor that discards standard
/// error, such as a service manager with no console, then loses the panic, and the application's own log shows nothing
/// past the last line before the crash. This hook puts the panic in the application's logs as well.
///
/// The log outputs are non-blocking, so the hook only queues the log line. A panic that unwinds out of `main` drops the
/// [`LoggingGuard`][super::LoggingGuard] on the way, which flushes the queued line before the process exits. The
/// previous hook still runs afterwards, so standard error keeps its usual panic output.
pub(super) fn install_panic_hook() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log_panic(info);
        previous_hook(info);
    }));
}

fn log_panic(info: &PanicHookInfo<'_>) {
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("<unnamed>");
    let location = info
        .location()
        .map_or_else(|| "<unknown>".to_string(), ToString::to_string);
    let message = info.payload_as_str().unwrap_or("<non-string panic payload>");

    // `capture` follows `RUST_BACKTRACE` and `RUST_LIB_BACKTRACE`, like the default hook.
    let backtrace = Backtrace::capture();
    if backtrace.status() == BacktraceStatus::Captured {
        error!(thread = thread_name, %location, %backtrace, panic_message = message, "Application panicked.");
    } else {
        error!(thread = thread_name, %location, panic_message = message, "Application panicked.");
    }
}
