//! Offline replay testbench for the anomaly detection core.
//!
//! This binary loads recorded Observer scenarios (Parquet v1 and v2 layouts), normalizes them into a
//! single ordered observation stream, replays that stream through the anomaly detection engine, and
//! writes a Go-compatible JSON artifact describing the run.
//!
//! The CLI is intentionally compatible with the Go testbench's flags so the two tools can be driven by
//! the same scripts during parity work. See [`run_cli`] for the flag set and the exit-code contract.
#![deny(warnings)]
// Some loader and diagnostic accessors (row counters, the catalog display names, the retained-loader
// skip list) are exercised only by tests or reserved for the export work that follows; keep the
// public surface intact rather than deleting it just to satisfy the lint.
#![allow(dead_code)]

mod config;
mod detector;
mod export;
mod parquet;
mod replay;
mod scenario;

use std::path::Path;

use detector::{BuiltinDetectorFactory, DetectorFactory};
use parquet::{LoadOptions, ParquetFormat};

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::ExitCode::from(run_cli(&args, &BuiltinDetectorFactory))
}

/// Runs the CLI with explicit arguments (without the program name) and a detector factory.
///
/// Returns the process exit code: `0` on success, `2` for a usage error (unknown flag, missing value,
/// invalid boolean), and `1` for a runtime error (invalid configuration, load failure, IO failure).
///
/// The detector factory is injected so tests can drive the full CLI and export path with deterministic
/// detectors while the statistical detector ports are unavailable.
pub fn run_cli<F: DetectorFactory>(args: &[String], factory: &F) -> u8 {
    match parse_args(args) {
        Ok(cli) => match execute(&cli, factory) {
            Ok(()) => 0,
            Err(message) => {
                eprintln!("Failed to run observer test bench: {message}");
                1
            }
        },
        Err(message) => {
            eprintln!("{message}");
            2
        }
    }
}

/// The parsed CLI arguments.
#[derive(Debug, Default, PartialEq)]
pub struct CliArgs {
    scenarios_dir: String,
    headless: String,
    config_file: Option<String>,
    only: Option<String>,
    enable: Option<String>,
    disable: Option<String>,
    baseline_duration: Option<String>,
    output: Option<String>,
    verbose: bool,
    include_detector_anomalies: bool,
    retain_parquet: bool,
    skip_dropped: bool,
    logs_only: bool,
    parquet_format: Option<String>,
    send_anomaly_event: Option<String>,
}

/// Parses the argument list, accepting Go-style `--name value`, `--name=value`, and single-dash flags.
fn parse_args(args: &[String]) -> Result<CliArgs, String> {
    let mut cli = CliArgs {
        scenarios_dir: "./comp/anomalydetection/observer/scenarios".to_string(),
        skip_dropped: true,
        ..CliArgs::default()
    };

    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        index += 1;

        let Some(flag) = arg.strip_prefix('-') else {
            return Err(format!("unexpected positional argument {arg:?}"));
        };
        let flag = flag.strip_prefix('-').unwrap_or(flag);
        let (name, inline_value) = match flag.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (flag, None),
        };

        let mut take_value = |name: &str| -> Result<String, String> {
            if let Some(value) = inline_value.clone() {
                return Ok(value);
            }
            if index < args.len() {
                let value = args[index].clone();
                index += 1;
                Ok(value)
            } else {
                Err(format!("flag --{name} requires a value"))
            }
        };

        match name {
            "scenarios-dir" => cli.scenarios_dir = take_value(name)?,
            "headless" => cli.headless = take_value(name)?,
            "config" => cli.config_file = Some(take_value(name)?),
            "only" => cli.only = Some(take_value(name)?),
            "enable" => cli.enable = Some(take_value(name)?),
            "disable" => cli.disable = Some(take_value(name)?),
            "baseline-duration" => cli.baseline_duration = Some(take_value(name)?),
            "output" => cli.output = Some(take_value(name)?),
            "parquet-format" => cli.parquet_format = Some(take_value(name)?),
            "send-anomaly-event" => cli.send_anomaly_event = Some(take_value(name)?),
            "verbose" => cli.verbose = bool_value(name, inline_value.as_deref())?,
            "include-detector-anomalies" => cli.include_detector_anomalies = bool_value(name, inline_value.as_deref())?,
            "retain-parquet" => cli.retain_parquet = bool_value(name, inline_value.as_deref())?,
            "skip-dropped" => cli.skip_dropped = bool_value(name, inline_value.as_deref())?,
            "logs-only" => cli.logs_only = bool_value(name, inline_value.as_deref())?,
            // Accepted for Go compatibility; the interactive server and profiling are not ported.
            "http" | "memprofile" | "cpuprofile" => {
                let _ = take_value(name)?;
            }
            "mute-noisy-metrics" => {
                let _ = bool_value(name, inline_value.as_deref())?;
            }
            other => return Err(format!("unknown flag --{other}")),
        }
    }

    Ok(cli)
}

