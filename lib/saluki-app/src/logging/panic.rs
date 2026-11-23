//! Panic logging.

use std::{
    backtrace::{Backtrace, BacktraceStatus},
    fmt::Write as _,
    panic::PanicHookInfo,
};

use tracing::error;

/// Part of the symbol of the frame that the standard library's panic machinery ends with. A short backtrace starts
/// after this frame.
const SHORT_BACKTRACE_START_MARKER: &str = "__rust_end_short_backtrace";

/// Part of the symbol of the frame that the standard library's entry point for `main` or a thread starts with. A short
/// backtrace stops before this frame.
const SHORT_BACKTRACE_STOP_MARKER: &str = "__rust_begin_short_backtrace";

/// The note that the standard library's default panic hook writes after a short backtrace.
const SHORT_BACKTRACE_NOTE: &str =
    "note: Some details are omitted, run with `RUST_BACKTRACE=full` for a verbose backtrace.";

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

    // The message names the location, as the first line of the default hook's output does, so the panic reads the
    // same way in the log. The location also stays in its own field for filtering.
    //
    // `capture` follows `RUST_LIB_BACKTRACE` and `RUST_BACKTRACE`, as the default hook does.
    let backtrace = Backtrace::capture();
    if backtrace.status() == BacktraceStatus::Captured {
        let full = std::env::var_os("RUST_BACKTRACE").is_some_and(|value| value == "full");
        let rendered = if full {
            format!("{backtrace:#}")
        } else {
            backtrace.to_string()
        };

        // The backtrace starts on its own line, as the default hook writes it to standard error.
        error!(
            thread = thread_name,
            %location,
            panic_message = message,
            "Application panicked at {}.\n{}",
            location,
            format_backtrace(&rendered, full)
        );
    } else {
        error!(
            thread = thread_name,
            %location,
            panic_message = message,
            "Application panicked at {}.",
            location
        );
    }
}

/// Formats a rendered backtrace the way the standard library's default panic hook writes it to standard error.
///
/// The output starts with a `stack backtrace:` line. Unless `full` is set, only the frames between the panic machinery
/// and the entry point of `main` or the thread remain, numbered from zero, and the default hook's note follows them.
/// With `full`, every frame remains.
fn format_backtrace(rendered: &str, full: bool) -> String {
    // Each frame is its header line, such as `   3: core::panicking::panic_fmt`, and the location lines after it.
    let mut frames: Vec<Vec<&str>> = Vec::new();
    for line in rendered.lines() {
        if frame_symbol(line).is_some() {
            frames.push(vec![line]);
        } else if let Some(frame) = frames.last_mut() {
            frame.push(line);
        }
    }

    let has_symbol = |frame: &Vec<&str>, marker: &str| frame_symbol(frame[0]).is_some_and(|s| s.contains(marker));
    let (start, stop) = if full {
        (0, frames.len())
    } else {
        let start = frames
            .iter()
            .position(|frame| has_symbol(frame, SHORT_BACKTRACE_START_MARKER))
            .map_or(0, |index| index + 1);
        let stop = frames[start..]
            .iter()
            .position(|frame| has_symbol(frame, SHORT_BACKTRACE_STOP_MARKER))
            .map_or(frames.len(), |index| start + index);
        (start, stop)
    };

    let mut output = String::from("stack backtrace:");
    for (index, frame) in frames[start..stop].iter().enumerate() {
        let symbol = frame_symbol(frame[0]).unwrap_or_default();
        let _ = write!(output, "\n{index:4}: {symbol}");
        for line in &frame[1..] {
            output.push('\n');
            output.push_str(line);
        }
    }
    if !full {
        output.push('\n');
        output.push_str(SHORT_BACKTRACE_NOTE);
    }
    output
}

/// Returns the symbol of a backtrace frame header line, such as `   3: core::panicking::panic_fmt`, or `None` for any
/// other line.
fn frame_symbol(line: &str) -> Option<&str> {
    let (index, symbol) = line.trim_start().split_once(": ")?;
    (!index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())).then_some(symbol)
}

#[cfg(test)]
mod tests {
    use super::{format_backtrace, frame_symbol, SHORT_BACKTRACE_NOTE};

    /// A rendered backtrace shaped like one that a panic hook captures: the capture and panic machinery, the frames that
    /// led to the panic, and the entry point.
    const RENDERED: &str = "   0: std::backtrace::Backtrace::create
             at /rustc/library/std/src/backtrace.rs:331:13
   1: saluki_app::logging::panic::log_panic
   2: std::sys::backtrace::__rust_end_short_backtrace
   3: __rustc::rust_begin_unwind
             at /rustc/library/std/src/panicking.rs:689:5
   4: my_app::main
             at ./src/main.rs:10:5
   5: std::sys::backtrace::__rust_begin_short_backtrace
   6: std::rt::lang_start";

    #[test]
    fn short_backtrace_keeps_only_the_frames_between_the_markers() {
        let expected = format!(
            "stack backtrace:
   0: __rustc::rust_begin_unwind
             at /rustc/library/std/src/panicking.rs:689:5
   1: my_app::main
             at ./src/main.rs:10:5
{SHORT_BACKTRACE_NOTE}"
        );
        assert_eq!(format_backtrace(RENDERED, false), expected);
    }

    #[test]
    fn full_backtrace_keeps_every_frame_without_the_note() {
        let formatted = format_backtrace(RENDERED, true);
        assert_eq!(formatted, format!("stack backtrace:\n{RENDERED}"));
        assert!(!formatted.contains("note:"));
    }

    #[test]
    fn short_backtrace_without_markers_keeps_every_frame() {
        let rendered = "   0: my_app::helper\n   1: my_app::main\n             at ./src/main.rs:10:5";
        assert_eq!(
            format_backtrace(rendered, false),
            format!("stack backtrace:\n{rendered}\n{SHORT_BACKTRACE_NOTE}")
        );
    }

    #[test]
    fn frame_symbol_reads_only_frame_header_lines() {
        assert_eq!(
            frame_symbol("  12: core::panicking::panic_fmt"),
            Some("core::panicking::panic_fmt")
        );
        assert_eq!(frame_symbol("             at ./src/main.rs:10:5"), None);
        assert_eq!(frame_symbol("             at C:\\src\\main.rs:10:5"), None);
        assert_eq!(frame_symbol("note: Some details are omitted"), None);
    }
}
