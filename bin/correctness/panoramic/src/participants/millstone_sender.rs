//! `millstone-sender`: one `millstone` that sends the load its configuration describes to one target and exits.
//!
//! The case supplies `millstone.yaml` with `template: true`, and its target is a field of the `target` connection. The
//! participant type places the file and starts `millstone` on it. The case does not name an executable, an image or a
//! path: each platform runs the tools its own way, as [`super::tools`] says.
//!
//! The `millstone` participant type is for comparisons. It runs a comparison sender and a baseline sender from one
//! seed, and its connections are `comparison_target` and `baseline_target`.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use super::millstone::TARGET;
use super::tools;
use crate::core::participant::{Enclosure, Os, ParticipantType, SpecContext};
use crate::core::spec::{FileSource, ParticipantSpec};
use crate::core::template::{Reference, Template};

/// The executable that sends.
const BINARY: &str = "millstone";

/// The name a case writes for the sender's file.
const MILLSTONE_YAML: &str = "millstone.yaml";

/// Where the file goes, and how the command line names it, on each realization.
const LINUX_YAML: &str = "/etc/millstone/millstone.yaml";
const WINDOWS_YAML: &str = "C:/tools/millstone.yaml";
const WINDOWS_YAML_ARG: &str = "C:\\tools\\millstone.yaml";
const HOST_YAML: &str = "./millstone.yaml";
const HOST_YAML_ARG: &str = "{{ self.dir }}/millstone.yaml";

/// The reference prefix of the connection, as the sender's file writes it.
const TARGET_REFERENCE: &str = "self.connections.target.";

/// Whether the `target` of the sender's file is one reference to a field of the `target` connection, and nothing else.
/// A reference elsewhere in the file, such as in a comment, does not make the sender send to the connection.
fn targets_connection(text: &str) -> Result<bool, String> {
    let file: Value =
        saluki_config::yaml::from_str(text).map_err(|e| format!("{} does not parse as YAML: {}", MILLSTONE_YAML, e))?;
    let Some(target) = file.get("target").and_then(Value::as_str) else {
        return Ok(false);
    };
    let target = target.trim();
    let template = Template::parse(target)?;
    let is_connection =
        |reference: &Reference| matches!(reference, Reference::Connection { name, .. } if name == "target");
    Ok(target.starts_with("{{")
        && target.ends_with("}}")
        && template.references().all(is_connection)
        && template.references().count() == 1)
}

/// The sender takes no `with` parameters.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct With {}

pub(crate) struct MillstoneSender;

impl ParticipantType for MillstoneSender {
    fn name(&self) -> &'static str {
        "millstone-sender"
    }

    fn supports(&self) -> &'static [(Os, Enclosure)] {
        &[
            (Os::Linux, Enclosure::LinuxContainer),
            (Os::Linux, Enclosure::KubernetesPod),
            (Os::Macos, Enclosure::HostProcessGroup),
            (Os::Windows, Enclosure::WindowsContainer),
        ]
    }

    fn spec(&self, cx: &SpecContext<'_>) -> Result<ParticipantSpec, String> {
        let With {} = cx.with()?;
        let Some(text) = cx.file_text(MILLSTONE_YAML)? else {
            return Err(format!(
                "millstone-sender sends what the case's {} describes; supply it under that name",
                MILLSTONE_YAML
            ));
        };
        let is_template = matches!(
            cx.case.files.get(MILLSTONE_YAML).map(|file| &file.source),
            Some(FileSource::Case { template: true, .. })
        );
        if !is_template || !targets_connection(text)? {
            return Err(format!(
                "millstone-sender's {} must set `target` to a field of the target connection, such as {{{{ {}url }}}}, \
                 and the case must place it with template: true",
                MILLSTONE_YAML, TARGET_REFERENCE
            ));
        }
        let (destination, argument) = match cx.realization.enclosure {
            Enclosure::LinuxContainer | Enclosure::KubernetesPod => (LINUX_YAML, LINUX_YAML),
            Enclosure::WindowsContainer => (WINDOWS_YAML, WINDOWS_YAML_ARG),
            Enclosure::HostProcessGroup => (HOST_YAML, HOST_YAML_ARG),
        };
        Ok(ParticipantSpec {
            aliases: BTreeMap::from([(MILLSTONE_YAML, destination)]),
            consumes: BTreeMap::from([("target", TARGET)]),
            ..tools::launch(cx, BINARY, &[argument])?
        })
    }
}
