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

        // !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
        // TIMESTAMP NORMALIZATION — READ BEFORE CHANGING
        //
        // Lading generates event timestamps as random u32 values sampled uniformly across the
        // full 0–4,294,967,295 range (year 1970 to 2106). When a DogStatsD event carries an
        // explicit `d:` field, both the stock agent and ADP preserve that value exactly, so the
        // two sides will agree.
        //
        // When `d:` is absent (~50% of lading events), the stock agent backfills `time.Now()`
        // (pkg/aggregator/aggregator.go, addEvent) while ADP currently stores 0 (see issue
        // #1528). After #1528 is fixed both sides will backfill `time.Now()`, but the two
        // pipelines start at slightly different moments so the values will still differ by a few
        // seconds.
        //
        // The probability that any single lading-generated random u32 lands within one hour of
        // the current wall-clock time is approximately 3600 / 4,294,967,296 ≈ 0.000084%. In
        // practice this never happens in a test run. Therefore: any timestamp within one hour of
        // now is almost certainly a pipeline fill-in value, not an explicit `d:` timestamp from
        // the test corpus. We zero those out on both sides before sorting and comparing, so that
        // fill-in values from both pipelines compare equal regardless of the exact wall-clock
        // time each pipeline happened to run.
        // !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let one_hour = 3600i64;

        for event in baseline_events.iter_mut().chain(comparison_events.iter_mut()) {
            if (event.timestamp - now).abs() < one_hour {
                event.timestamp = 0;
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
