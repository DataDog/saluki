//! Go-compatible headless JSON export.
//!
//! The output shape mirrors `internal/qbranch/anomalydetection-testbench/bench/output.go`: a
//! `metadata` block, an `anomaly_periods` array, and — when `--include-detector-anomalies` is set — a
//! `detector_anomalies` ledger. Two fields are additive to the Go shape and never replace a Go field:
//! `metadata.replay_mode`, and the per-second `score_timeline` array (one entry per scorer second,
//! including empty seconds).
//!
//! `metadata.component_configs` follows the Go contract (an object per component with an `enabled`
//! flag) and adds the fully resolved hyperparameters, so a Go/Rust configuration divergence is
//! visible in the artifact.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use saluki_anomaly_detection::identity::{Aggregate, SeriesDescriptor};
use saluki_anomaly_detection::model::{Anomaly, AnomalyEvidence, BocpdTrigger};
use saluki_anomaly_detection::scorer::ActiveCorrelation;
use serde::Serialize;
use serde_json::Value;

use crate::config::ResolvedSettings;
use crate::replay::ReplayResult;

/// The top-level headless output document.
#[derive(Debug, Serialize)]
pub struct ObserverOutput {
    /// Scenario and pipeline metadata.
    pub metadata: ObserverMetadata,
    /// Correlation episodes.
    pub anomaly_periods: Vec<ObserverCorrelation>,
    /// The pre-pipeline detector ledger, present only when requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detector_anomalies: Option<Vec<DetectorOutputAnomaly>>,
    /// One entry per scorer second (additive to the Go shape).
    pub score_timeline: Vec<ScoreTimelineEntry>,
}

/// Scenario and pipeline metadata.
#[derive(Debug, Serialize)]
pub struct ObserverMetadata {
    /// The scenario name.
    pub scenario: String,
    /// The replay mode (`streaming` or `retained`); additive to the Go shape.
    pub replay_mode: String,
    /// First observed data second.
    pub timeline_start: i64,
    /// Last observed data second.
    pub timeline_end: i64,
    /// Sorted names of enabled detectors.
    pub detectors_enabled: Vec<String>,
    /// Sorted names of enabled correlators.
    pub correlators_enabled: Vec<String>,
    /// Number of anomaly periods.
    pub total_anomaly_periods: usize,
    /// Number of detector-ledger entries, present only when the ledger is exported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_detector_anomalies: Option<usize>,
    /// Fully resolved configuration per component.
    pub component_configs: BTreeMap<String, Value>,
    /// Enabled detectors this build has no implementation for; additive to the Go shape.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unlinked_detectors: Vec<String>,
    /// Replay input statistics.
    pub stats: ReplayStats,
}

/// Replay input statistics, matching the Go `ReplayStats` counts.
#[derive(Debug, Serialize)]
pub struct ReplayStats {
    /// Number of accepted metric observations.
    pub input_metrics_count: usize,
    /// Number of unique metric series.
    pub input_metrics_cardinality: usize,
    /// Number of log observations.
    pub input_logs_count: usize,
    /// Number of accepted detector anomalies.
    pub input_anomalies_count: usize,
}

/// One correlation episode.
#[derive(Debug, Serialize)]
pub struct ObserverCorrelation {
    /// The episode pattern.
    pub pattern: String,
    /// First-seen data second.
    pub period_start: i64,
    /// Last-updated data second.
    pub period_end: i64,
    /// Display title (verbose only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Human-readable change message (verbose only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Reporting tags (verbose only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// Member series display names (verbose only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub member_series: Option<Vec<String>>,
    /// Nested anomalies (verbose only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anomalies: Option<Vec<ObserverAnomaly>>,
}

/// One anomaly nested inside a correlation.
#[derive(Debug, Serialize)]
pub struct ObserverAnomaly {
    /// Data second.
    pub timestamp: i64,
    /// Series display string.
    pub source: String,
    /// Compact series reference.
    pub source_series_id: String,
    /// Detector name.
    pub detector: String,
}

/// One pre-pipeline detector anomaly.
#[derive(Debug, Serialize)]
pub struct DetectorOutputAnomaly {
    /// Detector name.
    pub detector: String,
    /// Data second.
    pub timestamp: i64,
    /// Canonical series key.
    pub source: String,
    /// Formatted anomaly title.
    pub title: String,
    /// Detector score; `null` when the detector produced no score (additive to the Go shape).
    pub score: Option<f64>,
}

/// One per-second score tick.
#[derive(Debug, Serialize)]
pub struct ScoreTimelineEntry {
    /// The scorer second.
    pub second: i64,
    /// Deduplicated anomalous-series count per severity bin.
    pub bins: [usize; 5],
    /// Total anomalous series count.
    pub count: usize,
    /// Sum of discrete level weights.
    pub weight_sum: f64,
    /// Saturated per-second input.
    pub input: f64,
    /// EWMA after this second.
    pub ewma: f64,
    /// Raw severity derived from the EWMA.
    pub raw_severity: String,
    /// Delivered severity, or `null` when episode tracking is disabled.
    pub delivered_severity: Option<String>,
}

