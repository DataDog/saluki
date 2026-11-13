//! `millstone`: the load generator, as two senders in one container that send the same load, one to the comparison
//! and one to the baseline.
//!
//! The case supplies one `millstone.yaml`, whose target refers to the `comparison_target` connection. The baseline
//! sender's file is that text with the connection renamed to `baseline_target`, built before anything resolves, so
//! both senders run from one seed and send byte-identical payloads. A script the participant type places runs both
//! senders, and writes each one's output file and exit record.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::core::participant::{Enclosure, Os, ParticipantType, SpecContext};
use crate::core::spec::{
    CollectFile, Consumed, ExitChannel, FileSource, FileSpec, Keep, OutputChannel, ParticipantSpec, ProcessSpec,
    Protocol,
};
use crate::core::template::Template;

/// The tools image the `Makefile` builds.
const IMAGE: &str = "saluki-images/correctness-tools:latest";

/// The name a case writes for the comparison sender's file, and its destination.
const MILLSTONE_YAML: &str = "millstone.yaml";
const COMPARISON_YAML: &str = "/etc/millstone/comparison.yaml";
/// Where the baseline sender's file goes. `senders.sh` names both.
const BASELINE_YAML: &str = "/etc/millstone/baseline.yaml";

const SENDERS: &str = "/etc/millstone/senders.sh";
/// The script that runs both senders. A carriage return breaks it, so a test checks its line endings.
pub(crate) const SENDERS_SCRIPT: &[u8] = include_bytes!("millstone/senders.sh");

/// Where the baseline sender writes its input capture, and the files it writes there.
const CAPTURE_DIR: &str = "/var/lib/millstone/baseline";
const CAPTURE_FILES: [&str; 2] = ["input.jsonl.zst", "input-manifest.json"];

/// The two senders' connections. A sender's file refers to its own, so neither delivers anything.
const TARGET: Consumed = Consumed {
    accepts: &[
        Protocol::Tcp,
        Protocol::Udp,
        Protocol::Http,
        Protocol::Grpc,
        Protocol::UnixDatagram,
        Protocol::UnixStream,
    ],
    required: true,
    env: &[],
};

/// The reference prefix of each connection, as the senders' files write it.
const COMPARISON_TARGET: &str = "self.connections.comparison_target.";
const BASELINE_TARGET: &str = "self.connections.baseline_target.";

/// Millstone takes no `with` parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct With {}

pub(crate) struct Millstone;

impl ParticipantType for Millstone {
    fn name(&self) -> &'static str {
        "millstone"
    }

    fn supports(&self) -> &'static [(Os, Enclosure)] {
        &[(Os::Linux, Enclosure::LinuxContainer)]
    }

    fn spec(&self, cx: &SpecContext<'_>) -> Result<ParticipantSpec, String> {
        let With {} = cx.with()?;
        let Some(text) = cx.file_text(MILLSTONE_YAML)? else {
            return Err(format!(
                "millstone sends what the case's {} describes; supply it under that name",
                MILLSTONE_YAML
            ));
        };
        let template = matches!(
            cx.case.files.get(MILLSTONE_YAML).map(|file| &file.source),
            Some(FileSource::Case { template: true, .. })
        );
        if !text.contains(COMPARISON_TARGET) {
            return Err(format!(
                "millstone's {} must send to a field of the comparison_target connection, such as {{{{ {}url }}}}, so \
                 the baseline sender can send to baseline_target instead",
                MILLSTONE_YAML, COMPARISON_TARGET
            ));
        }
        let baseline = FileSpec {
            source: FileSource::Text {
                label: "the case's millstone.yaml, sending to baseline_target",
                text: text.replace(COMPARISON_TARGET, BASELINE_TARGET),
                template,
            },
            mode: None,
            supervises: false,
        };
        let senders = FileSpec {
            source: FileSource::Bytes {
                label: "senders",
                bytes: SENDERS_SCRIPT,
            },
            mode: Some(0o755),
            supervises: true,
        };
        let sender = |name: &str| {
            ProcessSpec::new(
                name,
                ExitChannel::Record { file: name.to_string() },
                OutputChannel::File {
                    file: format!("{}.log", name),
                },
            )
        };
        Ok(ParticipantSpec {
            image: Some(IMAGE.to_string()),
            command: Some(vec![Template::parse("/bin/sh")?, Template::parse(SENDERS)?]),
            files: BTreeMap::from([(BASELINE_YAML.to_string(), baseline), (SENDERS.to_string(), senders)]),
            processes: Some(vec![sender("comparison")?, sender("baseline")?]),
            collect: Some(
                CAPTURE_FILES
                    .iter()
                    .map(|name| CollectFile {
                        path: format!("{}/{}", CAPTURE_DIR, name),
                        name: name.to_string(),
                        keep: Keep::OnFailure,
                    })
                    .collect(),
            ),
            aliases: BTreeMap::from([(MILLSTONE_YAML, COMPARISON_YAML)]),
            consumes: BTreeMap::from([("comparison_target", TARGET), ("baseline_target", TARGET)]),
            ..ParticipantSpec::default()
        })
    }
}
