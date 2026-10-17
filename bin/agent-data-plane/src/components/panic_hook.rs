//! A global panic hook that logs panics with a bounded stack trace.
//!
//! The hook is installed unconditionally and chains whatever hook is current, so instrumentation
//! registered first stays intact. A panicking task is already isolated by the runtime—the unwind
//! stops at the task boundary—so the hook only observes: it logs the message with a bounded stack
//! and defers to the previous hook.

use std::backtrace::Backtrace;

use tracing::error;

/// How many bytes of the stack are kept for the log line.
const STACK_LOG_BYTES: usize = 4096;

/// Installs the global panic hook.
///
/// Must be called before any panic can occur, after any instrumentation hook that should stay
/// active: the installed hook chains the current one, so panic logging observes first and the
/// previous hook—the default formatter or test instrumentation—still runs.
pub fn install_panic_hook() {
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = panic_message(info);

        // Forced capture ignores the backtrace environment, so the stack is available in every
        // build configuration.
        let backtrace = Backtrace::force_capture().to_string();
        let stack = truncate_bytes(&backtrace, STACK_LOG_BYTES);
        error!(panic = %message, stack, "A task panicked.");

        previous_hook(info);
    }));
}

/// Extracts the panic message, wherever the payload carries one.
fn panic_message(info: &std::panic::PanicHookInfo<'_>) -> String {
    payload_message(info.payload())
}

fn payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|value| (*value).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

/// Truncates to at most `max_bytes` bytes, backing off to a character boundary.
fn truncate_bytes(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panic_message_extracts_string_payloads() {
        // The hook itself is global, so installing it in tests would observe every other test's
        // panics; the payload path is exercised directly instead.
        let static_str: &(dyn std::any::Any + Send) = &"index out of bounds: the len is 3";
        assert_eq!(payload_message(static_str), "index out of bounds: the len is 3");

        let owned: &(dyn std::any::Any + Send) = &String::from("owned message");
        assert_eq!(payload_message(owned), "owned message");

        let other: &(dyn std::any::Any + Send) = &17;
        assert_eq!(payload_message(other), "<non-string panic payload>");
    }

    #[test]
    fn stacks_truncate_to_the_byte_bound() {
        assert_eq!(truncate_bytes("short", 4096), "short");

        let long = "a".repeat(8192);
        assert_eq!(truncate_bytes(&long, 4096).len(), 4096);

        // The cut lands inside a multi-byte character and backs off to the boundary.
        let multibyte = "é".repeat(4096);
        let truncated = truncate_bytes(&multibyte, 4096);
        assert!(multibyte.is_char_boundary(truncated.len()));
        assert!(truncated.chars().all(|c| c == 'é'));
    }
}
