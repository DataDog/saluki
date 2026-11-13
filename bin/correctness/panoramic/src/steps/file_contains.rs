//! `file_contains`: a file in a participant holds some text.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

use super::{stopped, POLL};
use crate::core::case::Timeout;
use crate::core::handle::HandleError;
use crate::core::record::StepStatus;
use crate::core::step::{Need, Step, StepContext, StepFactory, StepFields, StepReport};
use crate::core::template::Template;

/// How long one read of the file may take.
const READ_TIME: Duration = Duration::from_secs(5);

pub(super) struct FileContainsFactory;

impl StepFactory for FileContainsFactory {
    fn name(&self) -> &'static str {
        "file_contains"
    }

    fn template_fields(&self) -> &'static [&'static str] {
        &["path"]
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(fields.decode::<FileContains>()?))
    }
}

/// Passes once the file at `path` in the participant holds `pattern` within `timeout`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileContains {
    on: String,
    path: Template,
    pattern: String,
    timeout: Timeout,
}

#[async_trait]
impl Step for FileContains {
    fn name(&self) -> &'static str {
        "file_contains"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Participant(self.on.clone())]
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let looked = async {
            let handle = cx.handle(&self.on)?;
            let path = cx.fill(&self.path, false)?;
            // What the last read that reached the participant showed, and the last read that did not.
            let mut seen = None;
            let mut failed = None;
            loop {
                let limit = READ_TIME.min(cx.deadline.saturating_duration_since(Instant::now()));
                match handle.read_file(&path, limit).await {
                    Ok(Some(contents)) if String::from_utf8_lossy(&contents).contains(&self.pattern) => {
                        let message = format!("'{}' holds '{}'.", path, self.pattern);
                        return Ok(StepReport {
                            status: StepStatus::Passed,
                            message,
                        });
                    }
                    Ok(Some(_)) => seen = Some("the file does not hold it"),
                    Ok(None) => seen = Some("there is no such file"),
                    Err(e) => failed = Some(e),
                }
                if Instant::now() >= cx.deadline {
                    break;
                }
                sleep_until((Instant::now() + POLL).min(cx.deadline)).await;
            }
            match (seen, failed) {
                (None, Some(e)) => Err(e),
                (seen, _) => {
                    let message = format!(
                        "'{}' is not in '{}' within {}: {}.",
                        self.pattern,
                        path,
                        self.timeout.text,
                        seen.unwrap_or("no read reached the participant")
                    );
                    Ok::<_, HandleError>(StepReport {
                        status: StepStatus::Failed,
                        message,
                    })
                }
            }
        };
        looked.await.unwrap_or_else(stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::super::scripted::{observation, run, Scripted};
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_file_without_the_text_fails_and_a_read_that_fails_errors() {
        let fields = "{on: app, path: /var/log/app.log, pattern: DATAPLANE, timeout: 2s}";
        let other = Scripted {
            files: [("/var/log/app.log", Ok(b"CORE started\n".to_vec()))].into(),
        };
        let report = run(&FileContainsFactory, fields, &other, &observation()).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);

        let unreadable = Scripted {
            files: [(
                "/var/log/app.log",
                Err(HandleError::Harness("Docker refused the copy".to_string())),
            )]
            .into(),
        };
        let report = run(&FileContainsFactory, fields, &unreadable, &observation()).await;
        assert_eq!(report.status, StepStatus::Errored, "{}", report.message);
    }
}
