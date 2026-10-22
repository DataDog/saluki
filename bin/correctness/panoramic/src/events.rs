//! Events for test execution reporting.
//!
//! The consume loop records runner events in the run report and forwards them to the TUI or plain output.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::reporter::TestResult;

/// Events emitted during test execution.
#[derive(Clone, Debug)]
pub enum TestEvent {
    /// The test run is starting.
    RunStarted {
        /// Names of the selected tests, in selection order.
        tests: Vec<String>,
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

        /// The directory where this test keeps its logs. Determined and created by the Runner object.
        log_dir: PathBuf,
    },

    /// A plain status message to display in the log/TUI (for example, infrastructure setup progress).
    StatusLine {
        /// The message to display.
        message: String,
    },
}

/// Create a channel for sending test events.
pub fn create_event_channel() -> (mpsc::UnboundedSender<TestEvent>, mpsc::UnboundedReceiver<TestEvent>) {
    mpsc::unbounded_channel()
}
