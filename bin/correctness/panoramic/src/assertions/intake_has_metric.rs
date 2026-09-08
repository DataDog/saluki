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
    metric_type: Option<MetricTypeMatcher>,
    value: Option<f64>,
    tags: Vec<String>,
    timeout: Duration,
}

impl IntakeHasMetricAssertion {
    pub fn new(
        name: String, metric_type: Option<MetricTypeMatcher>, value: Option<f64>, tags: Vec<String>, timeout: Duration,
    ) -> Self {
        Self {
            name,
            metric_type,
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

        if self.metric_type.is_none() && self.value.is_none() {
            return true;
        }

        // Type and value must be satisfied by the same value entry, otherwise a metric carrying
        // several values could match a type from one entry and a value from another.
        metric.values().iter().any(|(_, observed)| self.value_matches(observed))
    }

    fn value_matches(&self, observed: &MetricValue) -> bool {
        if let Some(metric_type) = self.metric_type {
            let observed_type = match observed {
                MetricValue::Count { .. } => MetricTypeMatcher::Count,
                MetricValue::Rate { .. } => MetricTypeMatcher::Rate,
                MetricValue::Gauge { .. } => MetricTypeMatcher::Gauge,
                MetricValue::Sketch { .. } => MetricTypeMatcher::Sketch,
            };
            if observed_type != metric_type {
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

    /// Builds the failure result for a run that ended without a matching metric.
    ///
    /// `summary` states why we stopped looking; the metric names and intake error seen on the last
    /// poll are appended as diagnostics when we have them.
    fn unmatched_result(
        &self, started: Instant, summary: String, observed_names: BTreeSet<String>, last_error: Option<String>,
    ) -> AssertionResult {
        let mut message = summary;
        if !observed_names.is_empty() {
            message.push_str(&format!(
                " Observed metric names: {}.",
                observed_names.into_iter().collect::<Vec<_>>().join(", ")
            ));
        }
        if let Some(error) = last_error {
            message.push_str(&format!(" Last intake error: {}.", error));
        }

        AssertionResult {
            name: self.name().to_string(),
            passed: false,
            message,
            duration: started.elapsed(),
        }
    }

    fn criteria(&self) -> String {
        let mut parts = vec![format!("name={}", self.name)];
        if let Some(metric_type) = self.metric_type {
            parts.push(format!("metric_type={:?}", metric_type).to_lowercase());
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
            // Read the exit state before polling so a metric that reached the intake just before
            // the target exited still counts; see the post-poll check below.
            let exited = ctx.container_exit_token.is_cancelled();

            if ctx.cancel_token.is_cancelled() {
                return self.unmatched_result(started, "Assertion cancelled.".to_string(), observed_names, last_error);
            }

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

            // Nothing more can reach the intake once the target is gone, so report the crash
            // instead of polling until the timeout.
            if exited {
                let summary = format!(
                    "No metric matching {} arrived at the intake before the target exited.",
                    self.criteria()
                );
                return self.unmatched_result(started, summary, observed_names, last_error);
            }

            if Instant::now() >= deadline {
                let summary = format!(
                    "No metric matching {} arrived at the intake within {:?}.",
                    self.criteria(),
                    self.timeout
                );
                return self.unmatched_result(started, summary, observed_names, last_error);
            }

            tokio::select! {
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
                _ = ctx.cancel_token.cancelled() => {}
                _ = ctx.container_exit_token.cancelled() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::net::TcpListener;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use stele::{Metric, MetricValue};
    use tokio_util::sync::CancellationToken;

    use super::IntakeHasMetricAssertion;
    use crate::assertions::{Assertion as _, AssertionContext, LogBuffer, TargetCommand};
    use crate::config::MetricTypeMatcher;

    fn metric(name: &str, tags: &[&str], value: MetricValue) -> Metric {
        metric_with_values(name, tags, &[value])
    }

    fn metric_with_values(name: &str, tags: &[&str], values: &[MetricValue]) -> Metric {
        let values = values
            .iter()
            .enumerate()
            .map(|(idx, value)| serde_json::json!([idx as u64, value]))
            .collect::<Vec<_>>();
        let json = serde_json::json!({
            "context": {
                "name": name,
                "tags": tags,
            },
            "values": values,
        });

        serde_json::from_value(json).expect("metric should deserialize")
    }

    fn assertion(
        name: &str, metric_type: Option<MetricTypeMatcher>, value: Option<f64>, tags: &[&str],
    ) -> IntakeHasMetricAssertion {
        IntakeHasMetricAssertion::new(
            name.to_string(),
            metric_type,
            value,
            tags.iter().map(|t| t.to_string()).collect(),
            Duration::from_secs(1),
        )
    }

    /// Returns a context pointing at a port with no listener, so every intake poll fails.
    fn context(cancel_token: CancellationToken, container_exit_token: CancellationToken) -> AssertionContext {
        // The assertion builds a reqwest client, which needs the process-wide crypto provider that
        // `main` installs at startup.
        let _ = crate::default_crypto_provider().install_default();

        let listener = TcpListener::bind("127.0.0.1:0").expect("should bind an ephemeral port");
        let closed_port = listener.local_addr().expect("should have a local address").port();
        drop(listener);

        AssertionContext {
            log_buffer: Arc::new(RwLock::new(LogBuffer::default())),
            container_exit_token,
            cancel_token,
            port_mappings: HashMap::new(),
            container_ip: None,
            target_os: None,
            container_name: "intake-has-metric-test".to_string(),
            is_host_process: false,
            host_process_exit_code: None,
            docker_container_exit_code: None,
            intake_host_port: Some(closed_port),
            core_agent_auth_token_path: None,
            adp_cli_command: TargetCommand::new(vec!["panoramic-unused-cli-program".to_string()]),
            core_agent_cli_command: TargetCommand::new(vec!["panoramic-unused-cli-program".to_string()]),
        }
    }

    fn polling_assertion(timeout: Duration) -> IntakeHasMetricAssertion {
        IntakeHasMetricAssertion::new("some.counter".to_string(), None, None, Vec::new(), timeout)
    }

    #[tokio::test]
    async fn target_exit_fails_without_waiting_out_the_timeout() {
        let container_exit_token = CancellationToken::new();
        container_exit_token.cancel();
        let ctx = context(CancellationToken::new(), container_exit_token);

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            polling_assertion(Duration::from_secs(600)).check(&ctx),
        )
        .await
        .expect("assertion should return once the target has exited");

        assert!(!result.passed, "unexpected assertion pass: {}", result.message);
        assert!(
            result.message.contains("before the target exited"),
            "unexpected message: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn missing_intake_port_fails_immediately() {
        let mut ctx = context(CancellationToken::new(), CancellationToken::new());
        ctx.intake_host_port = None;

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            polling_assertion(Duration::from_secs(600)).check(&ctx),
        )
        .await
        .expect("assertion should return without polling when the test has no intake sidecar");

        assert!(!result.passed, "unexpected assertion pass: {}", result.message);
        assert!(
            result.message.contains("intake.enabled"),
            "unexpected message: {}",
            result.message
        );
    }

    #[tokio::test]
    async fn cancellation_and_timeout_are_reported_distinctly() {
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();
        let cancelled = polling_assertion(Duration::from_secs(600))
            .check(&context(cancel_token, CancellationToken::new()))
            .await;

        assert!(!cancelled.passed, "unexpected assertion pass: {}", cancelled.message);
        assert!(
            cancelled.message.starts_with("Assertion cancelled."),
            "unexpected message: {}",
            cancelled.message
        );

        let timed_out = polling_assertion(Duration::ZERO)
            .check(&context(CancellationToken::new(), CancellationToken::new()))
            .await;

        assert!(!timed_out.passed, "unexpected assertion pass: {}", timed_out.message);
        assert!(
            timed_out.message.contains("arrived at the intake within"),
            "unexpected message: {}",
            timed_out.message
        );
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
    fn metric_type_and_value_must_hold_for_the_same_value_entry() {
        let observed = metric_with_values(
            "some.counter",
            &[],
            &[MetricValue::Count { value: 3.0 }, MetricValue::Gauge { value: 7.0 }],
        );

        assert!(assertion("some.counter", Some(MetricTypeMatcher::Count), Some(3.0), &[]).matches(&observed));
        assert!(assertion("some.counter", Some(MetricTypeMatcher::Gauge), Some(7.0), &[]).matches(&observed));

        // Each criterion holds for one entry, but no single entry satisfies both.
        assert!(!assertion("some.counter", Some(MetricTypeMatcher::Count), Some(7.0), &[]).matches(&observed));
        assert!(!assertion("some.counter", Some(MetricTypeMatcher::Gauge), Some(3.0), &[]).matches(&observed));
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
