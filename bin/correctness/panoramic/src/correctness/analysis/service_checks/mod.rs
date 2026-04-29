use std::time::{SystemTime, UNIX_EPOCH};

use saluki_error::{generic_error, GenericError};
use stele::ServiceCheck;
use tracing::{error, info};

use crate::correctness::analysis::collected::CollectedData;

/// Analyzes service checks for correctness.
pub struct ServiceChecksAnalyzer {
    baseline_checks: Vec<ServiceCheck>,
    comparison_checks: Vec<ServiceCheck>,
}

impl ServiceChecksAnalyzer {
    /// Creates a new `ServiceChecksAnalyzer` with the given baseline/comparison data.
    pub fn new(baseline_data: &CollectedData, comparison_data: &CollectedData) -> Self {
        let mut baseline_checks = baseline_data.service_checks().to_vec();
        let mut comparison_checks = comparison_data.service_checks().to_vec();

        // When `d:` is absent, some pipeline stages may backfill the current time. Any timestamp
        // within 5 minutes of now is treated as a fill-in (probability of a lading value landing
        // that close to now: ~0.0000070%) and normalized to None so both sides compare equal.
        // Explicit `d:` timestamps are compared exactly.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let five_minutes = 300u64;

        for check in baseline_checks.iter_mut().chain(comparison_checks.iter_mut()) {
            if let Some(ts) = check.timestamp {
                if ts.abs_diff(now) < five_minutes {
                    check.timestamp = None;
                }
            }
        }

        baseline_checks.sort_unstable();
        comparison_checks.sort_unstable();

        Self {
            baseline_checks,
            comparison_checks,
        }
    }

    /// Analyzes service checks from both the baseline and comparison targets, comparing them.
    ///
    /// # Errors
    ///
    /// If analysis fails or a difference is found, an error is returned with details.
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
        info!(
            "Analyzing {} service checks from baseline target, and {} service checks from comparison target.",
            self.baseline_checks.len(),
            self.comparison_checks.len(),
        );

        if self.baseline_checks.len() != self.comparison_checks.len() {
            let msg = format!(
                "Service check count mismatch: baseline has {} checks, comparison has {} checks.",
                self.baseline_checks.len(),
                self.comparison_checks.len(),
            );
            error!("{}", msg);
            return Err((generic_error!("{}", msg), vec![msg]));
        }

        if self.baseline_checks.is_empty() {
            return Err((
                generic_error!(
                    "No service checks were captured from either target. At least one service check is required."
                ),
                vec![],
            ));
        }

        info!(
            "Both targets emitted {} service checks. Comparing contents...",
            self.baseline_checks.len()
        );

        let mut mismatches: Vec<String> = Vec::new();

        for (baseline_check, comparison_check) in self.baseline_checks.iter().zip(self.comparison_checks.iter()) {
            if baseline_check != comparison_check {
                let detail = format!(
                    "Service check mismatch:\n    baseline:   name={:?} status={} hostname={:?} message={:?} tags={:?} timestamp={:?}\n    comparison: name={:?} status={} hostname={:?} message={:?} tags={:?} timestamp={:?}",
                    baseline_check.name(),
                    baseline_check.status(),
                    baseline_check.hostname(),
                    baseline_check.message(),
                    baseline_check.tags(),
                    baseline_check.timestamp(),
                    comparison_check.name(),
                    comparison_check.status(),
                    comparison_check.hostname(),
                    comparison_check.message(),
                    comparison_check.tags(),
                    comparison_check.timestamp(),
                );
                error!("{}", detail);
                mismatches.push(detail);
            }
        }

        if mismatches.is_empty() {
            info!(
                "All {} service checks matched between baseline and comparison.",
                self.baseline_checks.len()
            );
            Ok(())
        } else {
            Err((
                generic_error!(
                    "{} service check(s) did not match between baseline and comparison.",
                    mismatches.len()
                ),
                mismatches,
            ))
        }
    }
}
