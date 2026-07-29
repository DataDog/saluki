use std::time::Duration;

use airlock::docker;
use bollard::{
    container::LogOutput,
    exec::{CreateExecOptions, StartExecResults},
};
use futures::TryStreamExt as _;
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use serde_json::Value;
use tokio::time::sleep;

const EXPVAR_ENDPOINT: &str = "http://127.0.0.1:5000/debug/vars";
const SNAPSHOT_ATTEMPTS: usize = 30;
const SNAPSHOT_RETRY_DELAY: Duration = Duration::from_millis(200);

/// Captures the Core Agent expvar document from inside a Docker target container.
pub async fn capture_expvars(container_name: &str) -> Result<Value, GenericError> {
    let mut last_error = None;
    for _ in 0..SNAPSHOT_ATTEMPTS {
        match capture_once(container_name).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => {
                last_error = Some(error);
                sleep(SNAPSHOT_RETRY_DELAY).await;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| generic_error!("No attempts were made to capture Core Agent expvars.")))
}

async fn capture_once(container_name: &str) -> Result<Value, GenericError> {
    let docker = docker::connect().error_context("Failed to connect to Docker for expvar capture.")?;
    let exec = docker
        .create_exec(
            container_name,
            CreateExecOptions::<String> {
                cmd: Some(vec![
                    "curl".to_string(),
                    "--silent".to_string(),
                    "--show-error".to_string(),
                    "--fail".to_string(),
                    "--connect-timeout".to_string(),
                    "1".to_string(),
                    "--max-time".to_string(),
                    "5".to_string(),
                    EXPVAR_ENDPOINT.to_string(),
                ]),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..Default::default()
            },
        )
        .await
        .error_context("Failed to create expvar capture exec.")?;
    let exec_id = exec.id.clone();
    let result = docker
        .start_exec(&exec_id, None)
        .await
        .error_context("Failed to start expvar capture exec.")?;

    let mut stdout = String::new();
    let mut stderr = String::new();
    if let StartExecResults::Attached { mut output, .. } = result {
        while let Some(chunk) = output
            .try_next()
            .await
            .error_context("Failed to read expvar capture output.")?
        {
            match chunk {
                LogOutput::StdOut { message } => stdout.push_str(&String::from_utf8_lossy(&message)),
                LogOutput::StdErr { message } => stderr.push_str(&String::from_utf8_lossy(&message)),
                _ => {}
            }
        }
    }

    let inspect = docker
        .inspect_exec(&exec_id)
        .await
        .error_context("Failed to inspect expvar capture exec.")?;
    if inspect.exit_code != Some(0) {
        return Err(generic_error!(
            "Expvar capture exited {:?} in container '{}': {}",
            inspect.exit_code,
            container_name,
            stderr
        ));
    }

    decode_expvar_output(&stdout)
}

fn decode_expvar_output(output: &str) -> Result<Value, GenericError> {
    serde_json::from_str(output).error_context("Failed to decode Core Agent /debug/vars response.")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::decode_expvar_output;

    #[test]
    fn decodes_the_core_agent_expvar_document() {
        let value =
            decode_expvar_output(r#"{"dogstatsd":{"MetricPackets":12}}"#).expect("valid expvar JSON should decode");

        assert_eq!(value, json!({"dogstatsd": {"MetricPackets": 12}}));
    }

    #[test]
    fn rejects_non_json_command_output() {
        let error = decode_expvar_output("curl: connection refused")
            .expect_err("invalid command output should not become a snapshot");

        assert!(error.to_string().contains("decode Core Agent /debug/vars"));
    }
}
