//! Compares telemetry from two distinct intake participants and records their decoded traffic.

use std::{io::Write as _, time::Duration};

use panoramic_core::{
    record::{AssertionStatus, StepReport},
    step::{decode_fields, HandleError, Need, Step, StepContext, StepFactory, StepFields},
};
use serde::Deserialize;
use serde_json::json;
use tokio::time::Instant;
use tracing::warn;

use self::{
    analysis::{AnalysisMode, AnalysisRunner, TracesAnalysisOptions},
    decoded::{write_decoded_records, Side},
    intake::CollectedData,
};

pub(crate) mod analysis;
pub mod decoded;
pub(crate) mod intake;

#[cfg(any(test, feature = "test-util"))]
pub use intake::test_server;

const DUMP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Compare {
    kind: AnalysisMode,
    subject: String,
    baseline: String,
    #[serde(default)]
    otlp_direct: bool,
    #[serde(default)]
    ignore_fields: Vec<String>,
    #[serde(default)]
    require_forwarded_packets: bool,
    #[serde(default)]
    required_metric_tag_keys: Vec<String>,
}

impl Compare {
    pub(crate) const NAME: &'static str = "compare";

    fn write_capture(&self, cx: &StepContext<'_>, side: Side, data: &CollectedData) {
        let write = || -> Result<(), saluki_error::GenericError> {
            let records = cx.traffic_file(&side.records_file_name())?;
            let role = match side {
                Side::Baseline => format!("baseline participant '{}'", self.baseline),
                Side::Comparison => format!("subject participant '{}'", self.subject),
            };
            let manifest = write_decoded_records(records, side, &role, "participant dump operation", data)?;
            let mut sidecar = cx.traffic_file(&side.manifest_file_name())?;
            serde_json::to_writer_pretty(&mut sidecar, &manifest)?;
            sidecar.write_all(b"\n")?;
            Ok(())
        };
        if let Err(error) = write() {
            let reason = format!("Failed to write the decoded capture: {error:#}");
            warn!(side = side.name(), %reason, "Failed to write decoded telemetry capture.");
            if let Ok(mut marker) = cx.traffic_file(&side.unavailable_file_name()) {
                let _ = marker.write_all(reason.as_bytes());
            }
        }
    }
}

#[async_trait::async_trait]
impl Step for Compare {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn needs(&self) -> Vec<Need> {
        vec![
            Need::Operation(self.subject.clone(), "dump".into()),
            Need::Operation(self.baseline.clone(), "dump".into()),
        ]
    }

    fn budget(&self) -> Duration {
        DUMP_TIMEOUT * 6
    }

    async fn run(&self, cx: &StepContext<'_>) -> StepReport {
        let started = Instant::now();
        for name in [&self.subject, &self.baseline] {
            if cx.handle(name).is_none() {
                return StepReport::new(
                    started,
                    AssertionStatus::Errored,
                    format!("participant '{name}' has not started"),
                );
            }
        }
        let baseline = cx.handle(&self.baseline).expect("both participants have started");
        let subject = cx.handle(&self.subject).expect("both participants have started");
        let (baseline, subject) = tokio::join!(
            CollectedData::collect_dumps(|kind| baseline.op("dump", json!({"kind": kind}), DUMP_TIMEOUT)),
            CollectedData::collect_dumps(|kind| subject.op("dump", json!({"kind": kind}), DUMP_TIMEOUT)),
        );
        for (side, result) in [(Side::Baseline, &baseline), (Side::Comparison, &subject)] {
            if let Ok(data) = result {
                self.write_capture(cx, side, data);
            }
        }
        let mut captures = Vec::with_capacity(2);
        for (name, result) in [(&self.baseline, baseline), (&self.subject, subject)] {
            match result {
                Ok(data) => captures.push(data),
                Err(error) => {
                    let status = match error {
                        HandleError::Harness { .. } => AssertionStatus::Errored,
                        HandleError::Product { .. } => AssertionStatus::Failed,
                    };
                    return StepReport::new(started, status, format!("Failed to collect '{name}': {error}"));
                }
            }
        }
        let subject = captures.pop().expect("subject capture was collected");
        let baseline = captures.pop().expect("baseline capture was collected");
        let options = TracesAnalysisOptions {
            otlp_direct_analysis_mode: self.otlp_direct,
            additional_span_ignore_fields: self.ignore_fields.clone(),
        };
        match AnalysisRunner::new(self.kind.clone(), baseline, subject, Some(options))
            .with_dogstatsd_forwarding_requirement(self.require_forwarded_packets)
            .with_required_metric_tag_keys(self.required_metric_tag_keys.clone())
            .run_analysis()
        {
            Ok(()) => StepReport::new(started, AssertionStatus::Passed, "Telemetry matches the baseline."),
            Err((error, details)) => {
                let mut message = format!("{error:#}");
                for detail in details {
                    message.push('\n');
                    message.push_str(&detail);
                }
                StepReport::new(started, AssertionStatus::Failed, message)
            }
        }
    }
}

pub(crate) struct CompareFactory;

impl StepFactory for CompareFactory {
    fn name(&self) -> &'static str {
        Compare::NAME
    }

    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String> {
        let step: Compare = decode_fields(fields)?;
        if step.subject == step.baseline {
            return Err("compare requires distinct subject and baseline intake participants".into());
        }
        if !matches!(step.kind, AnalysisMode::Traces) && (step.otlp_direct || !step.ignore_fields.is_empty()) {
            return Err("compare otlp_direct and ignore_fields require kind: traces".into());
        }
        if !step.required_metric_tag_keys.is_empty() && !matches!(step.kind, AnalysisMode::Metrics) {
            return Err("compare required_metric_tag_keys requires kind: metrics".into());
        }
        if step
            .required_metric_tag_keys
            .iter()
            .any(|key| key.is_empty() || key.contains(':') || key.trim() != key)
        {
            return Err("compare required_metric_tag_keys must contain nonempty tag keys without colons or surrounding whitespace".into());
        }
        Ok(Box::new(step))
    }
}

#[cfg(test)]
mod tests;
