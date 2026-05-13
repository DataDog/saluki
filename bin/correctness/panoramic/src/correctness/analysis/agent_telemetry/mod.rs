use std::collections::BTreeMap;

use saluki_error::{generic_error, GenericError};
use serde_json::Value;
use tracing::{error, info, warn};

use crate::correctness::analysis::collected::CollectedData;

/// A single normalized agent telemetry metric timeseries.
///
/// A "context" is the unique combination of metric name and tag set. Each context maps to exactly
/// one value in a given telemetry payload.
#[derive(Debug, Clone)]
struct AtelMetric {
    name: String,
    metric_type: String,
    /// Sorted tag pairs, keyed by tag name.
    tags: BTreeMap<String, String>,
    value: f64,
}

impl AtelMetric {
    /// Returns the context key — a stable string that uniquely identifies this timeseries.
    fn context_key(&self) -> String {
        if self.tags.is_empty() {
            self.name.clone()
        } else {
            let tags_str = self
                .tags
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}[{}]", self.name, tags_str)
        }
    }

    fn display_value(&self) -> String {
        format!("{}({})", self.metric_type, self.value)
    }
}

/// Analyzes agent telemetry payloads for correctness.
///
/// Compares the full set of metric timeseries (context + value) reported by the baseline and
/// comparison targets. Both targets must report the same contexts with the same values.
///
/// A test failure surfaces one of three categories of mismatch:
/// - **Context mismatch**: a timeseries present on one side but absent from the other.
/// - **Value mismatch**: the same context reported with a different value on each side.
/// - **No payloads**: either side emitted no agent telemetry at all.
pub struct AgentTelemetryAnalyzer {
    baseline_payloads: Vec<Value>,
    comparison_payloads: Vec<Value>,
}

impl AgentTelemetryAnalyzer {
    /// Creates a new `AgentTelemetryAnalyzer` from the given collected data.
    pub fn new(baseline_data: &CollectedData, comparison_data: &CollectedData) -> Self {
        Self {
            baseline_payloads: baseline_data.agent_telemetry_payloads().to_vec(),
            comparison_payloads: comparison_data.agent_telemetry_payloads().to_vec(),
        }
    }

