//! Event types for test execution reporting.
//!
//! These events are emitted by the test runner and consumed by either the TUI
//! or a logging consumer, depending on the output mode.

use tokio::sync::mpsc;

use crate::reporter::TestResult;

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
        result: TestResult,
    },

    /// All tests have finished.
    AllDone,
}

/// Create a channel for sending test events.
pub fn create_event_channel() -> (mpsc::UnboundedSender<TestEvent>, mpsc::UnboundedReceiver<TestEvent>) {
    mpsc::unbounded_channel()
}
