//! ADP's `dogstatsd` commands as operations: `dogstatsd_capture`, `dogstatsd_replay` and `dogstatsd_stats`.
//!
//! Each runs the participant's command `adp` and answers with what it parsed from the output. A step decides whether
//! the answer is right. The output of a failed command is not quoted, because it can hold secrets.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::core::handle::{Exited, Handle, HandleError};
use crate::core::step::{Operation, Uses};

/// The most of a capture's output that a message quotes.
const QUOTED: usize = 1024;

/// Runs `adp` with `args`, and fails with the exit code alone if it exits nonzero.
async fn run_adp(on: &Handle<'_>, name: &str, args: &[String], limit: Duration) -> Result<Exited, HandleError> {
    let exited = on.command("adp", args, &BTreeMap::new(), limit).await?;
    if exited.code != 0 {
        return Err(HandleError::Software(format!(
            "agent-data-plane dogstatsd {} exited with code {}",
            name, exited.code
        )));
    }
    Ok(exited)
}

fn decode<T: for<'de> Deserialize<'de>>(name: &str, shape: &str, args: Value) -> Result<T, HandleError> {
    serde_json::from_value(args).map_err(|e| HandleError::Harness(format!("{} takes {}: {}", name, shape, e)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureArgs {
    duration_ms: u64,
}

/// `dogstatsd_capture`: starts a capture that lasts `duration_ms`, and answers `{path}`, the file it writes.
#[derive(Debug)]
pub(super) struct Capture;

#[async_trait]
impl Operation for Capture {
    fn name(&self) -> &'static str {
        "dogstatsd_capture"
    }

    fn uses(&self) -> &'static [Uses] {
        &[Uses::Command("adp")]
    }

    async fn run(&self, on: &Handle<'_>, args: Value, limit: Duration) -> Result<Value, HandleError> {
        let CaptureArgs { duration_ms } = decode(self.name(), "{duration_ms}", args)?;
        let argv = [
            "dogstatsd",
            "capture",
            "--duration",
            &format!("{}ms", duration_ms),
            "--compressed",
            "false",
        ]
        .map(String::from);
        let exited = run_adp(on, "capture", &argv, limit).await?;
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&exited.stdout),
            String::from_utf8_lossy(&exited.stderr)
        );
        match capture_path(&output) {
            Some(path) => Ok(json!({ "path": path })),
            None => {
                let stdout = String::from_utf8_lossy(&exited.stdout);
                let quoted: String = stdout.trim().chars().take(QUOTED).collect();
                Err(HandleError::Software(format!(
                    "DogStatsD capture did not report its output path: {}",
                    quoted
                )))
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayArgs {
    path: String,
}

/// `dogstatsd_replay`: replays the capture file `path`, and answers `{}`.
#[derive(Debug)]
pub(super) struct Replay;

#[async_trait]
impl Operation for Replay {
    fn name(&self) -> &'static str {
        "dogstatsd_replay"
    }

    fn uses(&self) -> &'static [Uses] {
        &[Uses::Command("adp")]
    }

    async fn run(&self, on: &Handle<'_>, args: Value, limit: Duration) -> Result<Value, HandleError> {
        let ReplayArgs { path } = decode(self.name(), "{path}", args)?;
        let argv = ["dogstatsd", "replay", "--file", &path].map(String::from);
        run_adp(on, "replay", &argv, limit).await?;
        Ok(json!({}))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatsArgs {
    duration_secs: u64,
}

/// `dogstatsd_stats`: collects statistics for `duration_secs`, and answers `{metrics}`, the name in each row.
#[derive(Debug)]
pub(super) struct Stats;

#[async_trait]
impl Operation for Stats {
    fn name(&self) -> &'static str {
        "dogstatsd_stats"
    }

    fn uses(&self) -> &'static [Uses] {
        &[Uses::Command("adp")]
    }

    async fn run(&self, on: &Handle<'_>, args: Value, limit: Duration) -> Result<Value, HandleError> {
        let StatsArgs { duration_secs } = decode(self.name(), "{duration_secs}", args)?;
        let argv = ["dogstatsd", "stats", "--duration-secs", &duration_secs.to_string()].map(String::from);
        let exited = run_adp(on, "stats", &argv, limit).await?;
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&exited.stdout),
            String::from_utf8_lossy(&exited.stderr)
        );
        Ok(json!({ "metrics": stats_metrics(&output) }))
    }
}

/// The path a capture reports, from the line `Data will be written to '<path>'`.
fn capture_path(output: &str) -> Option<&str> {
    let (_, after_prefix) = output.rsplit_once("Data will be written to '")?;
    let (path, _) = after_prefix.split_once('\'')?;
    Some(path)
}

/// The first cell of each row of the statistics table, which is a metric's name.
fn stats_metrics(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter_map(|line| {
            let (metric, _) = line.trim_start().strip_prefix('|')?.split_once('|')?;
            Some(metric.trim())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_path_is_read_from_cli_output() {
        let output = "2026-01-01T00:00:00Z INFO Capture started. Data will be written to '/tmp/capture.dog'.";

        assert_eq!(capture_path(output), Some("/tmp/capture.dog"));
    }

    #[test]
    fn stats_metrics_are_the_names_in_the_table() {
        let output = concat!(
            "+------------------+------+-------+-----------+\n",
            "| Metric           | Tags | Count | Last Seen |\n",
            "+------------------+------+-------+-----------+\n",
            "| replay.requests  |      | 1     |           |\n",
            "+------------------+------+-------+-----------+\n",
        );

        assert!(stats_metrics(output).contains(&"replay.requests"));
        assert!(!stats_metrics(output).contains(&"replay.request"));
    }
}