/// Builds the export document from a completed replay.
pub fn build_output(
    scenario: &str, result: &ReplayResult, settings: &ResolvedSettings, verbose: bool,
) -> ObserverOutput {
    let anomaly_periods: Vec<ObserverCorrelation> = result
        .correlations
        .iter()
        .map(|correlation| build_correlation(correlation, verbose))
        .collect();

    // The ledger is exported whenever the caller asked for it, even when it is empty.
    let detector_anomalies = if result.detector_anomalies_requested {
        Some(build_detector_anomalies(&result.detector_anomalies))
    } else {
        None
    };

    let total_detector_anomalies = detector_anomalies.as_ref().map(Vec::len);

    let metadata = ObserverMetadata {
        scenario: scenario.to_string(),
        replay_mode: result.replay_mode.as_str().to_string(),
        timeline_start: result.timeline_start,
        timeline_end: result.timeline_end,
        detectors_enabled: settings.detectors_enabled(),
        correlators_enabled: settings.correlators_enabled(),
        total_anomaly_periods: anomaly_periods.len(),
        total_detector_anomalies,
        component_configs: settings.component_configs(),
        unlinked_detectors: result.unlinked_detectors.clone(),
        stats: ReplayStats {
            input_metrics_count: result.input_metrics_count,
            input_metrics_cardinality: result.input_metrics_cardinality,
            input_logs_count: result.input_logs_count,
            input_anomalies_count: result.input_anomalies_count,
        },
    };

    ObserverOutput {
        metadata,
        anomaly_periods,
        detector_anomalies,
        score_timeline: result
            .score_timeline
            .iter()
            .map(|tick| ScoreTimelineEntry {
                second: tick.second,
                bins: tick.bins,
                count: tick.count,
                weight_sum: tick.weight_sum,
                input: tick.input,
                ewma: tick.ewma,
                raw_severity: tick.raw_severity.as_str().to_string(),
                delivered_severity: tick.delivered_severity.map(|level| level.as_str().to_string()),
            })
            .collect(),
    }
}

fn build_correlation(correlation: &ActiveCorrelation, verbose: bool) -> ObserverCorrelation {
    let mut output = ObserverCorrelation {
        pattern: correlation.pattern.clone(),
        period_start: correlation.first_seen,
        period_end: correlation.last_updated,
        title: None,
        message: None,
        tags: None,
        member_series: None,
        anomalies: None,
    };

    if verbose {
        output.title = Some(correlation.title.clone());
        output.message = Some(build_change_message(correlation));
        output.tags = Some(vec![
            "source:agent-q-branch-observer".to_string(),
            format!("pattern:{}", correlation.pattern),
        ]);
        output.member_series = Some(
            correlation
                .members
                .iter()
                .map(series_display_name)
                .collect::<Vec<String>>(),
        );
        output.anomalies = Some(
            correlation
                .anomalies
                .iter()
                .map(|anomaly| ObserverAnomaly {
                    timestamp: anomaly.timestamp_sec,
                    source: series_string(&anomaly.series),
                    source_series_id: anomaly.series_ref.map(|handle| handle.compact_id()).unwrap_or_default(),
                    detector: anomaly.detector_name.clone(),
                })
                .collect(),
        );
    }
    output
}

fn build_detector_anomalies(anomalies: &[Anomaly]) -> Vec<DetectorOutputAnomaly> {
    let mut entries: Vec<DetectorOutputAnomaly> = anomalies
        .iter()
        .map(|anomaly| DetectorOutputAnomaly {
            detector: anomaly.detector_name.clone(),
            timestamp: anomaly.timestamp_sec,
            source: anomaly.series.identity().key(),
            title: format_anomaly(anomaly).0,
            score: anomaly.score,
        })
        .collect();
    entries.sort_by(|left, right| {
        left.detector
            .cmp(&right.detector)
            .then_with(|| left.timestamp.cmp(&right.timestamp))
            .then_with(|| left.source.cmp(&right.source))
            .then_with(|| left.title.cmp(&right.title))
    });
    entries
}

/// Serializes `output` to `path` as two-space-indented JSON.
pub fn write_output(path: &Path, output: &ObserverOutput) -> io::Result<()> {
    let text = serde_json::to_string_pretty(output).map_err(io::Error::other)?;
    std::fs::write(path, text)
}

/// Returns the Go `SeriesDescriptor.String()` form (`name` or `name:agg`).
pub fn series_string(series: &SeriesDescriptor) -> String {
    if series.name.is_empty() {
        return String::new();
    }
    if series.aggregate == Aggregate::None {
        return series.name.clone();
    }
    format!("{}:{}", series.name, aggregate_str(series.aggregate))
}

