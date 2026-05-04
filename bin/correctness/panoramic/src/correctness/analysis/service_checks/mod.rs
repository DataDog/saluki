use std::time::{SystemTime, UNIX_EPOCH};

use saluki_error::{generic_error, GenericError};
use stele::ServiceCheck;
use tracing::{error, info, warn};

use crate::correctness::analysis::collected::CollectedData;

/// Check name prefixes used exclusively by the Core Agent's internal check scheduler.
///
/// These checks fire on a ~15 s flush cycle, so the number of flush cycles that complete
/// before the dump varies under parallel test load. Because millstone generates random names
/// that never start with these prefixes, filtering them out before comparison isolates the
/// user-generated checks without affecting correctness signal.
const AGENT_INTERNAL_PREFIXES: &[&str] = &["datadog."];

fn is_agent_internal(check: &ServiceCheck) -> bool {
    AGENT_INTERNAL_PREFIXES
        .iter()
        .any(|prefix| check.name().starts_with(prefix))
}

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

        // Strip Core Agent internal checks before comparison. Their count is non-deterministic
        // (depends on how many 15 s flush cycles complete before the dump), so including them
        // causes spurious count mismatches under parallel test load.
        let baseline_before = baseline_checks.len();
        let comparison_before = comparison_checks.len();
        baseline_checks.retain(|c| !is_agent_internal(c));
        comparison_checks.retain(|c| !is_agent_internal(c));
        let baseline_filtered = baseline_before - baseline_checks.len();
        let comparison_filtered = comparison_before - comparison_checks.len();
        if baseline_filtered > 0 || comparison_filtered > 0 {
            info!(
                baseline_filtered,
                comparison_filtered, "Filtered agent-internal service checks before comparison."
            );
        }

        // When `d:` is absent, some pipeline stages may backfill the current time. Any timestamp
        // within 5 minutes of now is treated as a fill-in (probability of a lading value landing
        // that close to now: ~0.0000070%) and normalized to u64::MAX so both sides compare equal.
        // Explicit `d:` timestamps are compared exactly.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let five_minutes = 300u64;

        // Use u64::MAX as the sentinel: it is guaranteed never to appear as a real lading
        // timestamp (u32 range tops out at ~4.3B, well below u64::MAX) so it unambiguously
        // marks a normalized fill-in value.
        for check in baseline_checks.iter_mut().chain(comparison_checks.iter_mut()) {
            if let Some(ts) = check.timestamp {
                if ts.abs_diff(now) < five_minutes {
                    check.timestamp = Some(u64::MAX);
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

            // Log the extra checks on whichever side has more to aid debugging.
            let (longer, shorter, longer_label, shorter_label) =
                if self.baseline_checks.len() > self.comparison_checks.len() {
                    (&self.baseline_checks, &self.comparison_checks, "baseline", "comparison")
                } else {
                    (&self.comparison_checks, &self.baseline_checks, "comparison", "baseline")
                };
            let extra_count = longer.len() - shorter.len();
            warn!(
                "{} has {} extra check(s) not present in {}:",
                longer_label, extra_count, shorter_label
            );
            for check in longer.iter().rev().take(extra_count.min(20)) {
                warn!(
                    "  extra check: name={:?} status={} hostname={:?} tags={:?} timestamp={:?}",
                    check.name(),
                    check.status(),
                    check.hostname(),
                    check.tags(),
                    check.timestamp(),
                );
            }

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
