//! The ADP telemetry scraper: reads ADP's `/metrics` every second and keeps short histories of the series the UI shows.
//!
//! Counters are cumulative, so the scraper turns each counter into a per-second rate between consecutive scrapes, and
//! reports the average of the last [`RATE_SCRAPES`] of those: payloads leave ADP every couple of seconds, so a single
//! scrape-to-scrape rate alternates between zero and twice the average. The series kept are the plan-level series
//! (`adp__plan_*`, such as `plan_node_events_total{component_id, port}`, the per-node counts the plan-driven topology
//! derives), and the per-component event counters of the native telemetry (`adp__component_events_*`,
//! `adp__component_errors_total`), which also cover the encoders and forwarders.
//!
//! A series that stops appearing, including every series while ADP does not answer, is dropped
//! [`SERIES_RETENTION_MS`] after it was last seen, so that rates never outlive ADP.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use serde::Serialize;

use crate::{adp_api::AdpApi, clock::unix_millis};

pub mod prometheus;
use self::prometheus::Exposition;

/// The name prefixes of the series the scraper keeps.
pub const TRACKED_PREFIXES: &[&str] = &["adp__plan_", "adp__component_events_", "adp__component_errors_total"];

/// The prefix ADP's internal metric names carry in the exposition.
const ADP_PREFIX: &str = "adp__";

/// How many scrapes of history each series keeps.
pub const HISTORY_LEN: usize = 30;

/// How many of the latest scrape-to-scrape rates a series' reported rate averages.
pub const RATE_SCRAPES: usize = 5;

/// How long a series that stopped appearing in scrapes is still reported, in milliseconds.
pub const SERIES_RETENTION_MS: u64 = 10_000;

/// How often the scraper reads `/metrics`.
pub const SCRAPE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct SeriesKey {
    name: String,
    labels: Vec<(String, String)>,
}

#[derive(Clone, Debug)]
struct SeriesHistory {
    counter: bool,
    last: (u64, f64),
    last_seen_ms: u64,
    rates: VecDeque<f64>,
}

#[derive(Default)]
struct ScrapeState {
    attempts: u64,
    last_scrape_ms: Option<u64>,
    last_success_ms: Option<u64>,
    /// When the current run of successful scrapes began.
    up_since_ms: Option<u64>,
    error: Option<String>,
    skipped_lines: usize,
    series: BTreeMap<SeriesKey, SeriesHistory>,
}

impl ScrapeState {
    fn expire(&mut self, now_ms: u64) {
        self.series
            .retain(|_, history| history.last_seen_ms + SERIES_RETENTION_MS >= now_ms);
    }
}

/// One series, as the UI shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SeriesEntry {
    /// The metric name, without ADP's `adp__` prefix.
    pub name: String,

    /// The labels, sorted by name.
    pub labels: BTreeMap<String, String>,

    /// Whether the series is a counter, whose rate is reported.
    pub counter: bool,

    /// The latest value: the cumulative total of a counter, or a gauge's value.
    pub value: f64,

    /// The rate of a counter, per second, averaged over its last [`RATE_SCRAPES`] scrape-to-scrape rates; zero for
    /// other series.
    pub rate: f64,

    /// The rates of a counter between consecutive scrapes, oldest first; empty for other series.
    pub history: Vec<f64>,
}

/// What the scraper knows, as the UI shows it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TelemetrySnapshot {
    /// `waiting` before the first scrape, `ok` when the last scrape succeeded, and `unreachable` when it failed.
    pub state: &'static str,

    /// When the last scrape was attempted.
    pub last_scrape_ms: Option<u64>,

    /// When the last scrape succeeded.
    pub last_success_ms: Option<u64>,

    /// Since when every scrape has succeeded, while the last one did.
    pub up_since_ms: Option<u64>,

    /// Why the last scrape failed.
    pub error: Option<String>,

    /// How many lines of the last exposition could not be read.
    pub skipped_lines: usize,

    /// The plan-level series (`plan_*`), other than per-node counts.
    pub plan: Vec<SeriesEntry>,

    /// The per-node counts derived by the plan-driven topology (`plan_node_events_total`).
    pub nodes: Vec<SeriesEntry>,

    /// The native per-component event and error counters.
    pub components: Vec<SeriesEntry>,
}

/// The telemetry scraper's record.
///
/// Cheap to clone: clones share the record.
#[derive(Clone, Default)]
pub struct TelemetryScraper {
    state: Arc<Mutex<ScrapeState>>,
}

impl TelemetryScraper {
    /// Creates an empty record.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a successful scrape taken at `at_ms`.
    pub fn observe(&self, at_ms: u64, exposition: &Exposition) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.error.is_some() || state.up_since_ms.is_none() {
            state.up_since_ms = Some(at_ms);
        }
        state.attempts += 1;
        state.last_scrape_ms = Some(at_ms);
        state.last_success_ms = Some(at_ms);
        state.error = None;
        state.skipped_lines = exposition.skipped.len();