/// Returns the Go `SeriesDescriptor.DisplayName()` form (`name:agg{host:...,tag}`).
pub fn series_display_name(series: &SeriesDescriptor) -> String {
    let base = series_string(series);
    let host = series.host.as_deref().unwrap_or("");
    if series.tags.is_empty() && host.is_empty() {
        return base;
    }

    let host_tag = format!("host:{host}");
    let mut display = format!("{base}{{");
    if !host.is_empty() && !series.tags.iter().any(|tag| tag == &host_tag) {
        display.push_str(&host_tag);
        if !series.tags.is_empty() {
            display.push(',');
        }
    }
    display.push_str(&series.tags.join(","));
    display.push('}');
    display
}

fn aggregate_str(aggregate: Aggregate) -> &'static str {
    aggregate.as_str()
}

/// Formats a detector anomaly title and description, porting `observer/def/format.go`.
pub fn format_anomaly(anomaly: &Anomaly) -> (String, String) {
    let source = series_string(&anomaly.series);
    let Some(evidence) = anomaly.evidence.as_ref() else {
        return (fallback_title(&anomaly.detector_name, &source), String::new());
    };

    match evidence {
        AnomalyEvidence::ScanMw {
            baseline_median,
            current_value,
            deviation_sigma,
            p_value,
            effect_size,
            ..
        } => (
            format!("ScanMW changepoint: {source}"),
            format!(
                "{source} {} (pre_median={:.4}, post_median={:.4}, p={:.2e}, effect={:.2}, {:.1} MADs)",
                direction(*current_value, *baseline_median),
                baseline_median,
                current_value,
                p_value,
                effect_size,
                deviation_sigma
            ),
        ),
        AnomalyEvidence::ScanWelch {
            baseline_median,
            current_value,
            deviation_sigma,
            p_value,
            effect_size,
            test_statistic,
            ..
        } => (
            format!("ScanWelch changepoint: {source}"),
            format!(
                "{source} {} (pre_median={:.4}, post_median={:.4}, t={:.2}, p={:.2e}, effect={:.2}, {:.1} MADs)",
                direction(*current_value, *baseline_median),
                baseline_median,
                current_value,
                test_statistic,
                p_value,
                effect_size,
                deviation_sigma
            ),
        ),
        AnomalyEvidence::Bocpd {
            baseline_mean: _,
            current_value,
            threshold,
            trigger,
            change_point_prob,
            short_run_mass,
            short_run_length,
            deviation_sigma,
            ..
        } => {
            let (trigger_type, trigger_value) = match trigger {
                BocpdTrigger::ChangePointProbability => ("changepoint probability", *change_point_prob),
                BocpdTrigger::ShortRunMass => ("short-run posterior mass", *short_run_mass),
                BocpdTrigger::Unknown => return (fallback_title(&anomaly.detector_name, &source), String::new()),
            };
            (
                format!("BOCPD changepoint detected: {source}"),
                format!(
                    "{source} {trigger_type} {trigger_value:.2} exceeded threshold {threshold:.2} (cp={change_point_prob:.2}, \
                     short-run<={short_run_length} mass={short_run_mass:.2}, |z|={deviation_sigma:.1}, value={current_value:.4})"
                ),
            )
        }
        AnomalyEvidence::HoltResidual {
            current_value,
            forecast,
            residual,
            deviation_sigma,
            holt_level,
            holt_trend,
            value_mads,
            ..
        } => (
            format!("Holt residual: {source}"),
            format!(
                "{source} deviated from forecast (observed={current_value:.4}, forecast={forecast:.4}, \
                 residual={residual:.4}, |z|={deviation_sigma:.2}, level={holt_level:.4}, trend={holt_trend:.4}, \
                 {value_mads:.1} valueMADs)"
            ),
        ),
        AnomalyEvidence::TukeyBiweight {
            baseline_median,
            baseline_mad,
            z_score,
            sample_count,
            ..
        } => {
            let direction = if *z_score < 0.0 { "below" } else { "above" };
            (
                format!("Tukey biweight: {source}"),
                format!(
                    "{source} {direction} biweight baseline (z={z_score:.2}, mu={baseline_median:.4}, \
                     sigma={baseline_mad:.4}, n={sample_count})"
                ),
            )
        }
    }
}

fn direction(current: f64, baseline: f64) -> &'static str {
    if current < baseline {
        "decreased"
    } else {
        "increased"
    }
}

fn fallback_title(detector: &str, source: &str) -> String {
    if source.is_empty() {
        return format!("Anomaly detected: {detector}");
    }
    if detector.is_empty() {
        return format!("Anomaly detected: {source}");
    }
    format!("Anomaly detected: {detector}: {source}")
}

/// Builds the compact human-readable change message (the Go metric path).
fn build_change_message(correlation: &ActiveCorrelation) -> String {
    let mut lines: Vec<String> = correlation
        .anomalies
        .iter()
        .map(|anomaly| {
            let (_, description) = format_anomaly(anomaly);
            if description.is_empty() {
                format!("- {}", series_display_name(&anomaly.series))
            } else {
                format!("- {description}")
            }
        })
        .collect();
    lines.sort();
    lines.dedup();

    let mut message = vec![format!(
        "Correlated behavior change detected: {} anomalies in pattern {:?}",
        lines.len(),
        correlation.pattern
    )];
    message.push(String::new());
    message.extend(lines);
    message.join("\n")
}