fn bool_value(name: &str, inline: Option<&str>) -> Result<bool, String> {
    match inline {
        None => Ok(true),
        Some("true") => Ok(true),
        Some("false") => Ok(false),
        Some(other) => Err(format!("flag --{name} expects true or false, got {other:?}")),
    }
}

fn execute<F: DetectorFactory>(cli: &CliArgs, factory: &F) -> Result<(), String> {
    if cli.retain_parquet && cli.headless.is_empty() {
        return Err("--retain-parquet requires --headless".to_string());
    }
    if cli.include_detector_anomalies && cli.headless.is_empty() {
        return Err("--include-detector-anomalies requires --headless".to_string());
    }
    if cli.send_anomaly_event.is_some() {
        return Err("--send-anomaly-event is not yet ported to Rust".to_string());
    }
    if cli.headless.is_empty() {
        return Err("interactive mode is not yet ported to Rust; run with --headless <scenario>".to_string());
    }

    let settings = config::resolve(
        cli.config_file.as_deref().map(Path::new),
        cli.only.as_deref(),
        cli.enable.as_deref(),
        cli.disable.as_deref(),
        cli.baseline_duration.as_deref(),
    )
    .map_err(|err| format!("resolving component settings: {err}"))?;

    let format = parse_format(cli.parquet_format.as_deref())?;
    let scenario_path = Path::new(&cli.scenarios_dir).join(&cli.headless);
    if !scenario_path.exists() {
        return Err(format!("scenario not found: {}", cli.headless));
    }
    let data_dir = scenario::scenario_parquet_data_dir(&scenario_path).unwrap_or_else(|| scenario_path.clone());
    let format = format.unwrap_or_else(|| scenario::detect_format(&data_dir));

    let load = LoadOptions {
        skip_dropped_metrics: cli.skip_dropped,
        logs_only: cli.logs_only,
    };

    let result = replay::run_replay(
        &data_dir,
        format,
        &load,
        cli.retain_parquet,
        &settings,
        cli.include_detector_anomalies,
        factory,
    )
    .map_err(|err| format!("replaying scenario {:?}: {err}", cli.headless))?;

    if !result.unlinked_detectors.is_empty() {
        eprintln!(
            "warning: enabled detectors are not linked into this build and were skipped: {}",
            result.unlinked_detectors.join(", ")
        );
    }

    if let Some(output) = &cli.output {
        let document = export::build_output(&cli.headless, &result, &settings, cli.verbose);
        export::write_output(Path::new(output), &document)
            .map_err(|err| format!("writing observer output to {output}: {err}"))?;
        println!("Observer output written to {output}");
    }

    Ok(())
}

