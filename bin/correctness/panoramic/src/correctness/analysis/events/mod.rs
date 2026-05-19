use std::time::{SystemTime, UNIX_EPOCH};

use saluki_error::{generic_error, GenericError};
use stele::Event;
use tracing::{error, info};

use crate::correctness::analysis::collected::CollectedData;

/// Analyzes events for correctness.
pub struct EventsAnalyzer {
    baseline_events: Vec<Event>,
    comparison_events: Vec<Event>,
}

impl EventsAnalyzer {
    /// Creates a new `EventsAnalyzer` instance with the given baseline/comparison data.
    pub fn new(baseline_data: &CollectedData, comparison_data: &CollectedData) -> Self {
        let mut baseline_events = baseline_data.events().to_vec();
        let mut comparison_events = comparison_data.events().to_vec();

        // Lading generates timestamps as random u32 values across 1970–2106. When `d:` is absent,
        // both pipelines backfill the current time, but start at slightly different moments so the
        // values diverge by a few seconds. Any timestamp within 5 minutes of now is treated as a
        // fill-in (probability of a lading value landing that close to now: ~0.0000070%) and
        // normalized to i64::MAX so both sides compare equal. Explicit `d:` timestamps are
        // compared exactly.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let five_minutes = 300i64;

        // Use i64::MAX as the sentinel: it is guaranteed never to appear as a real lading
        // timestamp (u32 range tops out at ~4.3B, well below i64::MAX) so it unambiguously
        // marks a normalized fill-in value.
        for event in baseline_events.iter_mut().chain(comparison_events.iter_mut()) {
            if (event.timestamp - now).abs() < five_minutes {
                event.timestamp = i64::MAX;
            }
        }

        baseline_events.sort_unstable();
        comparison_events.sort_unstable();

        Self {
            baseline_events,
            comparison_events,
        }
    }

    /// Analyzes events from both the baseline and comparison targets, comparing them to one another.
    ///
    /// # Errors
    ///
    /// If analysis fails or a difference is found, an error is returned with details.
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
        info!(
            "Analyzing {} events from baseline target, and {} events from comparison target.",
            self.baseline_events.len(),
            self.comparison_events.len(),
        );

        if self.baseline_events.len() != self.comparison_events.len() {
            let msg = format!(
                "Event count mismatch: baseline has {} events, comparison has {} events.",
                self.baseline_events.len(),
                self.comparison_events.len(),
            );
            error!("{}", msg);
            return Err((generic_error!("{}", msg), vec![msg]));
        }

        if self.baseline_events.is_empty() {
            return Err((
                generic_error!("No events were captured from either target. At least one event is required."),
                vec![],
            ));
        }

        info!(
            "Both targets emitted {} events. Comparing event contents...",
            self.baseline_events.len()
        );

        let mut mismatches: Vec<String> = Vec::new();

        for (baseline_event, comparison_event) in self.baseline_events.iter().zip(self.comparison_events.iter()) {
            if baseline_event != comparison_event {
                let detail = format!(
                    "Event mismatch:\n    baseline:   title={:?} text={:?} alert_type={:?} priority={:?} aggregation_key={:?} hostname={:?} source_type_name={:?} tags={:?} timestamp={}\n    comparison: title={:?} text={:?} alert_type={:?} priority={:?} aggregation_key={:?} hostname={:?} source_type_name={:?} tags={:?} timestamp={}",
                    baseline_event.title(),
                    baseline_event.text(),
                    baseline_event.alert_type(),
                    baseline_event.priority(),
                    baseline_event.aggregation_key(),
                    baseline_event.hostname(),
                    baseline_event.source_type_name(),
                    baseline_event.tags(),
                    baseline_event.timestamp(),
                    comparison_event.title(),
                    comparison_event.text(),
                    comparison_event.alert_type(),
                    comparison_event.priority(),
                    comparison_event.aggregation_key(),
                    comparison_event.hostname(),
                    comparison_event.source_type_name(),
                    comparison_event.tags(),
                    comparison_event.timestamp(),
                );
                error!("{}", detail);
                mismatches.push(detail);
            }
        }

        if mismatches.is_empty() {
            info!(
                "All {} events matched between baseline and comparison.",
                self.baseline_events.len()
            );
            Ok(())
        } else {
            Err((
                generic_error!(
                    "{} event(s) did not match between baseline and comparison.",
                    mismatches.len()
                ),
                mismatches,
            ))
        }
    }
}
