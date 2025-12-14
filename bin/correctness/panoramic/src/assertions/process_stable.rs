use std::time::{Duration, Instant};

use crate::assertions::{Assertion, AssertionContext, AssertionResult};

/// Assertion that checks the container process remains stable for a specified duration.
pub struct ProcessStableForAssertion {
    duration: Duration,
}

impl ProcessStableForAssertion {
    pub fn new(duration: Duration) -> Self {
        Self { duration }
    }
}

#[async_trait::async_trait]
impl Assertion for ProcessStableForAssertion {
    fn name(&self) -> &'static str {
        "process_stable_for"
    }

    fn description(&self) -> String {
        format!("Process remains stable for {:?}.", self.duration)
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();

        tokio::select! {
            // Wait for the stability duration.
            _ = tokio::time::sleep(self.duration) => {
                AssertionResult {
                    name: self.name().to_string(),
                    passed: true,
                    message: format!("Process remained stable for {:?}.", self.duration),
                    duration: started.elapsed(),
                }
            }

            // Handle cancellation (container exited).
            _ = ctx.cancel_token.cancelled() => {
                AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message: format!(
                        "Process exited unexpectedly after {:?} (expected to remain stable for {:?}).",
                        started.elapsed(),
                        self.duration
                    ),
                    duration: started.elapsed(),
                }
            }
        }
    }
}
