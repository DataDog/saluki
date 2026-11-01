//! Routes setup messages to stderr or tracing and reports command setup failures.

use std::{fmt::Display, process::ExitCode};

use tracing::{error, info, warn};

use crate::collector::{EXIT_HARNESS_ERROR, EXIT_NO_TESTS_SELECTED};

/// Routes setup messages to stderr while the TUI owns the terminal, otherwise to tracing.
pub(crate) struct Console {
    /// Whether the TUI owns stdout.
    pub(crate) use_tui: bool,
}

impl Console {
    pub(crate) fn info(&self, message: impl Display) {
        if self.use_tui {
            eprintln!("{message}");
        } else {
            info!("{message}");
        }
    }

    pub(crate) fn warn(&self, message: impl Display) {
        if self.use_tui {
            eprintln!("{message}");
        } else {
            warn!("{message}");
        }
    }

    pub(crate) fn error(&self, message: impl Display) {
        if self.use_tui {
            eprintln!("{message}");
        } else {
            error!("{message}");
        }
    }
}

/// A setup check that ended the command before any case ran.
#[derive(Debug)]
pub(crate) struct SetupFailure {
    pub(crate) message: String,
    pub(crate) exit: ExitCode,
}

impl SetupFailure {
    /// A harness error (exit 2).
    pub(crate) fn harness(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit: ExitCode::from(EXIT_HARNESS_ERROR),
        }
    }

    /// A case discovery error (exit 2).
    pub(crate) fn discovery(error: impl Display) -> Self {
        Self::harness(format!("Failed to discover tests: {error:#}"))
    }

    /// An empty or unmatched `-t` selection (exit 3).
    pub(crate) fn no_tests_selected(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            exit: ExitCode::from(EXIT_NO_TESTS_SELECTED),
        }
    }

    /// Reports the message as an error and returns the exit code.
    pub(crate) fn report(self, console: &Console) -> ExitCode {
        console.error(self.message);
        self.exit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_writes_each_message_to_tracing_at_its_level_outside_the_tui() {
        let output = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let console = Console { use_tui: false };
            console.info("discovered cases");
            console.warn("skipped cases");
            console.error("invalid cases");
        });
        let log = std::fs::read_to_string(output.path()).unwrap();
        assert_eq!(
            log.lines().map(str::trim).collect::<Vec<_>>(),
            ["INFO discovered cases", "WARN skipped cases", "ERROR invalid cases"]
        );
    }

    #[test]
    fn a_setup_failure_reports_its_message_as_an_error_and_returns_its_exit_code() {
        let output = tempfile::NamedTempFile::new().unwrap();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(false)
            .with_writer(std::sync::Mutex::new(output.reopen().unwrap()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let console = Console { use_tui: false };
            for (failure, exit) in [
                (SetupFailure::harness("invalid cases"), 2),
                (SetupFailure::no_tests_selected("empty selection"), 3),
            ] {
                assert_eq!(failure.report(&console), ExitCode::from(exit));
            }
        });
        let log = std::fs::read_to_string(output.path()).unwrap();
        assert_eq!(
            log.lines().collect::<Vec<_>>(),
            ["ERROR invalid cases", "ERROR empty selection"]
        );
    }
}
