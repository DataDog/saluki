//! Checks case participants against their type declarations.

use std::{collections::BTreeSet, path::Path};

use saluki_error::{generic_error, GenericError};

use super::{
    case::Os,
    case_file::{CaseFile, ParticipantEntry},
};

/// A participant fact available before execution, with values by OS.
pub(crate) struct FactDeclaration {
    pub(crate) name: &'static str,
    pub(crate) values: &'static [(Os, &'static str)],
}

/// The names a case may use for one participant type, checked when the case loads.
pub(crate) struct ParticipantDeclaration {
    /// Registry name, as written in a participant's `type`.
    pub(crate) name: &'static str,
    /// Names a case may supply under `config.files`.
    pub(crate) config_files: &'static [&'static str],
    /// Served endpoints: connection targets and keys of `config.endpoints`.
    pub(crate) endpoints: &'static [&'static str],
    /// Consumed endpoints: keys of `connect`.
    pub(crate) consumes: &'static [&'static str],
    /// Names a case may set under `config.params`.
    pub(crate) params: &'static [&'static str],
    /// Early facts steps may reference, sorted by name.
    pub(crate) facts: &'static [FactDeclaration],
    /// Named processes steps may address with `process`, in sorted order.
    pub(crate) processes: &'static [&'static str],
}

impl ParticipantDeclaration {
    /// Looks up a declared fact's value for a participant OS.
    pub(crate) fn fact_value(&self, fact: &str, os: Os) -> Option<&'static str> {
        self.facts
            .iter()
            .find(|declaration| declaration.name == fact)?
            .values
            .iter()
            .find_map(|(provided_os, value)| (*provided_os == os).then_some(*value))
    }
}

