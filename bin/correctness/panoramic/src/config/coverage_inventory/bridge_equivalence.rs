//! Checks logging-case translation and inventory entries against a frozen integration-schema baseline.
//!
//! Participant-schema twins of the seven logging cases use `logging_baseline.json`, copied while the cases used the
//! integration schema. All seven use the participant schema. Both the translated twins and checked-in inventory
//! must match this copy, except `schema` and `container.exposed_ports`.
//!
//! Never regenerate the copy from the inventory. If the inventory format changes intentionally (such as a new column
//! or `execution` value), edit the copy by hand so reviewers can see the change.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use panoramic_participants::AGENT_ADP;
use serde_json::{json, Value};

use super::{create_case_dirs, read_text, repository_root, Inventory, INTEGRATION, JSON_PATH, OLD, PARTICIPANTS};
use crate::config::{os_for_runtime, tests::write_case, typed_case::RunnableCase, ContainerConfig, IntakeConfig};

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
                .and_then(os_for_runtime)
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

fn logging_baseline() -> BTreeMap<String, RecordedCase> {
    let inventory: Value = serde_json::from_str(include_str!("logging_baseline.json")).unwrap();
    let rows = inventory["rows"].as_array().unwrap();
    let recorded: BTreeMap<_, _> = inventory["cases"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(key, case)| {
            let name = key
                .strip_prefix("integration/")
                .expect("baseline cases must be integration cases");
            let rows: Vec<_> = rows.iter().filter(|row| row["name"] == name).cloned().collect();
            assert!(!rows.is_empty(), "missing baseline rows for {name}");
            (
                name.to_string(),
                RecordedCase {
                    rows,
                    case: case.clone(),
                    old_schema: true,
                },
            )
        })
        .collect();
    assert_eq!(
        recorded.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        LOGGING_CASES.into_iter().collect(),
        "the frozen baseline must contain exactly the seven logging cases"
    );
    for row in rows {
        let name = row["name"].as_str().unwrap();
        assert!(recorded.contains_key(name), "missing baseline case for {name}");
        assert_eq!(row["suite"], INTEGRATION, "{name}: unexpected baseline suite");
        assert_eq!(row["schema"], INTEGRATION, "{name}: unexpected baseline schema");
        assert_eq!(row["execution"], OLD, "{name}: unexpected baseline execution");
    }
    recorded
}

fn migrated(baseline: &RecordedCase) -> (Vec<Value>, Value) {
    let mut rows = baseline.rows.clone();
    for row in &mut rows {
        row["schema"] = json!(PARTICIPANTS);
    }
    let mut case = baseline.case.clone();
    case["config"]["container"]["exposed_ports"] = json!([]);
    (rows, case)
}

#[test]
fn logging_case_twins_reproduce_the_frozen_baseline_except_schema_and_exposed_ports() {
    let baseline = logging_baseline();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    create_case_dirs(&root);
    let mut expected_rows = Vec::new();
    let mut expected_cases = serde_json::Map::new();
    for name in LOGGING_CASES {
        let baseline = &baseline[name];
        let (twin, _) = participant_twin(&baseline.case["config"]).unwrap_or_else(|reason| panic!("{name}: {reason}"));
        write_case(&root.join("test/integration/cases"), name, &twin);
        let (rows, case) = migrated(baseline);
        expected_rows.extend(rows);
        expected_cases.insert(format!("integration/{name}"), case);
    }
    let actual = serde_json::to_value(Inventory::generate(&root)).unwrap();
    assert_eq!(actual["rows"], json!(expected_rows));
    assert_eq!(actual["cases"], Value::Object(expected_cases));
}

#[test]
fn checked_in_logging_cases_use_the_participant_schema_and_otherwise_match_the_frozen_baseline() {
    let baseline = logging_baseline();
    let recorded = recorded_integration_cases();
    for name in LOGGING_CASES {
        let recorded = &recorded[name];
        assert!(!recorded.old_schema, "{name}: must use the participant schema");
        let (rows, case) = migrated(&baseline[name]);
        assert_eq!(
            json!({"rows": recorded.rows, "case": recorded.case}),
            json!({"rows": rows, "case": case}),
            "{name}: expected the participant schema with only schema and exposed_ports changed, \
             but its inventory entry differs from the frozen baseline"
        );
    }
}

/// Extracts logging participants' names and config, plus unresolved step paths omitted by integration translation.
fn unrecorded_logging_case_fields(case: &Value) -> Value {
    let step_paths: Vec<_> = case["procedure"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|node| match node.get("parallel") {
            Some(parallel) => parallel.as_array().unwrap().as_slice(),
            None => std::slice::from_ref(node),
        })
        .map(|step| step["path"].clone())
        .collect();
    json!({"participants": case["participants"], "step_paths": step_paths})
}

#[test]
fn logging_files_keep_their_twins_participants_and_unresolved_step_paths() {
    let baseline = logging_baseline();
    let root = repository_root();
    for name in LOGGING_CASES {
        let path = root.join(format!("test/integration/cases/{name}/config.yaml"));
        let case: Value = serde_saphyr::from_str(&read_text(&path)).unwrap();
        let (twin, _) =
            participant_twin(&baseline[name].case["config"]).unwrap_or_else(|reason| panic!("{name}: {reason}"));
        let twin: Value = serde_json::from_str(&twin).unwrap();
        assert_eq!(
            unrecorded_logging_case_fields(&case),
            unrecorded_logging_case_fields(&twin),
            "{name}: participants and unresolved step paths must match the frozen baseline's twin"
        );
    }
}

#[test]
fn logging_case_twins_use_the_default_log_file_fact_exactly_where_the_case_asserts_the_default_path() {
    let baseline = logging_baseline();
    let used_fact: BTreeSet<_> = LOGGING_CASES
        .into_iter()
        .filter(|name| participant_twin(&baseline[*name].case["config"]).unwrap().1)
        .collect();
    let expected: BTreeSet<_> = [
        "adp-logging-default-path",
        "adp-logging-ignores-core-agent-log-file",
        "adp-logging-windows-default-path",
    ]
    .into_iter()
    .collect();
    assert_eq!(used_fact, expected);
}

#[test]
fn only_the_logging_cases_and_the_log_parsed_exit_cases_fit_the_integration_runner_translation() {
    let baseline = logging_baseline();
    let recorded = recorded_integration_cases();
    let supported: BTreeSet<_> = baseline
        .iter()
        .chain(
            recorded
                .iter()
                .filter(|(name, recorded)| !LOGGING_CASES.contains(&name.as_str()) && recorded.old_schema),
        )
        .filter_map(|(name, recorded)| {
            let (twin, _) = participant_twin(&recorded.case["config"]).ok()?;
            let cases = crate::CASE_LOADER
                .load_content(&twin, name, Path::new("config.yaml"))
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
        .chain(
            LOG_PARSED_EXIT_CASES
                .into_iter()
                .filter(|name| recorded[*name].old_schema),
        )
        .collect();
    assert_eq!(supported, expected);
}
