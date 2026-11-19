//! `with: {own_ip: {env, hosts}}`: the address of the participant's own container, as a script the participant type
//! places. A case cannot write the address itself, because Docker gives it at start.
//!
//! The script sets each environment variable in `env` to the address, and gives each name in `hosts` a line in the
//! container's hosts file. In a Linux container it is an s6 init script, which runs before any service. In a Windows
//! container it is a script the entrypoint runs before it starts any program.

use std::collections::BTreeMap;

use super::datadog_agent::OwnIp;
use crate::core::participant::{Enclosure, SpecContext};
use crate::core::spec::{FileSource, FileSpec};

const LINUX_SCRIPT: &str = "/etc/cont-init.d/01-panoramic-own-ip.sh";
/// Where the entrypoint looks for the script. The entrypoint keeps this path too.
const WINDOWS_SCRIPT: &str = "C:\\adp\\panoramic-own-ip.ps1";

const WINDOWS_HOSTS_FILE: &str = "C:\\Windows\\System32\\drivers\\etc\\hosts";

/// The reserved prefix of the harness's own environment variables.
const RESERVED_PREFIX: &str = "PANORAMIC_";

/// The script file `own_ip` asks for in the participant's container, or none if the case sets no `own_ip`.
///
/// # Errors
///
/// Returns an error if the realization is not a Linux or Windows container, if `own_ip` names nothing, if a name is
/// not one the script can write as it is, or if the case's `env` also sets a variable it names.
pub(super) fn files(cx: &SpecContext<'_>, own_ip: Option<&OwnIp>) -> Result<BTreeMap<String, FileSpec>, String> {
    let Some(own_ip) = own_ip else {
        return Ok(BTreeMap::new());
    };
    let (destination, mode, text) = match cx.realization.enclosure {
        Enclosure::LinuxContainer => (LINUX_SCRIPT, Some(0o755), linux_script(own_ip)),
        Enclosure::WindowsContainer => (WINDOWS_SCRIPT, None, windows_script(own_ip)),
        _ => return Err("with.own_ip is built for containers".to_string()),
    };
    check(cx, own_ip)?;
    let file = FileSpec {
        source: FileSource::Text {
            label: "own_ip",
            text,
            template: false,
        },
        mode,
        supervises: false,
    };
    Ok(BTreeMap::from([(destination.to_string(), file)]))
}

/// Checks the names, which are pasted into a script.
fn check(cx: &SpecContext<'_>, own_ip: &OwnIp) -> Result<(), String> {
    if own_ip.env.is_empty() && own_ip.hosts.is_empty() {
        return Err("with.own_ip names no env variable and no host: set env, hosts or both".to_string());
    }
    for name in &own_ip.env {
        let mut chars = name.chars();
        let name_start = chars.next().is_some_and(|c| c.is_ascii_uppercase() || c == '_');
        if !name_start || !chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_') {
            return Err(format!(
                "with.own_ip.env names '{}', which is not a variable name in capital letters, digits and '_'",
                name
            ));
        }
        if name.starts_with(RESERVED_PREFIX) {
            return Err(format!(
                "with.own_ip.env names {}, and the {} prefix is the harness's",
                name, RESERVED_PREFIX
            ));
        }
        if cx.case.env.contains_key(name) {
            return Err(format!(
                "with.own_ip.env names {}, which env sets too: leave it out of env",
                name
            ));
        }
    }
    for host in &own_ip.hosts {
        let characters = host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        let edges = host.starts_with(['-', '.']) || host.ends_with(['-', '.']);
        if host.is_empty() || !characters || edges {
            return Err(format!(
                "with.own_ip.hosts names '{}', which is not a host name of letters, digits, '.' and '-' that neither \
                 starts nor ends with '-' or '.'",
                host
            ));
        }
    }
    Ok(())
}

fn linux_script(own_ip: &OwnIp) -> String {
    let mut lines = vec![
        "#!/bin/sh".to_string(),
        "# Written by panoramic for agent-adp's with.own_ip. s6 runs it before any service; the run scripts read the"
            .to_string(),
        "# environment directories.".to_string(),
        "set -e".to_string(),
        "ip=$(hostname -i | awk '{print $1}')".to_string(),
        "[ -n \"$ip\" ] || { echo \"with.own_ip: the container has no address of its own\" >&2; exit 1; }".to_string(),
        "mkdir -p /run/agent/env /run/adp/env".to_string(),
    ];
    for name in &own_ip.env {
        lines.push(format!("printf '%s' \"$ip\" > /run/agent/env/{name}"));
        lines.push(format!("printf '%s' \"$ip\" > /run/adp/env/{name}"));
    }
    for host in &own_ip.hosts {
        lines.push(format!("echo \"$ip {host}\" >> /etc/hosts"));
    }
    lines.push(String::new());
    lines.join("\n")
}

fn windows_script(own_ip: &OwnIp) -> String {
    let mut lines = vec![
        "# Written by panoramic for agent-adp's with.own_ip. The entrypoint runs it before the Agent and ADP start."
            .to_string(),
        "$OwnAddress = $null".to_string(),
        "foreach ($Address in [System.Net.Dns]::GetHostAddresses([System.Net.Dns]::GetHostName())) {".to_string(),
        "    if ($Address.AddressFamily -eq [System.Net.Sockets.AddressFamily]::InterNetwork -and".to_string(),
        "        -not [System.Net.IPAddress]::IsLoopback($Address)) { $OwnAddress = $Address.IPAddressToString; break }"
            .to_string(),
        "}".to_string(),
        "if (-not $OwnAddress) { throw \"with.own_ip: the container has no IPv4 address of its own.\" }".to_string(),
    ];
    for name in &own_ip.env {
        lines.push(format!(
            "[Environment]::SetEnvironmentVariable(\"{name}\", $OwnAddress)"
        ));
    }
    for host in &own_ip.hosts {
        lines.push(format!(
            "Add-Content -Path \"{WINDOWS_HOSTS_FILE}\" -Value \"$OwnAddress {host}\""
        ));
    }
    lines.push(String::new());
    lines.join("\n")
}
