//! Pauses a procedure for a fixed duration.

use std::time::Duration;

use async_trait::async_trait;
use panoramic_core::{
    case::CaseDuration,
    record::{AssertionStatus, StepReport},
    step::{decode_fields, Need, Step, StepContext, StepFactory, StepFields, StepKind},
};
use serde::Deserialize;
use tokio::time::{sleep_until, Instant};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Wait {
    /// Positive pause duration.
    duration: CaseDuration,
}

impl Wait {
    pub(crate) const NAME: &'static str = "wait";
}

#[async_trait]
impl Step for Wait {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        Vec::new()
    }

    fn budget(&self) -> Duration {
        self.duration.into()
    }

    async fn run(&self, _: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        sleep_until(started + self.budget()).await;
        StepReport::new(started, AssertionStatus::Passed, format!("waited {:?}", self.budget()))
    }
}

pub(crate) struct WaitFactory;

impl StepFactory for WaitFactory {
    fn name(&self) -> &'static str {
        Wait::NAME
    }

    fn kind(&self) -> StepKind {
        StepKind::Action
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        Ok(Box::new(decode_fields::<Wait>(fields)?))
    }
}

#[cfg(test)]
mod tests {
    use panoramic_core::step::test_support::run_step;
    use serde_json::Value;

    use super::*;
    use crate::test_support::valid_fields;

    #[tokio::test(start_paused = true)]
    async fn wait_passes_after_exactly_its_duration_without_a_started_participant() {
        for (duration, elapsed, message) in [
            ("30s", Duration::from_secs(30), "waited 30s"),
            ("1s500ms", Duration::from_millis(1500), "waited 1.5s"),
        ] {
            let mut fields = valid_fields(Wait::NAME);
            fields.insert("duration".into(), Value::String(duration.into()));
            let started = Instant::now();
            let report = run_step(&WaitFactory, &fields).await;
            assert_eq!(report.status, AssertionStatus::Passed);
            assert_eq!(report.duration, elapsed);
            assert_eq!(started.elapsed(), elapsed);
            assert_eq!(report.message, message);
        }
    }

    #[test]
    fn wait_requires_a_positive_duration_and_rejects_other_fields() {
        for (yaml, expected) in [
            ("{}", "missing field `duration`"),
            ("duration: 0s", "duration must be greater than zero"),
            ("duration: '30'", "has no unit"),
            ("duration: -1s", "unexpected character"),
            ("duration: 1day", "unknown duration unit"),
            ("duration: ''", "empty duration string"),
            ("duration: 30", "expected a string"),
            ("duration: 1s\non: subject", "unknown field `on`"),
            ("duration: 1s\nunknown: true", "unknown field `unknown`"),
        ] {
            let fields = serde_saphyr::from_str(yaml).unwrap();
            let message = WaitFactory.build(&fields).unwrap_err();
            assert!(message.contains(expected), "{yaml}: {message}");
        }
    }
}
