//! Resolves the container's own address before the Agent and ADP start.

use std::collections::BTreeMap;

use panoramic_core::backend::LateFact;
use serde_json::Value;

pub(super) fn validate(env: &BTreeMap<String, String>, params: &BTreeMap<String, Value>) -> Result<(), String> {
    for (key, value) in env {
        if key.starts_with("ADP_DD_") && value.contains(LateFact::Ip.substituted_reference()) {
            return Err(format!(
                "{key} cannot reference its own ip: the image copies ADP_DD_* values unresolved; use {} instead",
                key.trim_start_matches("ADP_")
            ));
        }
    }
    if let Some(hosts) = params.get("extra_hosts") {
        let hosts = hosts
            .as_object()
            .ok_or("extra_hosts must be a map of host names to 'self'")?;
        for (host, target) in hosts {
            if host.is_empty()
                || !host
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
                || host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
            {
                return Err(format!(
                    "extra_hosts name '{host}' must be nonempty, contain only letters, digits, dots or hyphens, and not be localhost"
                ));
            }
            if target.as_str() != Some("self") {
                return Err(format!("extra_hosts '{host}' must map to the string 'self'"));
            }
        }
    }
    Ok(())
}

fn mapped_hosts<'a>(env: &BTreeMap<String, String>, params: &'a BTreeMap<String, Value>) -> Option<Vec<&'a str>> {
    let hosts = params.get("extra_hosts").and_then(Value::as_object);
    if !env
        .values()
        .any(|value| value.contains(LateFact::Ip.substituted_reference()))
        && hosts.is_none_or(|hosts| hosts.is_empty())
    {
        return None;
    }
    Some(
        hosts
            .into_iter()
            .flat_map(|hosts| hosts.keys().map(String::as_str))
            .collect(),
    )
}

/// Returns the hook script, or `None` when no environment value references the own IP and no extra host is mapped.
/// Host names are written inside single quotes without escaping, so pass only parameters that `validate` accepted.
pub(super) fn hook(env: &BTreeMap<String, String>, params: &BTreeMap<String, Value>) -> Option<Vec<u8>> {
    let hosts = mapped_hosts(env, params)?;
    let mut script = include_str!("assets/cont-init/late-substitution.sh")
        .replace("\r\n", "\n")
        .replace("@IP_REFERENCE@", LateFact::Ip.substituted_reference());
    for host in hosts {
        script.push_str(&format!("printf '%s %s\\n' \"$ip\" '{host}' >> /etc/hosts\n"));
    }
    Some(script.into_bytes())
}

/// Returns the hook script, or `None` when no environment value references the own IP and no extra host is mapped.
/// Host names are written inside single quotes without escaping, so pass only parameters that `validate` accepted.
pub(super) fn windows_hook(env: &BTreeMap<String, String>, params: &BTreeMap<String, Value>) -> Option<Vec<u8>> {
    let hosts = mapped_hosts(env, params)?;
    let mut script = include_str!("assets/windows/late-substitution.ps1")
        .replace("\r\n", "\n")
        .replace("@IP_REFERENCE@", LateFact::Ip.substituted_reference());
    for host in hosts {
        script.push_str(&format!(
            "Add-Content -Path 'C:\\Windows\\System32\\drivers\\etc\\hosts' -Value ($Ip + ' ' + '{host}')\n"
        ));
    }
    Some(script.into_bytes())
}

#[cfg(test)]
mod tests {
    use panoramic_core::{
        layout::{Os, Realization, RealizationKind},
        participant::{CaseValues, ParticipantType},
        recipe::test_support::{files, rendered_with_params},
    };
    use serde_json::json;

    use super::*;
    use crate::AgentAdp;

    const LINUX: Realization = Realization {
        os: Os::Linux,
        kind: RealizationKind::LinuxContainer,
    };

    const WINDOWS: Realization = Realization {
        os: Os::Windows,
        kind: RealizationKind::WindowsContainer,
    };

    #[test]
    fn an_own_ip_reference_in_an_adp_dd_variable_is_rejected() {
        for key in ["ADP_DD_BIND_HOST", "ADP_DD_OTHER"] {
            let config = CaseValues {
                env: BTreeMap::from([(
                    key.into(),
                    format!("before {} after", LateFact::Ip.substituted_reference()),
                )]),
                ..Default::default()
            };
            let error = AgentAdp.validate_config(LINUX, &config).unwrap_err();
            assert!(error.contains(key), "{error}");
            assert!(
                error.contains(&format!("use {} instead", key.trim_start_matches("ADP_"))),
                "{error}"
            );
        }
        let env = BTreeMap::from([
            ("DD_BIND_HOST".into(), LateFact::Ip.substituted_reference().into()),
            ("ADP_DD_OTHER".into(), "literal".into()),
        ]);
        validate(&env, &BTreeMap::new()).unwrap();
    }

    #[test]
    fn extra_hosts_accept_only_self_mapped_host_names() {
        for value in [json!({}), json!({"foo.local": "self", "Host-2.example": "self"})] {
            let config = CaseValues {
                params: BTreeMap::from([("extra_hosts".into(), value)]),
                ..Default::default()
            };
            for realization in [LINUX, WINDOWS] {
                AgentAdp.validate_config(realization, &config).unwrap();
            }
        }
        for value in [
            Value::Null,
            json!("foo.local"),
            json!(["foo.local"]),
            json!(false),
            json!(1),
            json!({"foo.local": "other"}),
            json!({"foo.local": {"target": "self"}}),
            json!({"foo.local": ["self"]}),
            json!({"foo.local": true}),
            json!({"foo.local": 1}),
            json!({"foo.local": null}),
        ] {
            let config = CaseValues {
                params: BTreeMap::from([("extra_hosts".into(), value.clone())]),
                ..Default::default()
            };
            for realization in [LINUX, WINDOWS] {
                let error = AgentAdp.validate_config(realization, &config).unwrap_err();
                assert!(
                    error.contains("extra_hosts") && error.contains("self"),
                    "{value}: {error}"
                );
            }
        }
        for host in [
            "",
            "localhost",
            "LOCALHOST.",
            "foo local",
            "foo\nlocal",
            "foo_local",
            "foo/local",
            "foo'local",
            "$(hostname)",
            "föö.local",
        ] {
            let config = CaseValues {
                params: BTreeMap::from([("extra_hosts".into(), json!({host: "self"}))]),
                ..Default::default()
            };
            for realization in [LINUX, WINDOWS] {
                let error = AgentAdp.validate_config(realization, &config).unwrap_err();
                assert!(error.contains("extra_hosts name"), "{host:?}: {error}");
            }
        }
    }

