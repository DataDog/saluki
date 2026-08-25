use std::time::{Duration, Instant};

use saluki_error::generic_error;

use super::{
    target_exec::{execute_target_command, CommandDiagnostics},
    Action,
};
use crate::assertions::{AssertionContext, AssertionResult};

/// Runs a complete DogStatsD capture and replay flow against the tested ADP process.
pub(super) struct DogstatsdReplayAction {
    sender: Vec<String>,
    capture_duration: Duration,
    stats_duration_secs: u64,
    expected_metrics: Vec<String>,
    command_timeout: Duration,
}

impl DogstatsdReplayAction {
    /// Creates an action that captures traffic from `sender` and verifies its replay through DogStatsD statistics.
    pub(super) fn new(
        sender: Vec<String>, capture_duration: Duration, stats_duration_secs: u64, expected_metrics: Vec<String>,
        command_timeout: Duration,
    ) -> Self {
        Self {
            sender,
            capture_duration,
            stats_duration_secs,
            expected_metrics,
            command_timeout,
        }
    }

    fn failure(&self, started: Instant, message: impl Into<String>) -> AssertionResult {
        AssertionResult {
            name: self.name().to_string(),
            passed: false,
            message: message.into(),
            duration: started.elapsed(),
        }
    }
}

#[async_trait::async_trait]
impl Action for DogstatsdReplayAction {
    fn name(&self) -> &'static str {
        "dogstatsd_replay"
    }

    fn description(&self) -> String {
        "Capture and replay DogStatsD traffic through the tested ADP process".to_string()
    }

    async fn execute(&self, ctx: &AssertionContext) -> AssertionResult {
        let started = Instant::now();
        let adp_cli = &ctx.adp_cli_command;
        let diagnostics = CommandDiagnostics::redacted("tested ADP CLI command");
        let capture_command = adp_cli.with_args(&[
            "dogstatsd".to_string(),
            "capture".to_string(),
            "--duration".to_string(),
            duration_as_go_duration(self.capture_duration),
            "--compressed".to_string(),
            "false".to_string(),
        ]);
        let capture_output = match execute_target_command(
            ctx,
            &capture_command,
            &diagnostics,
            self.command_timeout,
            adp_cli.host_env(),
        )
        .await
        {
            Ok(output) => output,
            Err(error) => return self.failure(started, format!("Failed to start DogStatsD capture: {error}")),
        };
        let capture_path = match capture_path_from_output(&capture_output) {
            Ok(path) => path,
            Err(error) => return self.failure(started, error.to_string()),
        };

        if let Err(error) = execute_target_command(
            ctx,
            &self.sender,
            &CommandDiagnostics::redacted("DogStatsD replay test sender"),
            self.command_timeout,
            None,
        )
        .await
        {
            return self.failure(
                started,
                format!("Failed to send DogStatsD traffic for capture: {error}"),
            );
        }

        // The capture writer owns the capture file and writes its state trailer only after the requested duration.
        tokio::time::sleep(self.capture_duration).await;

        let stats_command = adp_cli.with_args(&[
            "dogstatsd".to_string(),
            "stats".to_string(),
            "--duration-secs".to_string(),
            self.stats_duration_secs.to_string(),
        ]);
        let replay_command = adp_cli.with_args(&[
            "dogstatsd".to_string(),
            "replay".to_string(),
            "--file".to_string(),
            capture_path,
        ]);
        let deadline = Instant::now() + self.command_timeout;
        let mut last_replay_error = None;
        let mut last_missing_metrics = Vec::new();

        loop {
            let stats = execute_target_command(
                ctx,
                &stats_command,
                &diagnostics,
                self.command_timeout,
                adp_cli.host_env(),
            );
            let replay = execute_target_command(
                ctx,
                &replay_command,
                &diagnostics,
                self.command_timeout,
                adp_cli.host_env(),
            );
            let (stats_result, replay_result) = tokio::join!(stats, replay);

            match replay_result {
                Ok(_) => match stats_result {
                    Ok(stats_output) => {
                        let missing_metrics: Vec<&str> = self
                            .expected_metrics
                            .iter()
                            .map(String::as_str)
                            .filter(|metric| !stats_output_contains_metric(&stats_output, metric))
                            .collect();
                        if missing_metrics.is_empty() {
                            break;
                        }
                        last_missing_metrics = missing_metrics.into_iter().map(str::to_string).collect();
                    }
                    Err(error) => {
                        return self.failure(
                            started,
                            format!("Failed to collect DogStatsD replay statistics: {error}"),
                        )
                    }
                },
                Err(error) => last_replay_error = Some(error.to_string()),
            }

            if Instant::now() >= deadline {
                if let Some(error) = last_replay_error {
                    return self.failure(started, format!("Failed to replay DogStatsD capture: {error}"));
                }
                return self.failure(
                    started,
                    format!(
                        "DogStatsD replay statistics did not contain expected metric(s): {}",
                        last_missing_metrics.join(", ")
                    ),
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        AssertionResult {
            name: self.name().to_string(),
            passed: true,
            message: self.description(),
            duration: started.elapsed(),
        }
    }
}

fn duration_as_go_duration(duration: Duration) -> String {
    format!("{}ms", duration.as_millis())
}

fn stats_output_contains_metric(output: &str, expected_metric: &str) -> bool {
    output.lines().any(|line| {
        line.trim_start()
            .strip_prefix('|')
            .and_then(|row| row.split_once('|'))
            .is_some_and(|(metric, _)| metric.trim() == expected_metric)
    })
}

fn capture_path_from_output(output: &str) -> Result<String, saluki_error::GenericError> {
    let (_, after_prefix) = output
        .rsplit_once("Data will be written to '")
        .ok_or_else(|| generic_error!("DogStatsD capture did not report its output path: {}", output.trim()))?;
    let (path, _) = after_prefix.split_once('\'').ok_or_else(|| {
        generic_error!(
            "DogStatsD capture reported an unterminated output path: {}",
            output.trim()
        )
    })?;
    Ok(path.to_string())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{capture_path_from_output, duration_as_go_duration, stats_output_contains_metric};

    #[test]
    fn capture_path_parser_extracts_path_from_cli_output() {
        let output = "2026-01-01T00:00:00Z INFO Capture started. Data will be written to '/tmp/capture.dog'.";

        assert_eq!(capture_path_from_output(output).unwrap(), "/tmp/capture.dog");
    }

    #[test]
    fn capture_duration_is_rendered_in_go_duration_syntax() {
        assert_eq!(duration_as_go_duration(Duration::from_millis(1250)), "1250ms");
    }

    #[test]
    fn stats_output_requires_an_exact_metric_name_match() {
        let output = concat!(
            "+------------------+------+-------+-----------+\n",
            "| Metric           | Tags | Count | Last Seen |\n",
            "+------------------+------+-------+-----------+\n",
            "| replay.requests  |      | 1     |           |\n",
            "+------------------+------+-------+-----------+\n",
        );

        assert!(stats_output_contains_metric(output, "replay.requests"));
        assert!(!stats_output_contains_metric(output, "replay.request"));
    }
}
