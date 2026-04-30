use crate::events::TestEvent;
use crate::reporter::TestResult;
use crate::test::Test;
use saluki_error::{generic_error, GenericError};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// The registry of all tests available to run. Rust tests are added at compile time and configuration-driven tests are
/// added by directory scans.
pub(crate) struct TestRegistry {
    tests: Vec<Box<dyn Test>>,
}

impl TestRegistry {
    pub(crate) fn new() -> Self {
        Self { tests: Vec::new() }
    }

    /// Register a test. Returns an error if the test name is a duplicate.
    pub(crate) fn register(&mut self, test: Box<dyn Test>) -> Result<(), GenericError> {
        // Inefficient but it probably does not matter unless we have a million tests
        if self
            .tests
            .iter()
            .filter(|&existing| existing.name() == test.name())
            .next()
            .is_some()
        {
            return Err(generic_error!(
                "A test named '{}' already exists in the registry",
                test.name()
            ));
        }
        self.tests.push(test);
        Ok(())
    }

    /// Run registered tests sequentially, emitting events for each. When `filter` is provided, only tests for which it
    /// returns true are executed.
    pub(crate) async fn run_tests(
        &self, filter: Option<impl Fn(&dyn Test) -> bool>, event_tx: mpsc::UnboundedSender<TestEvent>,
        cancel_token: CancellationToken,
    ) -> Vec<TestResult> {
        let tests: Vec<_> = self
            .tests
            .iter()
            .filter(|t| filter.as_ref().map_or(true, |f| f(t.as_ref())))
            .collect();

        let _ = event_tx.send(TestEvent::RunStarted {
            total_tests: tests.len(),
        });

        let mut results = Vec::new();

        for test in tests {
            if cancel_token.is_cancelled() {
                break;
            }

            let name = test.name();
            let timeout = test.timeout();
            let _ = event_tx.send(TestEvent::TestStarted { name: name.clone() });

            let result = tokio::select! {
                r = test.run() => r,
                _ = tokio::time::sleep(timeout) => {
                    test.cancel().await;
                    TestResult {
                        name,
                        passed: false,
                        duration: timeout,
                        assertion_results: vec![],
                        error: Some(format!("Test timed out after {:?}.", timeout)),
                        phase_timings: vec![],
                        log_dir: Some(test.log_dir()),
                        assertion_details: vec![],
                    }
                }
            };

            let _ = event_tx.send(TestEvent::TestCompleted { result: result.clone() });
            results.push(result);
        }

        let _ = event_tx.send(TestEvent::AllDone);
        results
    }
}