/// Checks participant names, configuration and connections against registered types.
///
/// # Errors
///
/// Returns the first error in this order: participant names, subject count, subject OS override;
/// then, per participant in name order, registered type, file names, endpoint overrides, parameters,
/// OS overlays (reachable OS, files, parameters), and connections (consumed name, target, endpoint).
/// Map keys and overlay OSes are checked in sorted order. Every error names the case and source.
pub(super) fn check_participants(
    file: &CaseFile, subject_os: &BTreeSet<Os>, types: &[&ParticipantDeclaration], name: &str, source: &Path,
) -> Result<(), GenericError> {
    let error = |message: String| generic_error!("case '{}' in {}: {}", name, source.display(), message);
    for participant in file.participants.keys() {
        if !is_dns_label(participant) {
            return Err(error(format!(
                "participant name '{participant}' must be a DNS label: 1 to 63 lowercase letters, digits or '-', \
                 starting with a letter and not ending with '-'"
            )));
        }
        if participant == "self" {
            return Err(error(
                "participant name 'self' is reserved for references to the participant itself; choose another name"
                    .into(),
            ));
        }
    }

    let mut subjects = file.participants.iter().filter(|(_, entry)| entry.subject);
    let Some((subject, entry)) = subjects.next() else {
        return Err(error(
            "no participant is the subject; set 'subject: true' on exactly one".into(),
        ));
    };
    if let Some((other, _)) = subjects.next() {
        return Err(error(format!(
            "participants '{subject}' and '{other}' are both marked 'subject: true'; \
             exactly one participant is the subject"
        )));
    }
    if entry.os.is_some() {
        return Err(error(format!(
            "subject '{subject}' sets 'os'; the subject runs on each OS in subject_os, so remove it"
        )));
    }

    for (participant, entry) in &file.participants {
        let Some(declaration) = find_type(types, &entry.participant_type) else {
            return Err(error(format!(
                "participant '{participant}' has unknown type '{}'; registered types: {}",
                entry.participant_type,
                types
                    .iter()
                    .map(|declaration| declaration.name)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        };
        let holder = format!("participant '{participant}'");
        let participant_type = declaration.name;
        check_keys(entry.config.files.keys(), declaration.config_files, "declares", |key| {
            format!("{holder} supplies configuration file '{key}', which type '{participant_type}' does not declare")
        })
        .map_err(&error)?;
        check_keys(entry.config.endpoints.keys(), declaration.endpoints, "serves", |key| {
            format!("{holder} overrides endpoint '{key}', which type '{participant_type}' does not serve")
        })
        .map_err(&error)?;
        check_keys(entry.config.params.keys(), declaration.params, "accepts", |key| {
            format!("{holder} sets parameter '{key}', which type '{participant_type}' does not accept")
        })
        .map_err(&error)?;

        let possible_os = possible_os(entry, subject_os);
        for (os, overlay) in &entry.config.os {
            if !possible_os.contains(os) {
                return Err(error(format!(
                    "{holder} has an 'os' overlay for '{}', but runs only on {}",
                    os.name(),
                    possible_os.iter().map(|os| os.name()).collect::<Vec<_>>().join(", ")
                )));
            }
            let os = os.name();
            check_keys(overlay.files.keys(), declaration.config_files, "declares", |key| {
                format!(
                    "{holder} supplies configuration file '{key}', which type '{participant_type}' does not declare \
                     in its '{os}' overlay"
                )
            })
            .map_err(&error)?;
            check_keys(overlay.params.keys(), declaration.params, "accepts", |key| {
                format!(
                    "{holder} sets parameter '{key}', which type '{participant_type}' does not accept \
                     in its '{os}' overlay"
                )
            })
            .map_err(&error)?;
        }

        for (consumed, target) in &entry.connect {
            if !declaration.consumes.contains(&consumed.as_str()) {
                return Err(error(format!(
                    "{holder} connects '{consumed}', which type '{participant_type}' does not consume; it consumes {}",
                    declared_names(declaration.consumes)
                )));
            }
            let Some(target_entry) = file.participants.get(&target.participant) else {
                return Err(error(format!(
                    "{holder} connects '{consumed}' to unknown participant '{}'",
                    target.participant
                )));
            };
            if target.participant == *participant {
                return Err(error(format!("{holder} connects '{consumed}' to itself")));
            }
            let Some(target_type) = find_type(types, &target_entry.participant_type) else {
                // The target's own turn reports its unknown type; checking it here would break name order.
                continue;
            };
            if !target_type.endpoints.contains(&target.endpoint.as_str()) {
                return Err(error(format!(
                    "{holder} connects '{consumed}' to '{}.{}', which type '{}' does not serve; it serves {}",
                    target.participant,
                    target.endpoint,
                    target_type.name,
                    declared_names(target_type.endpoints)
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn find_type<'a>(types: &[&'a ParticipantDeclaration], name: &str) -> Option<&'a ParticipantDeclaration> {
    types.iter().copied().find(|declaration| declaration.name == name)
}

fn is_dns_label(name: &str) -> bool {
    (1..=63).contains(&name.len())
        && name.as_bytes()[0].is_ascii_lowercase()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.ends_with('-')
}

pub(super) fn possible_os(entry: &ParticipantEntry, subject_os: &BTreeSet<Os>) -> BTreeSet<Os> {
    match entry.os {
        Some(os) => BTreeSet::from([os]),
        None => subject_os.clone(),
    }
}

fn check_keys<'a>(
    mut keys: impl Iterator<Item = &'a String>, declared: &[&str], verb: &str, describe: impl FnOnce(&str) -> String,
) -> Result<(), String> {
    if let Some(key) = keys.find(|key| !declared.contains(&key.as_str())) {
        return Err(format!("{}; it {verb} {}", describe(key), declared_names(declared)));
    }
    Ok(())
}

pub(super) fn declared_names(names: &[&str]) -> String {
    if names.is_empty() {
        "none".into()
    } else {
        names.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: ParticipantDeclaration = ParticipantDeclaration {
        name: "server",
        config_files: &["settings.yaml"],
        endpoints: &["api"],
        consumes: &[],
        params: &["mode"],
        facts: &[],
        processes: &["server"],
    };
    const CLIENT: ParticipantDeclaration = ParticipantDeclaration {
        name: "client",
        config_files: &[],
        endpoints: &[],
        consumes: &["upstream"],
        params: &[],
        facts: &[],
        processes: &[],
    };
    const TYPES: &[&ParticipantDeclaration] = &[&SERVER, &CLIENT];

    fn case_yaml(participants: &str, subject_os: &str) -> String {
        format!("timeout: 1s\nsubject_os: [{subject_os}]\nparticipants:\n{participants}\nprocedure: []\n")
    }

    fn check(yaml: &str) -> Result<(), GenericError> {
        let file: CaseFile = serde_saphyr::from_str(yaml).unwrap();
        let source = Path::new("config.yaml");
        let subject_os = file.check("alpha", source).unwrap();
        check_participants(&file, &subject_os, TYPES, "alpha", source)
    }

    #[track_caller]
    fn check_error(yaml: &str) -> String {
        let message = check(yaml).unwrap_err().to_string();
        assert!(message.starts_with("case 'alpha' in config.yaml: "), "{message}");
        message
    }

    #[test]
    fn participant_names_must_be_dns_labels_that_start_with_a_letter() {
        for name in ["a", "adp", "intake-adp", "load2", &"a".repeat(63)] {
            let yaml = case_yaml(&format!("  '{name}': {{type: server, subject: true}}"), "linux");
            check(&yaml).unwrap();
        }
        for name in [
            "",
            "Adp",
            "1adp",
            "-adp",
            "adp-",
            "ad_p",
            "ad.p",
            "äd p",
            &"a".repeat(64),
        ] {
            let yaml = case_yaml(&format!("  '{name}': {{type: server, subject: true}}"), "linux");
            let message = check_error(&yaml);
            assert!(
                message.contains(&format!("participant name '{name}' must be a DNS label")),
                "{message}"
            );
            assert!(
                message.contains("starting with a letter and not ending with '-'"),
                "{message}"
            );
        }
    }

    #[test]
    fn the_participant_name_self_is_reserved() {
        let yaml = case_yaml("  self: {type: server, subject: true}", "linux");
        let message = check_error(&yaml);
        assert!(message.contains("participant name 'self' is reserved"), "{message}");
        assert!(message.contains("choose another name"), "{message}");
        check(&yaml.replace("  self:", "  itself:")).unwrap();
    }

    #[test]
    fn a_case_must_mark_exactly_one_subject() {
        let yaml = case_yaml("  b: {type: server}\n  a: {type: client}", "linux");
        let message = check_error(&yaml);
        assert!(
            message.contains("no participant is the subject; set 'subject: true' on exactly one"),
            "{message}"
        );
        check(&yaml.replace("type: server", "type: server, subject: true")).unwrap();

        for participants in [
            "  b: {type: server, subject: true}\n  a: {type: client, subject: true}",
            r#"  c: {type: server, subject: true}
  b: {type: server, subject: true}
  a: {type: client, subject: true}"#,
        ] {
            let yaml = case_yaml(participants, "linux");
            let message = check_error(&yaml);
            assert!(
                message.contains("participants 'a' and 'b' are both marked 'subject: true'"),
                "{message}"
            );
            assert!(!message.contains("'c'"), "{message}");
            check(&yaml.replace("type: server, subject: true", "type: server")).unwrap();
        }
    }

    #[test]
    fn the_subject_cannot_set_an_os() {
        let yaml = case_yaml("  subject: {type: server, subject: true, os: linux}", "linux");
        let message = check_error(&yaml);
        assert!(message.contains("subject 'subject' sets 'os'"), "{message}");
        assert!(
            message.contains("the subject runs on each OS in subject_os, so remove it"),
            "{message}"
        );
        check(&yaml.replace(", os: linux", "")).unwrap();
    }

    #[test]
    fn a_participant_type_must_be_registered() {
        let yaml = case_yaml("  subject: {type: unknown, subject: true}", "linux");
        let message = check_error(&yaml);
        assert!(
            message.contains("participant 'subject' has unknown type 'unknown'; registered types: server, client"),
            "{message}"
        );
        check(&yaml.replace("type: unknown", "type: server")).unwrap();
    }

    #[test]
    fn configuration_names_must_be_declared_by_the_participant_type() {
        for (config, expected, declared) in [
            (
                "{files: {typo: missing.yaml}}",
                "supplies configuration file 'typo'",
                "settings.yaml",
            ),
            ("{endpoints: {typo: {port: 80}}}", "overrides endpoint 'typo'", "api"),
            ("{params: {typo: fast}}", "sets parameter 'typo'", "mode"),
            (
                "{os: {linux: {files: {typo: missing.yaml}}}}",
                "supplies configuration file 'typo'",
                "settings.yaml",
            ),
            ("{os: {linux: {params: {typo: fast}}}}", "sets parameter 'typo'", "mode"),
        ] {
            let yaml = case_yaml(
                &format!("  subject: {{type: server, subject: true, config: {config}}}"),
                "linux",
            );
            let message = check_error(&yaml);
            assert!(message.contains(expected), "{message}");
            assert!(message.contains("which type 'server' does not"), "{message}");
            assert!(message.ends_with(declared), "{message}");
            assert_eq!(
                message.contains("in its 'linux' overlay"),
                config.contains("os:"),
                "{message}"
            );
            check(&yaml.replace("typo", declared)).unwrap();
        }

        for (config, expected) in [
            ("{files: {settings.yaml: missing.yaml}}", "it declares none"),
            ("{endpoints: {api: {port: 80}}}", "it serves none"),
            ("{params: {mode: fast}}", "it accepts none"),
            (
                "{os: {linux: {files: {settings.yaml: missing.yaml}}}}",
                "it declares none",
            ),
            ("{os: {linux: {params: {mode: fast}}}}", "it accepts none"),
        ] {
            let yaml = case_yaml(
                &format!("  subject: {{type: client, subject: true, config: {config}}}"),
                "linux",
            );
            let message = check_error(&yaml);
            assert!(message.contains("which type 'client' does not"), "{message}");
            assert!(message.ends_with(expected), "{message}");
            check(&yaml.replace(config, "{}")).unwrap();
        }
    }

    #[test]
    fn an_os_overlay_must_name_an_os_the_participant_runs_on() {
        for (participants, subject_os, expected_os, valid_os, invalid_os) in [
            (
                "  subject: {type: server, subject: true, config: {os: {OVERLAY: {}}}}",
                "linux, macos",
                "linux, macos",
                "macos",
                "windows",
            ),
            (
                r#"  subject: {type: server, subject: true}
  other: {type: client, os: linux, config: {os: {OVERLAY: {}}}}"#,
                "macos",
                "linux",
                "linux",
                "macos",
            ),
            (
                r#"  subject: {type: server, subject: true}
  other: {type: client, config: {os: {OVERLAY: {}}}}"#,
                "macos",
                "macos",
                "macos",
                "linux",
            ),
        ] {
            let yaml = case_yaml(participants, subject_os);
            let message = check_error(&yaml.replace("OVERLAY", invalid_os));
            assert!(
                message.contains(&format!(
                    "has an 'os' overlay for '{invalid_os}', but runs only on {expected_os}"
                )),
                "{message}"
            );
            check(&yaml.replace("OVERLAY", valid_os)).unwrap();
        }
    }

    #[test]
    fn connections_join_a_consumed_endpoint_to_an_endpoint_another_participant_serves() {
        let yaml = case_yaml(
            "  client: {type: client, subject: true, connect: {upstream: server.api}}\n  server: {type: server}",
            "linux",
        );
        for (invalid, expected) in [
            (
                yaml.replace("upstream:", "typo:"),
                "participant 'client' connects 'typo', which type 'client' does not consume; it consumes upstream",
            ),
            (
                yaml.replace("server.api", "missing.api"),
                "participant 'client' connects 'upstream' to unknown participant 'missing'",
            ),
            (
                yaml.replace("server.api", "client.api"),
                "participant 'client' connects 'upstream' to itself",
            ),
            (
                yaml.replace("server.api", "server.typo"),
                "participant 'client' connects 'upstream' to 'server.typo', \
                 which type 'server' does not serve; it serves api",
            ),
            (
                yaml.replace("type: client,", "type: server,"),
                "which type 'server' does not consume; it consumes none",
            ),
            (
                yaml.replace("type: server}", "type: client}"),
                "to 'server.api', which type 'client' does not serve; it serves none",
            ),
        ] {
            let message = check_error(&invalid);
            assert!(message.contains(expected), "{message}");
            check(&yaml).unwrap();
        }
    }

    #[test]
    fn checks_report_the_first_problem_in_a_fixed_order() {
        let mut yaml = case_yaml(
            r#"  z_bad: {type: unknown, subject: true}
  a:
    type: unknown
    subject: true
    os: linux
    config:
      files: {z-file: z, a-file: a}
      endpoints: {z-endpoint: {port: 80}, a-endpoint: {port: 80}}
      params: {z-param: true, a-param: true}
      os:
        windows: {}
        macos: {files: {mac-file: x}}
        linux:
          files: {z-overlay-file: z, a-overlay-file: a}
          params: {z-overlay-param: true, a-overlay-param: true}
    connect: {z-connect: missing.api, a-connect: missing.api}"#,
            "linux, macos",
        );
        for (bad, fix, expected) in [
            ("z_bad", "z", "participant name 'z_bad' must be a DNS label"),
            (
                "z: {type: unknown, subject: true}",
                "z: {type: unknown}",
                "participants 'a' and 'z' are both marked",
            ),
            ("    os: linux\n", "", "subject 'a' sets 'os'"),
            (
                "    type: unknown",
                "    type: server",
                "participant 'a' has unknown type",
            ),
            (", a-file: a", "", "configuration file 'a-file'"),
            ("z-file: z", "", "configuration file 'z-file'"),
            (", a-endpoint: {port: 80}", "", "overrides endpoint 'a-endpoint'"),
            ("z-endpoint: {port: 80}", "", "overrides endpoint 'z-endpoint'"),
            (", a-param: true", "", "sets parameter 'a-param'"),
            ("z-param: true", "", "sets parameter 'z-param'"),
            (", a-overlay-file: a", "", "configuration file 'a-overlay-file'"),
            ("z-overlay-file: z", "", "configuration file 'z-overlay-file'"),
            (", a-overlay-param: true", "", "sets parameter 'a-overlay-param'"),
            ("z-overlay-param: true", "", "sets parameter 'z-overlay-param'"),
            ("mac-file: x", "", "configuration file 'mac-file'"),
            ("        windows: {}\n", "", "has an 'os' overlay for 'windows'"),
            (", a-connect: missing.api", "", "connects 'a-connect'"),
            ("z-connect: missing.api", "", "connects 'z-connect'"),
            (
                "z: {type: unknown}",
                "z: {type: server}",
                "participant 'z' has unknown type",
            ),
        ] {
            let message = check_error(&yaml);
            assert!(message.contains(expected), "expected {expected}: {message}");
            assert!(yaml.contains(bad), "missing replacement: {bad}");
            yaml = yaml.replace(bad, fix);
        }
        check(&yaml).unwrap();

        let yaml = case_yaml(
            r#"  subject:
    type: server
    subject: true
    config: {os: {windows: {files: {typo: x}, params: {typo: true}}}}"#,
            "linux",
        );
        let message = check_error(&yaml);
        assert!(message.contains("has an 'os' overlay for 'windows'"), "{message}");
        check(&yaml.replace("windows: {files: {typo: x}, params: {typo: true}}", "linux: {}")).unwrap();

        let mut yaml = case_yaml(
            "  z: {type: server}\n  a: {type: client, subject: true, connect: {typo: missing.api}}",
            "linux",
        );
        for (bad, fix, expected) in [
            ("typo:", "upstream:", "does not consume"),
            ("missing.api", "a.typo", "unknown participant 'missing'"),
            ("a.typo", "z.typo", "to itself"),
            ("z.typo", "z.api", "which type 'server' does not serve"),
        ] {
            let message = check_error(&yaml);
            assert!(message.contains(expected), "{message}");
            yaml = yaml.replace(bad, fix);
        }
        check(&yaml).unwrap();
    }

    #[test]
    fn a_connection_to_an_unknown_type_preserves_participant_check_order() {
        let yaml = case_yaml(
            r#"  z: {type: unknown}
  b: {type: server, config: {params: {typo: true}}}
  a: {type: client, subject: true, connect: {upstream: z.api}}"#,
            "linux",
        );
        let message = check_error(&yaml);
        assert!(message.contains("participant 'b' sets parameter 'typo'"), "{message}");
        let yaml = yaml.replace("params: {typo: true}", "params: {}");
        let message = check_error(&yaml);
        assert!(
            message.contains("participant 'z' has unknown type 'unknown'"),
            "{message}"
        );
        check(&yaml.replace("type: unknown", "type: server")).unwrap();
    }
}
