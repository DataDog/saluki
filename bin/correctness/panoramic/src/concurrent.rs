use std::{path::PathBuf, sync::Arc};

use futures::stream::{self, StreamExt as _};
use tokio::sync::Semaphore;

use crate::{config::TestCase, reporter::TestResult, runner::TestRunner};

/// Runner that executes multiple test cases concurrently.
pub struct ConcurrentRunner {
    test_cases: Vec<TestCase>,
    parallelism: usize,
    log_dir: Option<PathBuf>,
}

impl ConcurrentRunner {
    /// Create a new concurrent runner with the given test cases and parallelism limit.
    pub fn new(test_cases: Vec<TestCase>, parallelism: usize) -> Self {
        Self {
            test_cases,
            parallelism: parallelism.max(1),
            log_dir: None,
        }
    }

    /// Set the directory where container logs should be written.
    pub fn with_log_dir(mut self, log_dir: PathBuf) -> Self {
        self.log_dir = Some(log_dir);
        self
    }

    /// Run all test cases and return the results.
    pub async fn run_all(&self) -> Vec<TestResult> {
        let semaphore = Arc::new(Semaphore::new(self.parallelism));
        let log_dir = self.log_dir.clone();

        let results: Vec<TestResult> = stream::iter(self.test_cases.iter().cloned())
            .map(|test_case| {
                let semaphore = semaphore.clone();
                let log_dir = log_dir.clone();

                async move {
                    let _permit = semaphore.acquire().await.unwrap();
                    let mut runner = TestRunner::new(test_case);
                    if let Some(dir) = log_dir {
                        runner = runner.with_log_dir(dir);
                    }
                    runner.run().await
                }
            })
            .buffer_unordered(self.parallelism)
            .collect()
            .await;

        results
    }

    /// Run all test cases, stopping at the first failure.
    pub async fn run_fail_fast(&self) -> Vec<TestResult> {
        let semaphore = Arc::new(Semaphore::new(self.parallelism));
        let mut results = Vec::new();

        for test_case in &self.test_cases {
            let _permit = semaphore.acquire().await.unwrap();
            let mut runner = TestRunner::new(test_case.clone());
            if let Some(ref dir) = self.log_dir {
                runner = runner.with_log_dir(dir.clone());
            }
            let result = runner.run().await;

            let failed = !result.passed;
            results.push(result);

            if failed {
                break;
            }
        }

        results
    }
}
