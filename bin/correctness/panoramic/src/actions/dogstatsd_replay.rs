use std::time::Duration;

use saluki_error::generic_error;
use tokio::time::Instant;

use super::{
    target_exec::{execute_target_command, CommandDiagnostics, CommandError},
    Action,
};
use crate::assertions::{AssertionContext, AssertionResult, AssertionStatus};

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

    /// Reports a statistics run that failed.
    ///
    /// An earlier replay that reached ADP keeps the step `failed`, even when this error is the harness's.
    fn stats_failed(
        &self, started: Instant, error: CommandError, replay_ran: bool, last_missing_metrics: &[String],
    ) -> AssertionResult {
        let status = if replay_ran {
            AssertionStatus::Failed
        } else {
            error.status()
        };
        let mut message = format!("Failed to collect DogStatsD replay statistics: {error}");
        if !last_missing_metrics.is_empty() {
            message.push_str(&format!(
                "\nAn earlier replay's statistics did not contain expected metric(s): {}",
                last_missing_metrics.join(", ")
            ));
        }
        AssertionResult::new(self.name(), started, status, message)
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
        // Keep `ActionConfig::budget` in sync with this command and wait sequence.
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
            Err(error) => {
                return AssertionResult::new(
                    self.name(),
                    started,
                    error.status(),
                    format!("Failed to start DogStatsD capture: {error}"),
                )
            }
        };
        let capture_path = match capture_path_from_output(&capture_output) {
            Ok(path) => path,
            Err(error) => {
                return AssertionResult::new(self.name(), started, AssertionStatus::Failed, error.to_string())
            }
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
            return AssertionResult::new(
                self.name(),
                started,
                error.status(),
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
        let mut last_replay_error: Option<CommandError> = None;
        let mut last_missing_metrics = Vec::new();
        // At the replay deadline, any product observation makes the result `failed` rather than `errored`.
        let mut replay_ran = false;

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
                        replay_ran = true;
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
                    Err(error) => return self.stats_failed(started, error, replay_ran, &last_missing_metrics),
                },
                Err(error) => {
                    replay_ran |= matches!(error, CommandError::Failed(_));
                    last_replay_error = Some(error);
                }
            }

            if Instant::now() >= deadline {
                let status = if replay_ran {
                    AssertionStatus::Failed
                } else {
                    AssertionStatus::Errored
                };
                if let Some(error) = last_replay_error {
                    return AssertionResult::new(
                        self.name(),
                        started,
                        status,
                        format!("Failed to replay DogStatsD capture: {error}"),
                    );
                }
                return AssertionResult::new(
                    self.name(),
                    started,
                    status,
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
            status: AssertionStatus::Passed,
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
    use super::{CommandError, DogstatsdReplayAction};
    use crate::assertions::AssertionStatus;

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelling_during_the_replay_capture_wait_interrupts_the_replay() {
        use tokio::time::Instant;
        use tokio_util::sync::CancellationToken;

        use crate::assertions::test_support::{host_context, procedure};
        use crate::assertions::{run_assertion_steps, TargetCommand};
        use crate::deadline::{Budget, Deadline};
        use crate::record::{CaseRecord, StepStatus};

        let marker_dir = tempfile::tempdir().expect("temp dir should be creatable");
        let marker = marker_dir.path().join("sender-ran");
        let cancel = CancellationToken::new();
        let mut ctx = host_context(cancel.clone());
        // The fake ADP CLI supplies the capture path needed to reach the wait.
        ctx.adp_cli_command = TargetCommand::new(vec![
            "sh".to_string(),
            "-c".to_string(),
            "echo \"Data will be written to '/tmp/capture.dog'\"".to_string(),
            "adp".to_string(),
        ]);
        let case = procedure(&format!(
            "- action: dogstatsd_replay\n  sender: [sh, -c, \"touch {}\"]\n  capture_duration: 600s\n  \
             stats_duration_secs: 1\n  expected_metrics: []\n  timeout: 5s\n",
            marker.display()
        ));
        let case_deadline = Deadline::after(Budget::Case, Duration::from_secs(3600));

        // Allow time after the sender's marker for the action to reach its capture wait.
        let cancel_during_capture_wait = async {
            while !marker.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            cancel.cancel();
            Instant::now()
        };
        let record = CaseRecord::default();
        let driver = async {
            run_assertion_steps(&case, &ctx, case_deadline, &record).await;
            (record.steps(), Instant::now())
        };

        let ((results, finished), cancelled) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(driver, cancel_during_capture_wait)
        })
        .await
        .expect("cancellation should end the replay long before its capture wait does");

        assert!(finished.duration_since(cancelled) < Duration::from_secs(5));
        assert_eq!(results.len(), 1, "{results:?}");
        assert_eq!(results[0].name, "dogstatsd_replay");
        assert_eq!(results[0].status, StepStatus::Interrupted, "{}", results[0].message);
        assert!(
            results[0].message.starts_with("Interrupted after "),
            "{}",
            results[0].message
        );
    }

    #[test]
    fn a_statistics_harness_error_after_a_replay_keeps_the_step_failed() {
        // A harness error used to override an earlier replay's missing metrics and turn the step `errored`.
        let action = DogstatsdReplayAction::new(
            Vec::new(),
            Duration::from_secs(1),
            1,
            vec!["replay.requests".to_string()],
            Duration::from_secs(5),
        );
        let harness_error = || CommandError::Harness(saluki_error::generic_error!("Docker API failed."));

        let after_replay = action.stats_failed(
            tokio::time::Instant::now(),
            harness_error(),
            true,
            &["replay.requests".to_string()],
        );
        assert_eq!(after_replay.status, AssertionStatus::Failed, "{}", after_replay.message);
        assert_eq!(
            after_replay.message,
            "Failed to collect DogStatsD replay statistics: Docker API failed.\n\
             An earlier replay's statistics did not contain expected metric(s): replay.requests"
        );

        let before_replay = action.stats_failed(tokio::time::Instant::now(), harness_error(), false, &[]);
        assert_eq!(
            before_replay.status,
            AssertionStatus::Errored,
            "{}",
            before_replay.message
        );
        assert_eq!(
            before_replay.message,
            "Failed to collect DogStatsD replay statistics: Docker API failed."
        );
    }

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
