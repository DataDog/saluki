//! `agent-adp` in a Windows container: the Agent and ADP run under an entrypoint the harness places, which replaces the
//! base image's entrypoint.
//!
//! The entrypoint starts the Agent, then ADP, writes each one's log file and exit record, restarts nothing, and keeps
//! the container up after both exit.

use std::collections::BTreeMap;

use super::datadog_agent::{self, With};
use super::{add_endpoints, own_ip, program_command, reject_adp_keys, DOGSTATSD_UDP_STARTED, OPERATIONS};
use crate::core::participant::SpecContext;
use crate::core::spec::{Evidence, FileSource, FileSpec, ParticipantSpec};
use crate::core::template::Template;

/// The ADP-only image the Windows integration script builds.
const IMAGE: &str = "saluki-images/agent-data-plane:testing-windows";

/// Where the entrypoint is placed.
const ENTRYPOINT_PATH: &str = "C:\\adp\\panoramic-entrypoint.ps1";
const ENTRYPOINT: &[u8] = include_bytes!("../../../../../../ci/tooling/windows-adp-entrypoint-panoramic.ps1");

/// ADP's command line, with no `-c`.
const ADP_COMMAND: &str = "C:\\adp\\agent-data-plane.exe";
const AGENT_COMMAND: &str = "C:\\Program Files\\Datadog\\Datadog Agent\\bin\\agent.exe";

/// The spec in a Windows container: the Agent and ADP, started by the harness's entrypoint.
pub(super) fn spec(cx: &SpecContext<'_>) -> Result<ParticipantSpec, String> {
    let with: With = cx.with()?;
    if with.host_view {
        return Err("with.host_view mounts host paths, and a Windows container mounts none".to_string());
    }
    reject_adp_keys(cx, "In a Windows container")?;
    let ready = Evidence::Output {
        pattern: Template::parse(DOGSTATSD_UDP_STARTED)?,
        process: "adp".to_string(),
    };
    let mut files = BTreeMap::from([(
        ENTRYPOINT_PATH.to_string(),
        FileSpec {
            source: FileSource::Bytes {
                label: "windows entrypoint",
                bytes: ENTRYPOINT,
            },
            mode: None,
            supervises: true,
        },
    )]);
    files.extend(own_ip::files(cx, with.own_ip.as_ref())?);
    let mut spec = ParticipantSpec {
        image: Some(IMAGE.to_string()),
        // The entrypoint overrides the one the image bakes in, and the arguments are ADP's.
        command: Some(parse_all(&[
            "pwsh",
            "-NoProfile",
            "-NonInteractive",
            "-File",
            ENTRYPOINT_PATH,
        ])?),
        args: Some(parse_all(&["-c", "C:\\ProgramData\\Datadog\\datadog.yaml", "run"])?),
        files,
        ready: BTreeMap::from([("dogstatsd_udp".to_string(), ready)]),
        processes: Some(vec![
            datadog_agent::supervised_process("adp", "data-plane", "adp.log")?,
            datadog_agent::supervised_process("agent", "agent", "agent.log")?,
        ]),
        commands: BTreeMap::from([
            ("adp".to_string(), program_command(ADP_COMMAND)?),
            ("agent".to_string(), program_command(AGENT_COMMAND)?),
        ]),
        operations: &OPERATIONS,
        ..ParticipantSpec::default()
    };
    add_endpoints(cx, &mut spec)?;
    datadog_agent::set_defaults(cx, true, &mut spec)?;
    add_force_enable(cx, &mut spec)?;
    Ok(spec)
}

fn parse_all(texts: &[&str]) -> Result<Vec<Template>, String> {
    texts.iter().copied().map(Template::parse).collect()
}

/// Gives the Agent the flag that hands the data plane over when the case enables it.
fn add_force_enable(cx: &SpecContext<'_>, spec: &mut ParticipantSpec) -> Result<(), String> {
    // The Agent honors `data_plane.enabled` only on Linux, and on any other OS overrides it to false unless forced.
    // ADP then exits with "Agent Data Plane is not enabled."
    if cx
        .case
        .env
        .get("DD_DATA_PLANE_ENABLED")
        .is_some_and(|value| value.text() == "true")
    {
        spec.env
            .insert("DD_DATA_PLANE_FORCE_ENABLE".to_string(), Template::parse("true")?);
    }
    Ok(())
}