        for sample in &exposition.samples {
            if !TRACKED_PREFIXES.iter().any(|prefix| sample.name.starts_with(prefix)) {
                continue;
            }
            let counter = exposition.type_of(&sample.name) == Some("counter");
            let key = SeriesKey {
                name: sample.name.clone(),
                labels: sample.labels.clone(),
            };
            match state.series.get_mut(&key) {
                Some(history) => {
                    let (previous_ms, previous_value) = history.last;
                    if counter && at_ms > previous_ms {
                        let elapsed = (at_ms - previous_ms) as f64 / 1000.0;
                        // A counter that went down was reset (ADP restarted): count from zero.
                        let increase = if sample.value >= previous_value {
                            sample.value - previous_value
                        } else {
                            sample.value
                        };
                        if history.rates.len() == HISTORY_LEN {
                            history.rates.pop_front();
                        }
                        history.rates.push_back(increase / elapsed);
                    }
                    history.counter = counter;
                    history.last = (at_ms, sample.value);
                    history.last_seen_ms = at_ms;
                }
                None => {
                    state.series.insert(
                        key,
                        SeriesHistory {
                            counter,
                            last: (at_ms, sample.value),
                            last_seen_ms: at_ms,
                            rates: VecDeque::new(),
                        },
                    );
                }
            }
        }
        state.expire(at_ms);
    }

    /// Records a failed scrape attempted at `at_ms`.
    pub fn observe_failure(&self, at_ms: u64, error: String) {
        let mut state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.attempts += 1;
        state.last_scrape_ms = Some(at_ms);
        state.error = Some(error);
        state.expire(at_ms);
    }

    /// Returns what the scraper knows at `now_ms`: series not seen within [`SERIES_RETENTION_MS`] are left out.
    pub fn snapshot(&self, now_ms: u64) -> TelemetrySnapshot {
        let state = self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let ok = state.attempts > 0 && state.error.is_none();
        let mut snapshot = TelemetrySnapshot {
            state: match (state.attempts, &state.error) {
                (0, _) => "waiting",
                (_, Some(_)) => "unreachable",
                (_, None) => "ok",
            },
            last_scrape_ms: state.last_scrape_ms,
            last_success_ms: state.last_success_ms,
            up_since_ms: state.up_since_ms.filter(|_| ok),
            error: state.error.clone(),
            skipped_lines: state.skipped_lines,
            plan: Vec::new(),
            nodes: Vec::new(),
            components: Vec::new(),
        };
        let current = state
            .series
            .iter()
            .filter(|(_, history)| history.last_seen_ms + SERIES_RETENTION_MS >= now_ms);
        for (key, history) in current {
            let name = key.name.strip_prefix(ADP_PREFIX).unwrap_or(&key.name).to_string();
            let recent = history
                .rates
                .iter()
                .rev()
                .take(RATE_SCRAPES)
                .copied()
                .collect::<Vec<_>>();
            let entry = SeriesEntry {
                labels: key.labels.iter().cloned().collect(),
                counter: history.counter,
                value: history.last.1,
                rate: if recent.is_empty() {
                    0.0
                } else {
                    recent.iter().sum::<f64>() / recent.len() as f64
                },
                history: history.rates.iter().copied().collect(),
                name,
            };
            if entry.name == "plan_node_events_total" {
                snapshot.nodes.push(entry);
            } else if entry.name.starts_with("plan_") {
                snapshot.plan.push(entry);
            } else {
                snapshot.components.push(entry);
            }
        }
        snapshot
    }

    /// Scrapes ADP's `/metrics` every [`SCRAPE_INTERVAL`] until the task is dropped.
    pub async fn run(self, api: AdpApi) {
        let mut ticks = tokio::time::interval(SCRAPE_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            match api.metrics_text().await {
                Ok(text) => self.observe(unix_millis(), &prometheus::parse(&text)),
                Err(error) => self.observe_failure(unix_millis(), error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exposition(text: &str) -> Exposition {
        prometheus::parse(text)
    }

    #[test]
    fn counters_become_rates_between_scrapes_and_resets_count_from_zero() {
        let scraper = TelemetryScraper::new();
        let header = "# TYPE adp__plan_node_events_total counter\n# TYPE adp__plan_active_generation gauge\n";
        let scrape = |value: u64, generation: u64| {
            exposition(&format!(
                "{}adp__plan_node_events_total{{component_id=\"metrics_policy\",port=\"0\"}} {}\n\
                 adp__plan_active_generation {}\nadp__unrelated_total 5\n",
                header, value, generation
            ))
        };

        assert_eq!(scraper.snapshot(0).state, "waiting");
        scraper.observe(10_000, &scrape(100, 1));
        scraper.observe(12_000, &scrape(160, 1));
        scraper.observe(13_000, &scrape(170, 2));
        // ADP restarted: the counter starts again from zero.
        scraper.observe(14_000, &scrape(4, 1));

        let snapshot = scraper.snapshot(14_000);
        assert_eq!(snapshot.state, "ok");
        assert_eq!(snapshot.up_since_ms, Some(10_000));
        assert_eq!(snapshot.nodes.len(), 1);
        let node = &snapshot.nodes[0];
        assert_eq!(
            node.labels.get("component_id").map(String::as_str),
            Some("metrics_policy")
        );
        assert_eq!(node.labels.get("port").map(String::as_str), Some("0"));
        // (160 - 100) / 2 s, (170 - 160) / 1 s, then 4 / 1 s after the reset.
        assert_eq!(node.history, vec![30.0, 10.0, 4.0]);
        // The reported rate averages the last scrape-to-scrape rates.
        assert_eq!(node.rate, (30.0 + 10.0 + 4.0) / 3.0);
        assert_eq!(node.value, 4.0);

        assert_eq!(snapshot.plan.len(), 1);
        assert_eq!(snapshot.plan[0].name, "plan_active_generation");
        assert!(!snapshot.plan[0].counter);
        assert_eq!(snapshot.plan[0].value, 1.0);
        assert!(snapshot.plan[0].history.is_empty());
        // Untracked series are not kept.
        assert!(snapshot.components.is_empty());

        scraper.observe_failure(15_000, "connection refused".to_string());
        let snapshot = scraper.snapshot(15_000);
        assert_eq!(snapshot.state, "unreachable");
        assert_eq!(snapshot.last_success_ms, Some(14_000));
        assert_eq!(snapshot.up_since_ms, None);
        assert_eq!(snapshot.nodes[0].value, 4.0);

        // While ADP does not answer, its series expire as if they had stopped appearing: rates never outlive ADP.
        scraper.observe_failure(14_000 + SERIES_RETENTION_MS + 1, "connection refused".to_string());
        let snapshot = scraper.snapshot(14_000 + SERIES_RETENTION_MS + 1);
        assert!(snapshot.nodes.is_empty() && snapshot.plan.is_empty());
        // Even with no scrape at all, a snapshot leaves out series older than the retention.
        scraper.observe(30_000, &scrape(10, 1));
        assert_eq!(scraper.snapshot(30_000 + SERIES_RETENTION_MS).nodes.len(), 1);
        assert!(scraper.snapshot(30_000 + SERIES_RETENTION_MS + 1).nodes.is_empty());
        assert_eq!(scraper.snapshot(30_000).up_since_ms, Some(30_000));
    }

    #[test]
    fn rate_history_is_bounded_and_vanished_series_expire() {
        let scraper = TelemetryScraper::new();
        let text = |value: u64, with_sent: bool| {
            let mut text = format!(
                "# TYPE adp__component_events_received_total counter\n\
                 adp__component_events_received_total{{component_id=\"dsd_agg\",component_type=\"transform\"}} {}\n",
                value
            );
            if with_sent {
                text.push_str(
                    "# TYPE adp__component_events_sent_total counter\n\
                     adp__component_events_sent_total{component_id=\"dsd_agg\",output=\"_default\"} 1\n",
                );
            }
            text
        };
        let last = HISTORY_LEN as u64 + 5;
        for i in 0..last {
            scraper.observe(i * 1000, &exposition(&text(i * 3, i == 0)));
        }
        let snapshot = scraper.snapshot((last - 1) * 1000);
        assert_eq!(snapshot.components.len(), 1, "{:?}", snapshot.components);
        let received = &snapshot.components[0];
        assert_eq!(received.name, "component_events_received_total");
        assert_eq!(received.history, vec![3.0; HISTORY_LEN]);
    }

    #[test]
    fn rates_of_bursty_counters_are_averaged_over_several_scrapes() {
        let scraper = TelemetryScraper::new();
        // A payload every two seconds: the counter moves by 56 on every other scrape.
        let mut total = 0;
        for i in 0..10u64 {
            if i % 2 == 1 {
                total += 56;
            }
            let text = format!(
                "# TYPE adp__component_events_sent_total counter\n\
                 adp__component_events_sent_total{{component_id=\"dd_out\"}} {}\n",
                total
            );
            scraper.observe(i * 1000, &exposition(&text));
        }
        let snapshot = scraper.snapshot(9_000);
        let sent = &snapshot.components[0];
        assert_eq!(sent.history.last(), Some(&56.0));
        // The last five rates are 56, 0, 56, 0, 56.
        assert_eq!(sent.rate, 56.0 * 3.0 / 5.0);
    }
}
