//! Walks participant configuration strings and checks their reference syntax and names at load,
//! including every OS overlay.

use std::{collections::BTreeMap, fmt};

use serde_json::Value;

use super::{
    file::{CaseFile, ParticipantEntry},
    participant_check::declared_names,
    template::{ParticipantRef, Reference, Template},
};

#[derive(Clone, Copy)]
pub(crate) enum ConfigField<'a> {
    Env(&'a str),
    Param(&'a str),
}

impl ConfigField<'_> {
    fn in_config(self, prefix: &str) -> String {
        match self {
            Self::Env(key) => format!("{prefix}.env '{key}'"),
            Self::Param(key) => format!("{prefix}.params '{key}'"),
        }
    }
}

impl fmt::Display for ConfigField<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(key) => write!(f, "environment variable '{key}'"),
            Self::Param(key) => write!(f, "parameter '{key}'"),
        }
    }
}

/// Visits env values, then parameter strings depth-first, with map keys in sorted order.
/// Nested parameter strings retain their top-level key for diagnostics.
pub(crate) fn walk_config_strings(
    env: &BTreeMap<String, String>, params: &BTreeMap<String, Value>,
    mut visit: impl FnMut(ConfigField<'_>, &str) -> Result<(), String>,
) -> Result<(), String> {
    fn walk_param(
        key: &str, value: &Value, visit: &mut impl FnMut(ConfigField<'_>, &str) -> Result<(), String>,
    ) -> Result<(), String> {
        match value {
            Value::String(text) => visit(ConfigField::Param(key), text)?,
            Value::Array(values) => {
                for value in values {
                    walk_param(key, value, visit)?;
                }
            }
            Value::Object(values) => {
                for (_, value) in values.iter().collect::<BTreeMap<_, _>>() {
                    walk_param(key, value, visit)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    for (key, value) in env {
        visit(ConfigField::Env(key), value)?;
    }
    for (key, value) in params {
        walk_param(key, value, &mut visit)?;
    }
    Ok(())
}

/// Visits mutable strings in the same field order as [`walk_config_strings`].
pub(crate) fn walk_config_strings_mut(
    env: &mut BTreeMap<String, String>, params: &mut BTreeMap<String, Value>,
    mut visit: impl FnMut(ConfigField<'_>, &mut String) -> Result<(), String>,
) -> Result<(), String> {
    fn walk_param(
        key: &str, value: &mut Value, visit: &mut impl FnMut(ConfigField<'_>, &mut String) -> Result<(), String>,
    ) -> Result<(), String> {
        match value {
            Value::String(text) => visit(ConfigField::Param(key), text)?,
            Value::Array(values) => {
                for value in values {
                    walk_param(key, value, visit)?;
                }
            }
            Value::Object(values) => {
                for (_, value) in values.iter_mut().collect::<BTreeMap<_, _>>() {
                    walk_param(key, value, visit)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    for (key, value) in env {
        visit(ConfigField::Env(key), value)?;
    }
    for (key, value) in params {
        walk_param(key, value, &mut visit)?;
    }
    Ok(())
}

pub(super) fn check_config_references(
    file: &CaseFile, participant: &str, entry: &ParticipantEntry,
) -> Result<(), String> {
    let holder = format!("participant '{participant}'");
    let check = |env: &BTreeMap<String, String>, params: &BTreeMap<String, Value>, prefix: &str| {
        for key in env.keys() {
            if key.contains("{{") {
                return Err(format!(
                    "{holder} {prefix}.env '{key}': environment variable names can't contain template references"
                ));
            }
        }
        walk_config_strings(env, params, |field, text| {
            let field = format!("{holder} {}", field.in_config(prefix));
            let template: Template = text.parse().map_err(|reason| format!("{field} {reason}"))?;
            for (written, reference) in template.written_references() {
                let (target, referenced) = match reference {
                    Reference::Fact { participant, fact } => (participant, format!("fact '{fact}'")),
                    Reference::Late { participant, fact } => (participant, format!("late fact '{}'", fact.name())),
                    Reference::Endpoint {
                        participant, endpoint, ..
                    } => (participant, format!("endpoint '{endpoint}'")),
                    Reference::Connection { consumed, .. } => {
                        if !entry.connect.contains_key(consumed) {
                            return Err(format!(
                                "{field} '{written}' references connection '{consumed}', which it does not connect; it connects {}",
                                declared_names(&entry.connect.keys().map(String::as_str).collect::<Vec<_>>()),
                            ));
                        }
                        continue;
                    }
                };
                if let ParticipantRef::Named(target) = target {
                    if !file.participants.contains_key(target) {
                        return Err(format!(
                            "{field} '{written}' references {referenced} of unknown participant '{target}'; participants: {}",
                            declared_names(&file.participants.keys().map(String::as_str).collect::<Vec<_>>()),
                        ));
                    }
                }
            }
            Ok(())
        })
    };
    check(&entry.config.env, &entry.config.params, "config")?;
    for (os, overlay) in &entry.config.os {
        check(&overlay.env, &overlay.params, &format!("config.os.{}", os.name()))?;
    }
    Ok(())
}

pub(super) fn check_param_keys(params: &BTreeMap<String, Value>, field: &str) -> Result<(), String> {
    fn nested_key(value: &Value) -> Option<&str> {
        match value {
            Value::Object(values) => values
                .iter()
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .find_map(|(key, value)| {
                    if key.contains("{{") {
                        Some(key.as_str())
                    } else {
                        nested_key(value)
                    }
                }),
            Value::Array(values) => values.iter().find_map(nested_key),
            _ => None,
        }
    }

    for (key, value) in params {
        if key.contains("{{") {
            return Err(format!(
                "{field} '{key}': parameter keys can't contain template references"
            ));
        }
        if let Some(nested) = nested_key(value) {
            return Err(format!(
                "{field} '{key}' key '{nested}': parameter keys can't contain template references"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::case::{fixtures::LOADER, CaseSchema};

    fn case(config: &str) -> String {
        format!("timeout: 1s\nsubject_os: [linux, macos, windows]\nparticipants:\n  subject: {{type: server, subject: true, config: {config}, connect: {{link: other.api}}}}\n  other: {{type: server}}\nprocedure: []\n")
    }

    #[track_caller]
    fn error(config: &str, expected: &str) {
        assert_eq!(
            LOADER
                .load_content(&case(config), "alpha", Path::new("alpha/config.yaml"))
                .err()
                .unwrap()
                .to_string(),
            format!("case 'alpha' in alpha/config.yaml: participant 'subject' {expected}")
        );
    }

    #[test]
    fn every_reference_form_loads_in_env_values_and_parameter_strings_of_the_base_and_overlays() {
        let mut references = Vec::new();
        for name in ["self", "subject", "other"] {
            references.push(format!("{{{{ {name}.facts.file }}}}"));
            for fact in ["ip", "container_id", "pod_uid"] {
                references.push(format!("{{{{ {name}.late.{fact} }}}}"));
            }
            for part in ["url", "host", "port", "path"] {
                references.push(format!("{{{{ {name}.endpoints.api.{part} }}}}"));
            }
        }
        for part in ["url", "host", "port", "path"] {
            references.push(format!("{{{{ self.connections.link.{part} }}}}"));
        }
        for reference in references {
            let values = format!(
                "{{env: {{KEY: 'prefix/{reference}'}}, params: {{mode: [true, false, 3, 1.5, {{nested: [null, '{reference}']}}]}}}}"
            );
            for config in [
                values.clone(),
                format!("{{os: {{linux: {values}, macos: {values}, windows: {values}}}}}"),
            ] {
                let cases = LOADER
                    .load_content(&case(&config), "alpha", Path::new("alpha/config.yaml"))
                    .unwrap();
                assert_eq!(cases.len(), 1);
                assert_eq!(cases[0].name(), "alpha");
                let CaseSchema::Participants { file, .. } = cases[0].schema() else {
                    panic!("expected participants");
                };
                let config = &file.participants["subject"].config;
                let expected_env = std::collections::BTreeMap::from([("KEY".into(), format!("prefix/{reference}"))]);
                let expected_params = std::collections::BTreeMap::from([(
                    "mode".into(),
                    serde_json::json!([true, false, 3, 1.5, {"nested": [null, reference]}]),
                )]);
                if config.os.is_empty() {
                    assert_eq!(config.env, expected_env);
                    assert_eq!(config.params, expected_params);
                } else {
                    assert_eq!(config.os.len(), 3);
                    for overlay in config.os.values() {
                        assert_eq!(overlay.env, expected_env);
                        assert_eq!(overlay.params, expected_params);
                    }
                }
            }
        }
    }

    #[test]
    fn environment_variable_names_cannot_contain_template_references_in_the_base_or_an_overlay() {
        for prefix in ["config", "config.os.linux", "config.os.macos", "config.os.windows"] {
            let values = "{env: {'{{name}}': literal}}";
            let config = prefix
                .strip_prefix("config.os.")
                .map_or(values.into(), |os| format!("{{os: {{{os}: {values}}}}}"));
            error(
                &config,
                &format!("{prefix}.env '{{{{name}}}}': environment variable names can't contain template references"),
            );
        }
    }

    #[test]
    fn parameter_keys_cannot_contain_template_references_at_any_depth() {
        for (params, field) in [
            ("{'{{name}}': true}", "'{{name}}'"),
            ("{mode: {'{{name}}': 3}}", "'mode' key '{{name}}'"),
            (
                "{mode: [null, {nested: [{'{{name}}': false}]}]}",
                "'mode' key '{{name}}'",
            ),
        ] {
            for (config, prefix) in [
                (format!("{{params: {params}}}"), "config"),
                (
                    format!("{{os: {{windows: {{params: {params}}}}}}}"),
                    "config.os.windows",
                ),
            ] {
                error(
                    &config,
                    &format!("{prefix}.params {field}: parameter keys can't contain template references"),
                );
            }
        }
    }

    #[test]
    fn parameter_key_syntax_is_checked_before_declarations_and_connection_targets() {
        for (config, prefix) in [
            (
                "{env: {KEY: '{{ unclosed'}, params: {unknown: true, mode: {'{{name}}': false}}}",
                "config",
            ),
            (
                "{env: {KEY: '{{ unclosed'}, os: {windows: {params: {unknown: true, mode: {'{{name}}': false}}}}}",
                "config.os.windows",
            ),
        ] {
            let yaml = case(config).replace("other.api", "missing.api");
            assert_eq!(LOADER.load_content(&yaml, "alpha", Path::new("alpha/config.yaml")).err().unwrap().to_string(),
                format!("case 'alpha' in alpha/config.yaml: participant 'subject' {prefix}.params 'mode' key '{{{{name}}}}': parameter keys can't contain template references"));
        }
    }

    #[test]
    fn malformed_references_in_env_values_and_nested_parameter_strings_fail_naming_the_field() {
        for (text, reason) in [
            (
                "prefix/{{ unclosed",
                "unterminated template reference '{{ unclosed': expected '}}'",
            ),
            (
                "{{self.late.unknown}}",
                "'{{self.late.unknown}}': a late fact is container_id, ip or pod_uid",
            ),
            (
                "{{self.endpoints.api.bad}}",
                "'{{self.endpoints.api.bad}}': an address part is url, host, port or path",
            ),
        ] {
            for (field, values) in [
                ("env 'KEY'", format!("{{env: {{KEY: '{text}'}}}}")),
                (
                    "params 'mode'",
                    format!("{{params: {{mode: [false, {{nested: ['{text}']}}]}}}}"),
                ),
            ] {
                for (config, prefix) in [
                    (values.clone(), "config"),
                    (format!("{{os: {{windows: {values}}}}}"), "config.os.windows"),
                ] {
                    error(&config, &format!("{prefix}.{field} {reason}"));
                }
            }
        }
    }

    #[test]
    fn config_references_must_name_declared_participants() {
        for (reference, reason) in [
            ("{{ missing.facts.file }}", "fact 'file'"),
            ("{{missing.late.ip}}", "late fact 'ip'"),
            ("{{  missing.endpoints.api.url  }}", "endpoint 'api'"),
        ] {
            for (field, values) in [
                ("env 'KEY'", format!("{{env: {{KEY: '{reference}'}}}}")),
                (
                    "params 'mode'",
                    format!("{{params: {{mode: {{nested: ['{reference}']}}}}}}"),
                ),
            ] {
                for (config, prefix) in [
                    (values.clone(), "config"),
                    (format!("{{os: {{linux: {values}}}}}"), "config.os.linux"),
                ] {
                    error(&config, &format!("{prefix}.{field} '{reference}' references {reason} of unknown participant 'missing'; participants: other, subject"));
                }
            }
        }
    }

    #[test]
    fn connection_references_must_name_a_connect_key_of_the_holder() {
        error("{env: {KEY: '{{ self.connections.typo.url }}'}}",
            "config.env 'KEY' '{{ self.connections.typo.url }}' references connection 'typo', which it does not connect; it connects link");
        error("{os: {windows: {params: {mode: ['{{self.connections.typo.path}}']}}}}",
            "config.os.windows.params 'mode' '{{self.connections.typo.path}}' references connection 'typo', which it does not connect; it connects link");
        let yaml =
            case("{env: {KEY: '{{self.connections.link.url}}'}}").replace("connect: {link: other.api}", "connect: {}");
        assert_eq!(LOADER.load_content(&yaml, "alpha", Path::new("alpha/config.yaml")).err().unwrap().to_string(),
            "case 'alpha' in alpha/config.yaml: participant 'subject' config.env 'KEY' '{{self.connections.link.url}}' references connection 'link', which it does not connect; it connects none");
    }
}
