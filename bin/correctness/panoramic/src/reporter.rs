use std::time::Duration;

use colored::Colorize as _;
use serde::Serialize;

use crate::assertions::AssertionResult;

/// Result of a single test case.
#[derive(Clone, Debug, Serialize)]
pub struct TestResult {
    /// Name of the test case.
    pub name: String,
    /// Whether all assertions passed.
    pub passed: bool,
    /// Total duration of the test.
    #[serde(with = "duration_millis")]
    pub duration: Duration,
    /// Results of individual assertions.
    pub assertion_results: Vec<AssertionResult>,
    /// Error message if the test failed to run.
    pub error: Option<String>,
}

/// Result of running all test cases.
#[derive(Clone, Debug, Serialize)]
pub struct TestSuiteResult {
    /// Total number of tests.
    pub total: usize,
    /// Number of passed tests.
    pub passed: usize,
    /// Number of failed tests.
    pub failed: usize,
    /// Total duration of the test suite.
    #[serde(with = "duration_millis")]
    pub duration: Duration,
    /// Individual test results.
    pub results: Vec<TestResult>,
}

impl TestSuiteResult {
    /// Create a test suite result from individual test results.
    pub fn from_results(results: Vec<TestResult>, duration: Duration) -> Self {
        let total = results.len();
        let passed = results.iter().filter(|r| r.passed).count();
        let failed = total - passed;

        Self {
            total,
            passed,
            failed,
            duration,
            results,
        }
    }

    /// Returns true if all tests passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }
}

/// Output format for test results.
#[derive(Clone, Copy, Debug, Default)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

impl OutputFormat {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "text" => Some(OutputFormat::Text),
            "json" => Some(OutputFormat::Json),
            _ => None,
        }
    }
}

/// Reporter for test results.
pub struct Reporter {
    format: OutputFormat,
    verbose: bool,
}

impl Reporter {
    /// Create a new reporter with the given output format.
    pub fn new(format: OutputFormat, verbose: bool) -> Self {
        Self { format, verbose }
    }

    /// Report the result of a single test.
    pub fn report_test_result(&self, result: &TestResult) {
        if matches!(self.format, OutputFormat::Text) {
            let status = if result.passed {
                "PASS".green().bold()
            } else {
                "FAIL".red().bold()
            };

            println!("{} {} ({:.2?})", status, result.name, result.duration);

            // Show error if present
            if let Some(ref error) = result.error {
                println!("  {} {}", "Error:".red(), error);
            }

            // Show assertion results on failure or in verbose mode
            if !result.passed || self.verbose {
                for assertion in &result.assertion_results {
                    let indicator = if assertion.passed { "+".green() } else { "-".red() };

                    println!("  {} {} ({:.2?})", indicator, assertion.name, assertion.duration);
                    println!("    {}", assertion.message);
                }
            }
        }
    }

    /// Report the final test suite result.
    pub fn report_suite_result(&self, suite: &TestSuiteResult) {
        match self.format {
            OutputFormat::Text => {
                println!();
                println!("{}", "=".repeat(60));

                let status = if suite.all_passed() {
                    "PASSED".green().bold()
                } else {
                    "FAILED".red().bold()
                };

                println!(
                    "{}: {} passed, {} failed, {} total ({:.2?})",
                    status, suite.passed, suite.failed, suite.total, suite.duration
                );

                if suite.failed > 0 {
                    println!();
                    println!("{}", "Failed tests:".red().bold());
                    for result in &suite.results {
                        if !result.passed {
                            println!("  - {}", result.name);
                        }
                    }
                }
            }
            OutputFormat::Json => {
                if let Ok(json) = serde_json::to_string_pretty(suite) {
                    println!("{}", json);
                }
            }
        }
    }
}

/// Serde helper for serializing Duration as milliseconds.
mod duration_millis {
    use serde::{Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        duration.as_millis().serialize(serializer)
    }
}

/// Serde implementation for AssertionResult.
impl Serialize for AssertionResult {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("AssertionResult", 4)?;
        state.serialize_field("name", &self.name)?;
        state.serialize_field("passed", &self.passed)?;
        state.serialize_field("message", &self.message)?;
        state.serialize_field("duration_ms", &self.duration.as_millis())?;
        state.end()
    }
}
