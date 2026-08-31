use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use stele::{Metric, MetricValue};
use tracing::trace;

use crate::assertions::{Assertion, AssertionContext, AssertionResult};
use crate::config::MetricTypeMatcher;

const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Assertion that polls the intake sidecar for a metric matching the configured criteria.
pub struct IntakeHasMetricAssertion {
    name: String,
    mtype: Option<MetricTypeMatcher>,
    value: Option<f64>,
    tags: Vec<String>,
    timeout: Duration,
}

impl IntakeHasMetricAssertion {
    pub fn new(
        name: String, mtype: Option<MetricTypeMatcher>, value: Option<f64>, tags: Vec<String>, timeout: Duration,
    ) -> Self {
        Self {
            name,
            mtype,
            value,
            tags,
            timeout,
        }
    }

    /// Returns whether `metric` satisfies every configured criterion.
    fn matches(&self, metric: &Metric) -> bool {
        if metric.context().name() != self.name {
            return false;
        }

        let present_tags = metric.context().tags();
        if !self.tags.iter().all(|tag| present_tags.iter().any(|t| t == tag)) {
            return false;
        }

        if self.mtype.is_none() && self.value.is_none() {
            return true;
        }

        // Type and value must be satisfied by the same value entry, otherwise a metric carrying
        // several values could match a type from one entry and a value from another.
        metric.values().iter().any(|(_, observed)| self.value_matches(observed))
    }

    fn value_matches(&self, observed: &MetricValue) -> bool {
        if let Some(mtype) = self.mtype {
            let observed_type = match observed {
                MetricValue::Count { .. } => MetricTypeMatcher::Count,
                MetricValue::Rate { .. } => MetricTypeMatcher::Rate,
                MetricValue::Gauge { .. } => MetricTypeMatcher::Gauge,
                MetricValue::Sketch { .. } => MetricTypeMatcher::Sketch,
            };
            if observed_type != mtype {
                return false;
            }
        }

        match self.value {
            // Compare through `MetricValue` so float comparison stays owned by stele.
            Some(expected) => match observed {
                MetricValue::Count { .. } => *observed == MetricValue::Count { value: expected },
                MetricValue::Gauge { .. } => *observed == MetricValue::Gauge { value: expected },
                MetricValue::Rate { interval, .. } => {
                    *observed
                        == MetricValue::Rate {
                            interval: *interval,
                            value: expected,
                        }
                }
                MetricValue::Sketch { .. } => false,
            },
            None => true,
        }
    }

    fn criteria(&self) -> String {
        let mut parts = vec![format!("name={}", self.name)];
        if let Some(mtype) = self.mtype {
            parts.push(format!("mtype={:?}", mtype).to_lowercase());
        }
        if let Some(value) = self.value {
            parts.push(format!("value={}", value));
        }
        if !self.tags.is_empty() {
            parts.push(format!("tags=[{}]", self.tags.join(", ")));
        }
        parts.join(", ")
    }
}

#[async_trait::async_trait]
impl Assertion for IntakeHasMetricAssertion {
    fn name(&self) -> &'static str {
        "intake_has_metric"
    }

    fn description(&self) -> String {
        format!("Intake received a metric matching {}.", self.criteria())
    }

    async fn check(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let Some(intake_port) = ctx.intake_host_port else {
            return AssertionResult {
                name: self.name().to_string(),
                passed: false,
                message: "No intake sidecar for this test; set `intake.enabled: true`.".to_string(),
                duration: started.elapsed(),
            };
        };

        let endpoint = format!("http://localhost:{}/metrics/dump", intake_port);
        let client = reqwest::Client::new();
        let deadline = started + self.timeout;
        let mut observed_names = BTreeSet::new();
        let mut last_error = None;

        loop {
            match client.get(&endpoint).send().await {
                Ok(response) => match response.json::<Vec<Metric>>().await {
                    Ok(metrics) => {
                        if metrics.iter().any(|metric| self.matches(metric)) {
                            return AssertionResult {
                                name: self.name().to_string(),
                                passed: true,
                                message: self.description(),
                                duration: started.elapsed(),
                            };
                        }
                        observed_names.clear();
                        observed_names.extend(metrics.iter().map(|m| m.context().name().to_string()));
                    }
                    Err(error) => last_error = Some(format!("failed to decode metrics dump: {}", error)),
                },
                Err(error) => last_error = Some(format!("failed to query metrics dump: {}", error)),
            }

            trace!(endpoint = %endpoint, "Intake metric not present yet.");

            if Instant::now() >= deadline || ctx.cancel_token.is_cancelled() {
                let mut message = format!("No metric matching {} arrived at the intake.", self.criteria());
                if !observed_names.is_empty() {
                    message.push_str(&format!(
                        " Observed metric names: {}.",
                        observed_names.into_iter().collect::<Vec<_>>().join(", ")
                    ));
                }
                if let Some(error) = last_error {
                    message.push_str(&format!(" Last intake error: {}.", error));
                }
                return AssertionResult {
                    name: self.name().to_string(),
                    passed: false,
                    message,
                    duration: started.elapsed(),
                };
            }

            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                _ = ctx.cancel_token.cancelled() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use stele::{Metric, MetricValue};

    use super::IntakeHasMetricAssertion;
    use crate::config::MetricTypeMatcher;

    fn metric(name: &str, tags: &[&str], value: MetricValue) -> Metric {
        let json = serde_json::json!({
            "context": {
                "name": name,
                "tags": tags,
            },
            "values": [[1_u64, value]],
        });

        serde_json::from_value(json).expect("metric should deserialize")
    }

    fn assertion(
        name: &str, mtype: Option<MetricTypeMatcher>, value: Option<f64>, tags: &[&str],
    ) -> IntakeHasMetricAssertion {
        IntakeHasMetricAssertion::new(
            name.to_string(),
            mtype,
            value,
            tags.iter().map(|t| t.to_string()).collect(),
            Duration::from_secs(1),
        )
    }

    #[test]
    fn name_only_criteria_matches_on_name_alone() {
        let observed = metric("some.counter", &["env:test"], MetricValue::Count { value: 3.0 });

        assert!(assertion("some.counter", None, None, &[]).matches(&observed));
        assert!(!assertion("other.counter", None, None, &[]).matches(&observed));
    }

    #[test]
    fn metric_type_and_value_must_both_hold() {
        let observed = metric("some.counter", &[], MetricValue::Count { value: 3.0 });

        assert!(assertion("some.counter", Some(MetricTypeMatcher::Count), Some(3.0), &[]).matches(&observed));
        assert!(!assertion("some.counter", Some(MetricTypeMatcher::Gauge), Some(3.0), &[]).matches(&observed));
        assert!(!assertion("some.counter", Some(MetricTypeMatcher::Count), Some(4.0), &[]).matches(&observed));
    }

    #[test]
    fn tags_are_matched_as_a_subset() {
        let observed = metric(
            "some.counter",
            &["source:test", "host:example"],
            MetricValue::Count { value: 3.0 },
        );

        assert!(assertion("some.counter", None, None, &["source:test"]).matches(&observed));
        assert!(assertion("some.counter", None, None, &["source:test", "host:example"]).matches(&observed));
        assert!(!assertion("some.counter", None, None, &["source:other"]).matches(&observed));
    }
}
