//! Events a run emits as it progresses.
//!
//! A run sends them over the channel from [`create_event_channel`]; the consumer records and displays them.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::result::TestResult;

/// Events emitted during test execution.
#[derive(Clone, Debug)]
pub enum TestEvent {
    /// The test run is starting.
    RunStarted {
        /// Total number of tests to run.
        total_tests: usize,
    },

    /// A test has started running.
    TestStarted {
        /// Name of the test that started.
        name: String,
    },

    /// A test has completed (passed or failed).
    TestCompleted {
        /// The result of the completed test.
        result: Box<TestResult>,

        /// The directory where this test keeps its logs, created by the runner before the test starts.
        log_dir: PathBuf,
    },

    /// A plain status message to display in the log/TUI (for example, infrastructure setup progress).
    StatusLine {
        /// The message to display.
        message: String,
    },
}

/// Sends a run's events to its consumer.
pub type EventSender = mpsc::UnboundedSender<TestEvent>;

/// Create a channel for sending test events.
pub fn create_event_channel() -> (EventSender, mpsc::UnboundedReceiver<TestEvent>) {
    mpsc::unbounded_channel()
}