    /// Runs the analysis.
    ///
    /// # Errors
    ///
    /// Returns an error if either side emitted no agent telemetry payloads, if the sets of
    /// reported contexts differ, or if any shared context has a different value on each side.
    pub fn run_analysis(self) -> Result<(), (GenericError, Vec<String>)> {
        info!(
            "Analyzing agent telemetry: {} payload(s) from baseline, {} payload(s) from comparison.",
            self.baseline_payloads.len(),
            self.comparison_payloads.len(),
        );

        if self.baseline_payloads.is_empty() {
            return Err((
                generic_error!(
                    "Baseline emitted no agent telemetry payloads. \
                     Check that `agent_telemetry.logs_dd_url` is set in datadog.yaml and \
                     `skip_ssl_validation: true` is configured."
                ),
                vec![],
            ));
        }
        if self.comparison_payloads.is_empty() {
            return Err((
                generic_error!(
                    "Comparison emitted no agent telemetry payloads. \
                     Check that `agent_telemetry.logs_dd_url` is set in datadog.yaml and \
                     `skip_ssl_validation: true` is configured."
                ),
                vec![],
            ));
        }

        let baseline_metrics = extract_metrics(&self.baseline_payloads);
        let comparison_metrics = extract_metrics(&self.comparison_payloads);

        info!(
            "Extracted {} context(s) from baseline, {} from comparison.",
            baseline_metrics.len(),
            comparison_metrics.len(),
        );

        // Build context-keyed maps for lookup.
        let baseline_map: BTreeMap<String, &AtelMetric> =
            baseline_metrics.iter().map(|m| (m.context_key(), m)).collect();
        let comparison_map: BTreeMap<String, &AtelMetric> =
            comparison_metrics.iter().map(|m| (m.context_key(), m)).collect();

        let mut details: Vec<String> = Vec::new();
        let mut context_mismatches = 0usize;
        let mut value_mismatches = 0usize;

        // Phase 1: contexts in baseline but not comparison.
        let baseline_only: Vec<&str> = baseline_map
            .keys()
            .filter(|k| !comparison_map.contains_key(*k))
            .map(|k| k.as_str())
            .collect();

        if !baseline_only.is_empty() {
            error!(
                "Agent telemetry context(s) in baseline but not in comparison ({} total):",
                baseline_only.len()
            );
            for ctx in &baseline_only {
                let m = &baseline_map[*ctx];
                error!("  - {} = {}", ctx, m.display_value());
                details.push(format!("baseline-only: {} = {}", ctx, m.display_value()));
                context_mismatches += 1;
            }
        }

        // Phase 2: contexts in comparison but not baseline.
        let comparison_only: Vec<&str> = comparison_map
            .keys()
            .filter(|k| !baseline_map.contains_key(*k))
            .map(|k| k.as_str())
            .collect();

        if !comparison_only.is_empty() {
            error!(
                "Agent telemetry context(s) in comparison but not in baseline ({} total):",
                comparison_only.len()
            );
            for ctx in &comparison_only {
                let m = &comparison_map[*ctx];
                error!("  - {} = {}", ctx, m.display_value());
                details.push(format!("comparison-only: {} = {}", ctx, m.display_value()));
                context_mismatches += 1;
            }
        }

        // Phase 3: shared contexts — compare values.
        let shared_keys: Vec<&str> = baseline_map
            .keys()
            .filter(|k| comparison_map.contains_key(*k))
            .map(|k| k.as_str())
            .collect();

        for ctx in shared_keys {
            let b = baseline_map[ctx];
            let c = comparison_map[ctx];

            if b.value != c.value {
                value_mismatches += 1;
                error!("Agent telemetry value mismatch for '{}':", ctx);
                error!("  baseline:    {}", b.display_value());
                error!("  comparison:  {}", c.display_value());
                let detail = format!(
                    "  {}\n    baseline:    {}\n    comparison:  {}",
                    ctx,
                    b.display_value(),
                    c.display_value()
                );
                details.push(detail);
            }
        }

        if context_mismatches == 0 && value_mismatches == 0 {
            info!(
                "Baseline and comparison agent telemetry match across {} context(s).",
                baseline_metrics.len()
            );
            return Ok(());
        }

        Err((
            generic_error!(
                "Agent telemetry mismatch: {} context difference(s), {} value mismatch(es).",
                context_mismatches,
                value_mismatches,
            ),
            details,
        ))
    }
}

// ---------------------------------------------------------------------------
// Extraction helpers
// ---------------------------------------------------------------------------

/// Extracts and sorts all metric timeseries from a set of raw APM telemetry payloads.
fn extract_metrics(payloads: &[Value]) -> Vec<AtelMetric> {
    let mut metrics = Vec::new();
    for payload in payloads {
        collect_from_payload(payload, &mut metrics);
    }
    metrics.sort_by_key(|a| a.context_key());
    metrics
}

fn collect_from_payload(payload: &Value, out: &mut Vec<AtelMetric>) {
    let request_type = payload.get("request_type").and_then(Value::as_str).unwrap_or("");
    match request_type {
        "agent-metrics" => collect_from_metrics_payload(payload.get("payload"), out),
        "message-batch" => {
            if let Some(batch) = payload.get("payload").and_then(Value::as_array) {
                for item in batch {
                    collect_from_payload(item, out);
                }
            }
        }
        other => {
            warn!("Skipping unknown agent telemetry request_type: {:?}", other);
        }
    }
}

fn collect_from_metrics_payload(payload: Option<&Value>, out: &mut Vec<AtelMetric>) {
    let metrics_map = match payload.and_then(|p| p.get("metrics")).and_then(Value::as_object) {
        Some(m) => m,
        None => return,
    };

    for (name, val) in metrics_map {
        if name == "agent_metadata" {
            continue;
        }

        let metric_type = val.get("type").and_then(Value::as_str).unwrap_or("?").to_string();

        let value = match val.get("value").and_then(Value::as_f64) {
            Some(v) => v,
            None => {
                warn!("Agent telemetry metric '{}' has no numeric value; skipping.", name);
                continue;
            }
        };

        let tags: BTreeMap<String, String> = val
            .get("tags")
            .and_then(Value::as_object)
            .map(|t| {
                t.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default();

        out.push(AtelMetric {
            name: name.clone(),
            metric_type,
            tags,
            value,
        });
    }
}