    #[test]
    fn the_late_substitution_hook_is_installed_only_when_the_environment_or_extra_hosts_need_it() {
        for (env, params, installed) in [
            (BTreeMap::new(), BTreeMap::new(), false),
            (
                BTreeMap::new(),
                BTreeMap::from([("extra_hosts".into(), json!({}))]),
                false,
            ),
            (
                BTreeMap::from([("DD_BIND_HOST".into(), "foo.local".into())]),
                BTreeMap::new(),
                false,
            ),
            (
                BTreeMap::from([("DD_BIND_HOST".into(), LateFact::Ip.substituted_reference().into())]),
                BTreeMap::new(),
                true,
            ),
            (
                BTreeMap::new(),
                BTreeMap::from([("extra_hosts".into(), json!({"foo.local": "self"}))]),
                true,
            ),
        ] {
            let config = rendered_with_params(env, params, BTreeMap::new());
            for (realization, path, mode, base_count) in [
                (LINUX, "/etc/cont-init.d/10-panoramic-late-substitution.sh", 0o755, 6),
                (WINDOWS, r"C:\adp\participant-late.ps1", 0o644, 1),
            ] {
                let recipe = AgentAdp.recipe(realization, &config).unwrap();
                let files = files(&recipe);
                let hook = files.iter().find(|(placed, _, _)| *placed == path);
                assert_eq!(hook.is_some(), installed, "{realization:?}");
                assert_eq!(files.len(), base_count + usize::from(installed));
                if let Some((_, placed_mode, script)) = hook {
                    assert_eq!(*placed_mode, mode);
                    if realization == LINUX {
                        assert!(script.starts_with(b"#!/bin/bash\n"));
                    }
                    assert!(script
                        .windows(LateFact::Ip.substituted_reference().len())
                        .any(|text| text == LateFact::Ip.substituted_reference().as_bytes()));
                }
            }
        }
    }

    #[test]
    fn the_hook_rewrites_placeholders_into_both_service_env_directories_and_appends_extra_hosts() {
        let params = BTreeMap::from([("extra_hosts".into(), json!({"foo.local": "self", "bar.local": "self"}))]);
        let script = String::from_utf8(hook(&BTreeMap::new(), &params).unwrap()).unwrap();
        for expected in [
            "ip=$(hostname -i | awk '{print $1}')",
            "if [[ -z \"$ip\" ]]; then",
            "echo 'panoramic: could not determine the participant IP address' >&2\n    exit 1",
            "placeholder='{{ self.late.ip }}'",
            "mkdir -p /run/adp/env /run/agent/env",
            "while IFS= read -r -d '' entry; do",
            "resolved=${value//\"$placeholder\"/$ip}",
            "printf '%s' \"$resolved\" > \"/run/adp/env/$key\"",
            "printf '%s' \"$resolved\" > \"/run/agent/env/$key\"",
            "done < <(env -0)",
            "printf '%s %s\\n' \"$ip\" 'bar.local' >> /etc/hosts\n",
            "printf '%s %s\\n' \"$ip\" 'foo.local' >> /etc/hosts\n",
        ] {
            assert!(script.contains(expected), "missing {expected:?}: {script}");
        }
        assert!(!script.contains("@IP_REFERENCE@"));
    }

    #[test]
    fn the_windows_hook_replaces_the_canonical_ip_reference_and_adds_one_hosts_line_per_host() {
        let params = BTreeMap::from([("extra_hosts".into(), json!({"foo.local": "self", "bar.local": "self"}))]);
        validate(&BTreeMap::new(), &params).unwrap();
        let script = String::from_utf8(windows_hook(&BTreeMap::new(), &params).unwrap()).unwrap();
        for expected in [
            "[System.Net.Dns]::GetHostAddresses([System.Net.Dns]::GetHostName())",
            "[System.Net.Sockets.AddressFamily]::InterNetwork",
            r#"$Text -ne "127.0.0.1" -and $Text -ne "0.0.0.0""#,
            r#"throw "panoramic: could not determine the participant IP address""#,
            "$Ip = Get-ContainerIPv4Address",
            "$Placeholder = '{{ self.late.ip }}'",
            "foreach ($Item in Get-ChildItem Env:)",
            "$Item.Value.Contains($Placeholder)",
            r#"Set-Item -Path "Env:\$($Item.Name)" -Value $Item.Value.Replace($Placeholder, $Ip)"#,
            r"Add-Content -Path 'C:\Windows\System32\drivers\etc\hosts' -Value ($Ip + ' ' + 'bar.local')",
            r"Add-Content -Path 'C:\Windows\System32\drivers\etc\hosts' -Value ($Ip + ' ' + 'foo.local')",
        ] {
            assert!(script.contains(expected), "missing {expected:?}: {script}");
        }
        assert_eq!(script.matches("Add-Content").count(), 2);
        assert!(!script.contains("@IP_REFERENCE@"));
    }
}
