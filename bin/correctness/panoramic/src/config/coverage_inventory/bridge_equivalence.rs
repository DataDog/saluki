//! Checks that participant-schema twins built from the checked-in logging-case inventory reproduce its rows and
//! configurations through the integration runner translation.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use serde_json::{json, Value};

use super::{read_text, repository_root, Inventory, INTEGRATION, JSON_PATH, PARTICIPANTS};
use crate::{
    config::{
        case::{CaseConfig, Os},
        tests::write_case,
        ContainerConfig, IntakeConfig,
    },
    participants::AGENT_ADP,
};

const LOGGING_CASES: [&str; 7] = [
    "adp-logging-default-path",
    "adp-logging-ignores-core-agent-log-file",
    "adp-logging-mac-ignores-core-agent-log-file",
    "adp-logging-respects-data-plane-log-file",
    "adp-logging-windows-default-path",
    "adp-logging-windows-ignores-core-agent-log-file",
    "adp-logging-windows-respects-data-plane-log-file",
];

/// Cases in the translation's subset whose log text serves as evidence of process exit.
const LOG_PARSED_EXIT_CASES: [&str; 2] = ["adp-disabled-exit", "adp-no-pipelines-exit"];

/// Converts a recorded integration configuration's log/file assertions to participant-schema text.
///
/// Returns the text and whether a file path uses `{{ adp.facts.default_log_file }}`. The sole deliberately dropped
/// setting is `container.exposed_ports`: the translation uses default container settings, and only `port_listening`
/// reads a published port's mapping.
fn participant_twin(config: &Value) -> Result<(String, bool), String> {
    let config = config.as_object().ok_or("configuration must be an object")?;
    for key in config.keys() {
        if ![
            "name",
            "description",
            "timeout",
            "runtimes",
            "env",
            "container",
            "intake",
            "procedure",
        ]
        .contains(&key.as_str())
        {
            return Err(format!("unsupported configuration field '{key}'"));
        }
    }
    let mut container = config["container"].clone();
    container
        .as_object_mut()
        .ok_or("container must be an object")?
        .remove("exposed_ports");
    let mut default_container = serde_json::to_value(ContainerConfig::default()).unwrap();
    default_container.as_object_mut().unwrap().remove("exposed_ports");
    if container != default_container {
        return Err("container settings other than exposed_ports must be default".into());
    }
    if config["intake"] != serde_json::to_value(IntakeConfig::default()).unwrap() {
        return Err("intake settings must be default".into());
    }
    let subject_os = config["runtimes"]
        .as_array()
        .ok_or("runtimes must be an array")?
        .iter()
        .map(|runtime| {
            runtime
                .as_str()
                .and_then(Os::from_runtime)
                .ok_or_else(|| format!("unsupported runtime {runtime}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if subject_os.is_empty() {
        return Err("runtimes must not be empty".into());
    }

    let mut used_fact = false;
    let mut translate = |assertion: &Value| -> Result<Value, String> {
        let mut fields = assertion.as_object().ok_or("assertion must be an object")?.clone();
        if fields.contains_key("action") {
            return Err("actions have no participant twin".into());
        }
        let kind = fields.remove("assertion").ok_or("missing assertion kind")?;
        let allowed = match kind.as_str() {
            Some("log_contains") => &["pattern", "regex", "timeout", "stream"][..],
            Some("log_not_contains") => &["pattern", "regex", "during", "stream"][..],
            Some("file_contains") => &["pattern", "regex", "timeout", "path"][..],
            _ => return Err(format!("unsupported assertion {kind}")),
        };
        for key in fields.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(format!("unsupported field '{key}' in {kind}"));
            }
        }
        if let Some(stream) = fields.remove("stream") {
            if stream != "both" {
                return Err(format!("unsupported log stream {stream}"));
            }
        }
        if kind == "file_contains" {
            if fields.get("pattern").is_none_or(Value::is_null) {
                return Err("file_contains requires a pattern".into());
            }
            let path = fields
                .get("path")
                .and_then(Value::as_str)
                .ok_or("file path must be a string")?;
            if subject_os
                .iter()
                .all(|os| AGENT_ADP.fact_value("default_log_file", *os) == Some(path))
            {
                fields.insert("path".into(), json!("{{ adp.facts.default_log_file }}"));
                used_fact = true;
            }
        }
        fields.insert("step".into(), kind);
        fields.insert("on".into(), json!("adp"));
        Ok(Value::Object(fields))
    };
    let procedure = config["procedure"]
        .as_array()
        .ok_or("procedure must be an array")?
        .iter()
        .map(|node| {
            if let Some(parallel) = node.get("parallel") {
                if node.as_object().unwrap().len() != 1 {
                    return Err("parallel block has extra fields".into());
                }
                let parallel = parallel
                    .as_array()
                    .ok_or("parallel must be an array")?
                    .iter()
                    .map(&mut translate)
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(json!({"parallel": parallel}))
            } else {
                translate(node)
            }
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut twin = json!({
        "name": config["name"],
        "timeout": config["timeout"],
        "subject_os": subject_os.iter().map(|os| os.name()).collect::<Vec<_>>(),
        "participants": {"adp": {"type": "agent-adp", "subject": true, "config": {"env": config["env"]}}},
        "procedure": procedure,
    });
    if let Some(description) = config.get("description").filter(|value| !value.is_null()) {
        if !description.is_string() {
            return Err("description must be a string or null".into());
        }
        twin["description"] = description.clone();
    }
    Ok((serde_json::to_string(&twin).unwrap(), used_fact))
}

/// A checked-in case configuration, its subject-OS rows, and whether all rows use the integration schema.
struct RecordedCase {
    rows: Vec<Value>,
    case: Value,
    old_schema: bool,
}

/// Reads integration cases from the checked-in inventory, requiring known, uniform schema labels and named fixtures.
fn recorded_integration_cases() -> BTreeMap<String, RecordedCase> {
    let inventory: Value = serde_json::from_str(&read_text(&repository_root().join(JSON_PATH))).unwrap();
    let recorded: BTreeMap<_, _> = inventory["cases"]
        .as_object()
        .unwrap()
        .iter()
        .filter_map(|(key, case)| {
            let name = key.strip_prefix("integration/")?;
            let rows: Vec<_> = inventory["rows"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|row| row["suite"] == INTEGRATION && row["name"] == name)
                .cloned()
                .collect();
            assert!(!rows.is_empty(), "missing integration rows for {name}");
            for row in &rows {
                assert!(
                    row["schema"] == INTEGRATION || row["schema"] == PARTICIPANTS,
                    "{name} has an unexpected schema label: {}",
                    row["schema"]
                );
            }
            let old_schema = rows.iter().all(|row| row["schema"] == INTEGRATION);
            assert!(
                old_schema || rows.iter().all(|row| row["schema"] == PARTICIPANTS),
                "{name} has mixed schema labels across its subject-OS rows"
            );
            Some((
                name.to_string(),
                RecordedCase {
                    rows,
                    case: case.clone(),
                    old_schema,
                },
            ))
        })
        .collect();
    for name in LOGGING_CASES.into_iter().chain(LOG_PARSED_EXIT_CASES) {
        assert!(recorded.contains_key(name), "{name} is not in the checked-in inventory");
    }
    recorded
}

#[test]
fn logging_case_twins_reproduce_the_recorded_inventory_except_schema_and_exposed_ports() {
    let recorded = recorded_integration_cases();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::create_dir_all(root.join("test/integration/cases")).unwrap();
    std::fs::create_dir_all(root.join("test/correctness/cases")).unwrap();
    let mut expected_rows = Vec::new();
    let mut expected_cases = serde_json::Map::new();
    for name in LOGGING_CASES {
        let recorded = &recorded[name];
        if !recorded.old_schema {
            continue;
        }
        let (twin, _) = participant_twin(&recorded.case["config"]).unwrap_or_else(|reason| panic!("{name}: {reason}"));
        write_case(&root.join("test/integration/cases"), name, &twin);
        for mut row in recorded.rows.clone() {
            row["schema"] = json!(PARTICIPANTS);
            expected_rows.push(row);
        }
        let mut case = recorded.case.clone();
        case["config"]["container"]["exposed_ports"] = json!([]);
        expected_cases.insert(format!("integration/{name}"), case);
    }
    let checkout = repository_root();
    let old_files = LOGGING_CASES
        .into_iter()
        .filter(|name| {
            let path = checkout.join(format!("test/integration/cases/{name}/config.yaml"));
            let file: Value = serde_saphyr::from_str(&read_text(&path)).unwrap();
            file.get("type").is_some()
        })
        .count();
    assert_eq!(
        expected_cases.len(),
        old_files,
        "every old-schema logging file must have a twin"
    );
    let actual = serde_json::to_value(Inventory::generate(&root)).unwrap();
    assert_eq!(actual["rows"], json!(expected_rows));
    assert_eq!(actual["cases"], Value::Object(expected_cases));
}

#[test]
fn logging_case_twins_use_the_default_log_file_fact_exactly_where_the_case_asserts_the_default_path() {
    let recorded = recorded_integration_cases();
    let used_fact: BTreeSet<_> = LOGGING_CASES
        .into_iter()
        .filter(|name| recorded[*name].old_schema)
        .filter(|name| participant_twin(&recorded[*name].case["config"]).unwrap().1)
        .collect();
    let expected: BTreeSet<_> = [
        "adp-logging-default-path",
        "adp-logging-ignores-core-agent-log-file",
        "adp-logging-windows-default-path",
    ]
    .into_iter()
    .filter(|name| recorded[*name].old_schema)
    .collect();
    assert_eq!(used_fact, expected);
}

#[test]
fn only_the_logging_cases_and_the_log_parsed_exit_cases_fit_the_integration_runner_translation() {
    let recorded = recorded_integration_cases();
    let supported: BTreeSet<_> = recorded
        .iter()
        .filter(|(_, recorded)| recorded.old_schema)
        .filter_map(|(name, recorded)| {
            let (twin, _) = participant_twin(&recorded.case["config"]).ok()?;
            let cases = CaseConfig::load_content(&twin, name, Path::new("config.yaml"))
                .unwrap_or_else(|error| panic!("{name}: {error:#}"));
            let [case] = cases.as_slice() else {
                panic!("{name}: expected one participant case");
            };
            for os in case.subject_os() {
                case.integration_config(*os)
                    .unwrap_or_else(|error| panic!("{name} on {os:?}: {error:#}"));
            }
            Some(name.as_str())
        })
        .collect();
    let expected: BTreeSet<_> = LOGGING_CASES
        .into_iter()
        .chain(LOG_PARSED_EXIT_CASES)
        .filter(|name| recorded[*name].old_schema)
        .collect();
    assert_eq!(supported, expected);
}