fn parse_format(value: Option<&str>) -> Result<Option<ParquetFormat>, String> {
    match value {
        None | Some("") => Ok(None),
        Some(value) if value.eq_ignore_ascii_case("v1") => Ok(Some(ParquetFormat::V1)),
        Some(value) if value.eq_ignore_ascii_case("v2") => Ok(Some(ParquetFormat::V2)),
        Some(value) => Err(format!(
            "invalid --parquet-format {value:?}: expected \"v1\", \"v2\", or empty for auto-detect"
        )),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::detector::StubDetectorFactory;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| item.to_string()).collect()
    }

    /// Writes a v1 metric series under `<root>/<scenario>/`: a low ramp that steps to 100 at `step_sec`.
    fn write_stepped_metric(root: &Path, scenario: &str, step_sec: i64) {
        use crate::parquet::fixtures::{v1_metric, write_v1_metrics, V1MetricFixt};

        let scenario_dir = root.join(scenario);
        std::fs::create_dir_all(&scenario_dir).unwrap();
        let mut rows: Vec<V1MetricFixt> = Vec::new();
        for second in 1_000..1_010 {
            let value = if second >= step_sec { 100.0 } else { 1.0 };
            rows.push(v1_metric("run", second * 1_000, "system.cpu", Some(value)));
        }
        write_v1_metrics(&scenario_dir, "observer-metrics-0.parquet", &rows);
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn parses_go_style_flags() {
        let cli = parse_args(&args(&[
            "--headless",
            "s",
            "--scenarios-dir=/tmp/x",
            "--skip-dropped=false",
            "-verbose",
            "--parquet-format",
            "v2",
        ]))
        .unwrap();
        assert_eq!(cli.headless, "s");
        assert_eq!(cli.scenarios_dir, "/tmp/x");
        assert!(!cli.skip_dropped);
        assert!(cli.verbose);
        assert_eq!(cli.parquet_format.as_deref(), Some("v2"));
    }

    #[test]
    fn rejects_unknown_flags_and_missing_values() {
        assert!(parse_args(&args(&["--nope"])).is_err());
        assert!(parse_args(&args(&["--output"])).is_err());
        assert!(parse_args(&args(&["--verbose=maybe"])).is_err());
        assert!(parse_args(&args(&["positional"])).is_err());
    }

    #[test]
    fn headless_streaming_end_to_end_produces_expected_json() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_stepped_metric(root, "scenario", 1_005);
        let output = root.join("out.json");

        let code = run_cli(
            &args(&[
                "--headless",
                "scenario",
                "--scenarios-dir",
                root.to_str().unwrap(),
                "--only",
                "bocpd,anomaly_scorer",
                "--include-detector-anomalies",
                "--baseline-duration",
                "0",
                "--output",
                output.to_str().unwrap(),
            ]),
            &StubDetectorFactory::new(50.0),
        );
        assert_eq!(code, 0);

        let document = read_json(&output);
        // Metadata: component_configs with enabled flags.
        assert_eq!(document["metadata"]["scenario"], "scenario");
        assert_eq!(document["metadata"]["replay_mode"], "streaming");
        assert_eq!(
            document["metadata"]["component_configs"]["bocpd"]["enabled"],
            Value::Bool(true)
        );
        assert_eq!(
            document["metadata"]["component_configs"]["scanmw"]["enabled"],
            Value::Bool(false)
        );
        assert_eq!(document["metadata"]["detectors_enabled"], serde_json::json!(["bocpd"]));

        // One detector anomaly with the Go ledger field names.
        let anomalies = document["detector_anomalies"].as_array().unwrap();
        assert_eq!(anomalies.len(), 1, "{document}");
        assert_eq!(anomalies[0]["detector"], "bocpd");
        assert_eq!(anomalies[0]["timestamp"], serde_json::json!(1_005));
        assert!(anomalies[0]["source"].is_string());
        assert!(anomalies[0]["title"].is_string());
        assert_eq!(document["metadata"]["total_detector_anomalies"], serde_json::json!(1));

        // anomaly_periods is present (possibly empty) with the Go field names.
        assert!(document["anomaly_periods"].is_array());

        // Per-second score timeline with the required fields.
        let timeline = document["score_timeline"].as_array().unwrap();
        assert!(!timeline.is_empty(), "{document}");
        let tick = &timeline[0];
        assert!(tick["second"].is_i64());
        assert_eq!(tick["bins"].as_array().unwrap().len(), 5);
        assert!(tick["count"].is_u64());
        assert!(tick["weight_sum"].is_f64());
        assert!(tick["input"].is_f64());
        assert!(tick["ewma"].is_f64());
        assert!(tick["raw_severity"].is_string());
        assert!(
            tick["delivered_severity"].is_null() || tick["delivered_severity"].is_string(),
            "{tick}"
        );
    }

    #[test]
    fn headless_export_includes_anomaly_periods_and_verbose_detail() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_stepped_metric(root, "scenario", 1_005);
        let config_path = root.join("params.json");
        // A metrics-only config (README section 13.1 shape) with deliberately tiny scorer thresholds
        // so a single uncalibrated stub anomaly opens a correlation episode.
        std::fs::write(
            &config_path,
            r#"{"components":{
                "bocpd":{"enabled":true},
                "scanmw":{"enabled":false},
                "scanwelch":{"enabled":false},
                "holt_residual":{"enabled":false},
                "tukey_biweight":{"enabled":false},
                "anomaly_scorer":{"enabled":true,"correlation_events":true,
                    "correlation_event_threshold":"medium",
                    "low_threshold":0.001,"high_threshold":0.002}
            }}"#,
        )
        .unwrap();
        let output = root.join("out.json");

        let code = run_cli(
            &args(&[
                "--headless",
                "scenario",
                "--scenarios-dir",
                root.to_str().unwrap(),
                "--config",
                config_path.to_str().unwrap(),
                "--verbose",
                "--include-detector-anomalies",
                "--output",
                output.to_str().unwrap(),
            ]),
            &StubDetectorFactory::new(50.0),
        );
        assert_eq!(code, 0);

        let document = read_json(&output);
        let periods = document["anomaly_periods"].as_array().unwrap();
        assert_eq!(periods.len(), 1, "{document}");
        assert_eq!(document["metadata"]["total_anomaly_periods"], serde_json::json!(1));
        assert!(periods[0]["pattern"].is_string());
        assert!(periods[0]["title"].is_string());
        assert!(periods[0]["message"].is_string());
        assert!(periods[0]["tags"].is_array());
        assert!(periods[0]["member_series"].is_array());
        assert!(periods[0]["anomalies"].is_array());
    }

    #[test]
    fn v2_scenario_end_to_end() {
        use crate::parquet::fixtures::{context, v2_metric, write_contexts_v2, write_v2_metrics};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let scenario_dir = root.join("v2");
        std::fs::create_dir_all(&scenario_dir).unwrap();
        write_contexts_v2(&scenario_dir, &[context(1, "system.cpu", Some("host-a"), None)], false);
        let rows: Vec<_> = (1_000..1_008)
            .map(|second| {
                let value = if second >= 1_004 { 100.0 } else { 1.0 };
                v2_metric(1, value, Some(second * 1_000_000_000), Some("check"))
            })
            .collect();
        write_v2_metrics(&scenario_dir, "metrics-0.parquet", &rows);
        let output = root.join("v2.json");

        let code = run_cli(
            &args(&[
                "--headless",
                "v2",
                "--scenarios-dir",
                root.to_str().unwrap(),
                "--only",
                "bocpd,anomaly_scorer",
                "--output",
                output.to_str().unwrap(),
            ]),
            &StubDetectorFactory::new(50.0),
        );
        assert_eq!(code, 0);

        let document = read_json(&output);
        assert_eq!(
            document["metadata"]["stats"]["input_metrics_count"],
            serde_json::json!(8)
        );
        assert!(!document["score_timeline"].as_array().unwrap().is_empty());
        assert_eq!(document["detector_anomalies"], Value::Null, "ledger not requested");
    }

    #[test]
    fn determinism_two_runs_share_identical_score_timeline() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_stepped_metric(root, "scenario", 1_005);

        let mut timelines = Vec::new();
        for index in 0..2 {
            let output = root.join(format!("run-{index}.json"));
            let code = run_cli(
                &args(&[
                    "--headless",
                    "scenario",
                    "--scenarios-dir",
                    root.to_str().unwrap(),
                    "--only",
                    "bocpd,anomaly_scorer",
                    "--output",
                    output.to_str().unwrap(),
                ]),
                &StubDetectorFactory::new(50.0),
            );
            assert_eq!(code, 0);
            timelines.push(read_json(&output)["score_timeline"].clone());
        }
        assert_eq!(timelines[0], timelines[1]);
    }

    #[test]
    fn retained_mode_replays_and_labels_its_mode() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_stepped_metric(root, "scenario", 1_005);
        let output = root.join("retained.json");

        let code = run_cli(
            &args(&[
                "--headless",
                "scenario",
                "--scenarios-dir",
                root.to_str().unwrap(),
                "--only",
                "bocpd,anomaly_scorer",
                "--retain-parquet",
                "--output",
                output.to_str().unwrap(),
            ]),
            &StubDetectorFactory::new(50.0),
        );
        assert_eq!(code, 0);

        let document = read_json(&output);
        assert_eq!(document["metadata"]["replay_mode"], "retained");
        assert!(!document["score_timeline"].as_array().unwrap().is_empty());
    }

    #[test]
    fn config_file_takes_precedence_over_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_stepped_metric(root, "scenario", 1_005);
        let config_path = root.join("params.json");
        std::fs::write(
            &config_path,
            r#"{"components":{"bocpd":{"enabled":false,"warmup_points":77}}}"#,
        )
        .unwrap();
        let output = root.join("out.json");

        let code = run_cli(
            &args(&[
                "--headless",
                "scenario",
                "--scenarios-dir",
                root.to_str().unwrap(),
                "--config",
                config_path.to_str().unwrap(),
                "--only",
                "bocpd,anomaly_scorer",
                "--output",
                output.to_str().unwrap(),
            ]),
            &StubDetectorFactory::new(50.0),
        );
        assert_eq!(code, 0);

        let document = read_json(&output);
        assert_eq!(
            document["metadata"]["component_configs"]["bocpd"]["enabled"],
            Value::Bool(false)
        );
        assert_eq!(
            document["metadata"]["component_configs"]["bocpd"]["warmup_points"],
            serde_json::json!(77)
        );
        assert_eq!(document["detector_anomalies"], Value::Null);
    }

    #[test]
    fn enabling_time_cluster_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_stepped_metric(root, "scenario", 1_005);
        let config_path = root.join("params.json");
        std::fs::write(&config_path, r#"{"components":{"time_cluster":{"enabled":true}}}"#).unwrap();

        let code = run_cli(
            &args(&[
                "--headless",
                "scenario",
                "--scenarios-dir",
                root.to_str().unwrap(),
                "--config",
                config_path.to_str().unwrap(),
            ]),
            &StubDetectorFactory::new(50.0),
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn nonzero_baseline_duration_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write_stepped_metric(root, "scenario", 1_005);
        let code = run_cli(
            &args(&[
                "--headless",
                "scenario",
                "--scenarios-dir",
                root.to_str().unwrap(),
                "--baseline-duration",
                "7m",
            ]),
            &StubDetectorFactory::new(50.0),
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn headless_only_flags_are_validated() {
        let code = run_cli(&args(&["--retain-parquet"]), &StubDetectorFactory::new(50.0));
        assert_eq!(code, 1, "--retain-parquet requires --headless");

        let code = run_cli(
            &args(&["--include-detector-anomalies"]),
            &StubDetectorFactory::new(50.0),
        );
        assert_eq!(code, 1, "--include-detector-anomalies requires --headless");

        let code = run_cli(&args(&[]), &StubDetectorFactory::new(50.0));
        assert_eq!(code, 1, "interactive mode is not ported");
    }

    #[test]
    fn missing_scenario_is_reported() {
        let temp = tempfile::tempdir().unwrap();
        let code = run_cli(
            &args(&["--headless", "nope", "--scenarios-dir", temp.path().to_str().unwrap()]),
            &StubDetectorFactory::new(50.0),
        );
        assert_eq!(code, 1);
    }

    #[test]
    fn parquet_format_flag_is_validated() {
        assert!(parse_format(Some("v1")).is_ok());
        assert!(parse_format(Some("V2")).is_ok());
        assert!(parse_format(Some("")).is_ok());
        assert!(parse_format(Some("v3")).is_err());
    }
}
