use saluki_error::{generic_error, GenericError};
use stele::StatefulLog;
use tracing::{error, info};

use crate::correctness::analysis::collected::CollectedData;

/// Analyzes decoded stateful logs for correctness.
pub struct LogsAnalyzer {
    baseline_logs: Vec<StatefulLog>,
    comparison_logs: Vec<StatefulLog>,
}

impl LogsAnalyzer {
    /// Creates a new `LogsAnalyzer` instance with the given baseline/comparison data.
    pub fn new(baseline_data: &CollectedData, comparison_data: &CollectedData) -> Self {
        let mut baseline_logs = baseline_data.logs().to_vec();
        let mut comparison_logs = comparison_data.logs().to_vec();

        baseline_logs.sort_unstable();
        comparison_logs.sort_unstable();

        Self {
            baseline_logs,
            comparison_logs,
        }
    }

    /// Analyzes decoded stateful logs from both targets, comparing them to one another.
    ///
    /// # Errors
    ///
    /// If analysis fails or a difference is found, an error is returned with details.
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
        info!(
            "Analyzing {} stateful logs from baseline target, and {} stateful logs from comparison target.",
            self.baseline_logs.len(),
            self.comparison_logs.len(),
        );

        if self.baseline_logs.len() != self.comparison_logs.len() {
            let msg = format!(
                "Stateful log count mismatch: baseline has {} logs, comparison has {} logs.",
                self.baseline_logs.len(),
                self.comparison_logs.len(),
            );
            error!("{}", msg);
            return Err((generic_error!("{}", msg), vec![msg]));
        }

        if self.baseline_logs.is_empty() {
            return Err((
                generic_error!("No stateful logs were captured from either target. At least one log is required."),
                vec![],
            ));
        }

        let mut mismatches = Vec::new();
        for (baseline_log, comparison_log) in self.baseline_logs.iter().zip(self.comparison_logs.iter()) {
            if baseline_log != comparison_log {
                let detail = format!(
                    "Stateful log mismatch:\n    baseline:   message={:?} status={:?} service={:?} tags={:?} timestamp={} uuid={:?}\n    comparison: message={:?} status={:?} service={:?} tags={:?} timestamp={} uuid={:?}",
                    baseline_log.message(),
                    baseline_log.status(),
                    baseline_log.service(),
                    baseline_log.tags(),
                    baseline_log.timestamp(),
                    baseline_log.uuid(),
                    comparison_log.message(),
                    comparison_log.status(),
                    comparison_log.service(),
                    comparison_log.tags(),
                    comparison_log.timestamp(),
                    comparison_log.uuid(),
                );
                error!("{}", detail);
                mismatches.push(detail);
            }
        }

        if mismatches.is_empty() {
            info!(
                "All {} stateful logs matched between baseline and comparison.",
                self.baseline_logs.len()
            );
            Ok(())
        } else {
            Err((
                generic_error!(
                    "{} stateful log(s) did not match between baseline and comparison.",
                    mismatches.len()
                ),
                mismatches,
            ))
        }
    }
}
