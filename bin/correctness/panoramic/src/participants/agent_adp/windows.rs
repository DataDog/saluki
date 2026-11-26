//! `agent-adp` in a Windows container: the Agent and ADP run under an entrypoint the harness places, which replaces the
//! base image's entrypoint.
//!
//! The entrypoint starts the Agent, then ADP, writes each one's log file and exit record, restarts nothing, and keeps
//! the container up after both exit.

use std::collections::BTreeMap;

use super::datadog_agent::{self, With, INTAKE};
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

/// The flat name of each `DD_DATA_PLANE_` setting a case writes, and the nested name added beside it.
///
/// ADP reads the flat names, such as `DD_DATA_PLANE_API_LISTEN_ADDRESS` and `DD_DATA_PLANE_ENABLED`, and they stay set.
/// Its environment reader binds no nested name, which has a double underscore between path levels. The table copies the
/// legacy runner's `WINDOWS_ENV_ALIASES`, and lists only the names a case uses.
const DATA_PLANE_NESTED_NAMES: &[(&str, &str)] = &[
    ("DD_DATA_PLANE_ENABLED", "DD_DATA_PLANE__ENABLED"),
    ("DD_DATA_PLANE_STANDALONE_MODE", "DD_DATA_PLANE__STANDALONE_MODE"),
    (
        "DD_DATA_PLANE_USE_NEW_CONFIG_STREAM_ENDPOINT",
        "DD_DATA_PLANE__USE_NEW_CONFIG_STREAM_ENDPOINT",
    ),
    (
        "DD_DATA_PLANE_REMOTE_AGENT_ENABLED",
        "DD_DATA_PLANE__REMOTE_AGENT_ENABLED",
    ),
    ("DD_DATA_PLANE_DOGSTATSD_ENABLED", "DD_DATA_PLANE__DOGSTATSD__ENABLED"),
    ("DD_DATA_PLANE_OTLP_ENABLED", "DD_DATA_PLANE__OTLP__ENABLED"),
    (
        "DD_DATA_PLANE_EXPERIMENTAL_OTLP_PROXY_TRACES",
        "DD_DATA_PLANE__EXPERIMENTAL__OTLP__PROXY__TRACES",
    ),
    (
        "DD_DATA_PLANE_EXPERIMENTAL_OTLP_PROXY_METRICS",
        "DD_DATA_PLANE__EXPERIMENTAL__OTLP__PROXY__METRICS",
    ),
    (
        "DD_DATA_PLANE_EXPERIMENTAL_OTLP_PROXY_LOGS",
        "DD_DATA_PLANE__EXPERIMENTAL__OTLP__PROXY__LOGS",
    ),
    ("DD_DATA_PLANE_LOG_FILE", "DD_DATA_PLANE__LOG_FILE"),
];

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
        consumes: BTreeMap::from([("intake", INTAKE)]),
        commands: BTreeMap::from([
            ("adp".to_string(), program_command(ADP_COMMAND)?),
            ("agent".to_string(), program_command(AGENT_COMMAND)?),
        ]),
        operations: &OPERATIONS,
        ..ParticipantSpec::default()
    };
    add_endpoints(cx, &mut spec)?;
    datadog_agent::set_defaults(cx, true, &mut spec)?;
    add_nested_names(cx, &mut spec)?;
    Ok(spec)
}

fn parse_all(texts: &[&str]) -> Result<Vec<Template>, String> {
    texts.iter().copied().map(Template::parse).collect()
}

/// Gives ADP each nested name of a flat setting the case writes and not the nested name, and the Agent the flag that
/// hands the data plane over when the case enables it.
fn add_nested_names(cx: &SpecContext<'_>, spec: &mut ParticipantSpec) -> Result<(), String> {
    for (flat, nested) in DATA_PLANE_NESTED_NAMES {
        if let Some(template) = cx.case.env.get(*flat).filter(|_| !cx.case.env.contains_key(*nested)) {
            spec.env.insert(nested.to_string(), template.clone());
        }
    }
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
