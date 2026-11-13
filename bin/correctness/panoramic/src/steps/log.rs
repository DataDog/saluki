//! `log_contains` and `log_not_contains`: what a participant's processes write.
//!
//! Each line of a process's output is matched on its own, as text it must contain, or, with `regex: true`, as a regex.
//! A value a reference fills into a regex is escaped, so a path or an address matches itself.

use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::{sleep_until, Instant};

use super::{described, stopped, POLL};
use crate::core::case::Timeout;
use crate::core::handle::{Handle, HandleError};
use crate::core::record::StepStatus;
use crate::core::step::{Need, Step, StepContext, StepFactory, StepFields, StepReport};
use crate::core::template::Template;

/// The fields a reference may fill in.
const TEMPLATE_FIELDS: &[&str] = &["pattern"];

pub(super) struct LogContainsFactory;

impl StepFactory for LogContainsFactory {
    fn name(&self) -> &'static str {
        "log_contains"
    }

    fn template_fields(&self) -> &'static [&'static str] {
        TEMPLATE_FIELDS
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step: LogContains = fields.decode()?;
        check_regex(&step.pattern, step.regex)?;
        Ok(Box::new(step))
    }
}

/// Passes once a line of the process, or of any process of the participant, matches the pattern within `timeout`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogContains {
    on: String,
    #[serde(default)]
    process: Option<String>,
    pattern: Template,
    #[serde(default)]
    regex: bool,
    timeout: Timeout,
}

#[async_trait]
impl Step for LogContains {
    fn name(&self) -> &'static str {
        "log_contains"
    }

    fn needs(&self) -> Vec<Need> {
        match &self.process {
            Some(process) => vec![Need::Process(self.on.clone(), process.clone())],
            None => vec![Need::Participant(self.on.clone())],
        }
    }

    fn budget(&self) -> Duration {
        self.timeout.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let watched = described(&self.on, self.process.as_deref());
        let looked = async {
            let handle = cx.handle(&self.on)?;
            let filled = cx.fill(&self.pattern, self.regex)?;
            let pattern = Pattern::new(filled.clone(), self.regex)?;
            let mut output = Output::new(handle, self.process.as_deref())?;
            loop {
                if let Some((process, line)) = output.find(&pattern) {
                    let message = format!("Found '{}' in process '{}': {}", filled, process, line);
                    return Ok(StepReport {
                        status: StepStatus::Passed,
                        message,
                    });
                }
                if Instant::now() >= cx.deadline {
                    let message = format!(
                        "'{}' is not in the output of {} within {}.",
                        filled, watched, self.timeout.text
                    );
                    return Ok(StepReport {
                        status: StepStatus::Failed,
                        message,
                    });
                }
                sleep_until((Instant::now() + POLL).min(cx.deadline)).await;
            }
        };
        looked.await.unwrap_or_else(stopped)
    }
}

pub(super) struct LogNotContainsFactory;

impl StepFactory for LogNotContainsFactory {
    fn name(&self) -> &'static str {
        "log_not_contains"
    }

    fn template_fields(&self) -> &'static [&'static str] {
        TEMPLATE_FIELDS
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step: LogNotContains = fields.decode()?;
        check_regex(&step.pattern, step.regex)?;
        Ok(Box::new(step))
    }
}

/// Fails as soon as a line of any process of the participant, written before or during `during`, matches the pattern.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LogNotContains {
    on: String,
    pattern: Template,
    #[serde(default)]
    regex: bool,
    during: Timeout,
}

#[async_trait]
impl Step for LogNotContains {
    fn name(&self) -> &'static str {
        "log_not_contains"
    }

    fn needs(&self) -> Vec<Need> {
        vec![Need::Participant(self.on.clone())]
    }

    fn budget(&self) -> Duration {
        self.during.duration
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        // The step ends when its own duration does. A case deadline before then cuts the step short, and the driver
        // does not let it pass.
        let end = Instant::now() + self.during.duration;
        let looked = async {
            let handle = cx.handle(&self.on)?;
            let filled = cx.fill(&self.pattern, self.regex)?;
            let pattern = Pattern::new(filled.clone(), self.regex)?;
            let mut output = Output::new(handle, None)?;
            loop {
                if let Some((process, line)) = output.find(&pattern) {
                    let message = format!("Found '{}' in process '{}': {}", filled, process, line);
                    return Ok(StepReport {
                        status: StepStatus::Failed,
                        message,
                    });
                }
                if Instant::now() >= end {
                    let message = format!(
                        "'{}' is not in the output of {} during {}.",
                        filled,
                        described(&self.on, None),
                        self.during.text
                    );
                    return Ok(StepReport {
                        status: StepStatus::Passed,
                        message,
                    });
                }
                sleep_until((Instant::now() + POLL).min(end)).await;
            }
        };
        looked.await.unwrap_or_else(stopped)
    }
}

/// Checks that `pattern` is a valid regex when `regex` is set. A value a reference fills in is escaped, so only the
/// text as written can make it invalid. Each reference stands for one literal character here, so a quantifier after
/// it stays valid.
fn check_regex(pattern: &Template, regex: bool) -> Result<(), String> {
    if regex {
        let written = pattern.resolve(true, |_| Some("x".to_string()));
        Regex::new(written.text()).map_err(|e| format!("pattern '{}' is not a valid regex: {}", pattern, e))?;
    }
    Ok(())
}

/// What a line must match.
enum Pattern {
    Text(String),
    Regex(Regex),
}

impl Pattern {
    fn new(text: String, regex: bool) -> Result<Self, HandleError> {
        if !regex {
            return Ok(Self::Text(text));
        }
        Regex::new(&text)
            .map(Self::Regex)
            .map_err(|e| HandleError::Harness(format!("pattern '{}' is not a valid regex: {}", text, e)))
    }

    fn matches(&self, line: &str) -> bool {
        match self {
            Self::Text(text) => line.contains(text.as_str()),
            Self::Regex(regex) => regex.is_match(line),
        }
    }
}

/// The output of processes of one participant, with how many lines of each have been read.
struct Output<'a> {
    processes: Vec<(&'a str, watch::Receiver<Vec<String>>, usize)>,
}

impl<'a> Output<'a> {
    /// The output of `process` of `handle`'s participant, or of every process when `process` is `None`.
    fn new(handle: &Handle<'a>, process: Option<&'a str>) -> Result<Self, HandleError> {
        let names = match process {
            Some(process) => vec![process],
            None => handle.processes(),
        };
        let processes = names
            .into_iter()
            .map(|name| Ok((name, handle.output(name)?, 0)))
            .collect::<Result<_, HandleError>>()?;
        Ok(Self { processes })
    }

    /// The first line not read yet that matches `pattern`, and its process.
    fn find(&mut self, pattern: &Pattern) -> Option<(&'a str, String)> {
        for (process, output, read) in &mut self.processes {
            let lines = output.borrow();
            let found = lines[*read..].iter().find(|line| pattern.matches(line)).cloned();
            *read = lines.len();
            if let Some(line) = found {
                return Some((process, line));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::super::scripted::{observation, run, Scripted};
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn log_contains_fails_without_the_line_and_errors_for_a_process_it_cannot_read() {
        let observation = observation();
        observation.line("main", "starting".to_string());
        let fields = "{on: app, pattern: ready, timeout: 2s}";
        let report = run(&LogContainsFactory, fields, &Scripted::default(), &observation).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);

        let fields = "{on: app, process: other, pattern: ready, timeout: 2s}";
        let report = run(&LogContainsFactory, fields, &Scripted::default(), &observation).await;
        assert_eq!(report.status, StepStatus::Errored, "{}", report.message);
    }

    #[tokio::test(start_paused = true)]
    async fn log_not_contains_fails_on_the_line_and_errors_for_a_participant_it_cannot_reach() {
        let observation = observation();
        observation.line("job", "thread panicked: PANIC".to_string());
        let fields = "{on: app, pattern: 'panic|PANIC', regex: true, during: 2s}";
        let report = run(&LogNotContainsFactory, fields, &Scripted::default(), &observation).await;
        assert_eq!(report.status, StepStatus::Failed, "{}", report.message);

        let fields = "{on: other, pattern: PANIC, during: 2s}";
        let report = run(&LogNotContainsFactory, fields, &Scripted::default(), &observation).await;
        assert_eq!(report.status, StepStatus::Errored, "{}", report.message);
    }
}
