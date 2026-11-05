//! Compares the seven logging cases with their frozen integration configuration.
//!
//! Never regenerate the baseline: it guards rows, steps, participants and unresolved paths.

use std::collections::{BTreeMap, BTreeSet};

use panoramic_core::{
    layout::{Os, Realization, RealizationKind},
    participant::ParticipantType,
};
use panoramic_participants::AgentAdp;
use serde_json::{json, Value};

use super::{create_case_dirs, read_text, repository_root, Inventory, INTEGRATION, JSON_PATH, NATIVE, PARTICIPANTS};
use crate::config::tests::write_case;

const LOGGING_CASES: [&str; 7] = [
    "adp-logging-default-path",
    "adp-logging-ignores-core-agent-log-file",
    "adp-logging-mac-ignores-core-agent-log-file",
    "adp-logging-respects-data-plane-log-file",
    "adp-logging-windows-default-path",
    "adp-logging-windows-ignores-core-agent-log-file",
    "adp-logging-windows-respects-data-plane-log-file",
];

/// Converts a recorded integration configuration's log/file assertions to participant-schema text.
///
/// Returns the text and whether a file path uses `{{ adp.facts.default_log_file }}`. The sole deliberately dropped
/// setting is `container.exposed_ports`: log and file assertions do not read published ports.
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
    let default_container = json!({"entrypoint": [], "command": [], "files": [], "host_cgroup_namespace": false});
    if container != default_container {
        return Err("container settings other than exposed_ports must be default".into());
    }
    if config["intake"] != json!({"enabled": false}) {
        return Err("intake settings must be default".into());
    }
    let subject_os = config["runtimes"]
        .as_array()
        .ok_or("runtimes must be an array")?
        .iter()
        .map(|runtime| {
            [Os::Linux, Os::MacOs, Os::Windows]
                .into_iter()
                .find(|os| Some(crate::config::runtime_for_os(*os)) == runtime.as_str())
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
            if subject_os.iter().all(|os| {
                let os = *os;
                let kind = match os {
                    Os::Linux => RealizationKind::LinuxContainer,
                    Os::MacOs => RealizationKind::HostProcessGroup,
                    Os::Windows => RealizationKind::WindowsContainer,
                };
                AgentAdp
                    .facts(Realization { os, kind })
                    .iter()
                    .any(|fact| fact.name == "default_log_file" && fact.value == path)
            }) {
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
    for name in LOGGING_CASES {
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
        assert_eq!(row["execution"], "old", "{name}: unexpected baseline execution");
    }
    recorded
}

fn migrated(baseline: &RecordedCase) -> Vec<Value> {
    let mut rows = baseline.rows.clone();
    for row in &mut rows {
        row["schema"] = json!(PARTICIPANTS);
        if row["subject_os"] == "linux" {
            row["execution"] = json!(NATIVE);
            row["runtime"] = json!("linux-container");
            row["images"] = json!({
                "agent-adp-linux-image": "saluki-images/datadog-agent:testing-devel",
                "courier-linux-binary": "target/x86_64-unknown-linux-musl/release/panoramic-courier",
                "fixup-image": "alpine:latest",
            });
        } else if row["subject_os"] == "macos" {
            row["execution"] = json!(NATIVE);
            row["runtime"] = json!("host-process-group");
            row["images"] = json!({
                "adp-host-binary": "target/release/agent-data-plane",
                "agent-host-install": "/tmp/saluki-dda/datadog-agent",
            });
        } else if row["subject_os"] == "windows" {
            row["execution"] = json!(NATIVE);
            row["runtime"] = json!("windows-container");
            row["images"] = json!({
                "agent-adp-windows-image": "saluki-images/agent-data-plane:testing-windows",
                "courier-windows-binary": "target/release/panoramic-courier.exe",
            });
        }
    }
    rows
}

#[test]
fn logging_case_twins_reproduce_the_migrated_rows() {
    let baseline = logging_baseline();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    create_case_dirs(&root);
    let mut expected_rows = Vec::new();
    for name in LOGGING_CASES {
        let baseline = &baseline[name];
        let (twin, _) = participant_twin(&baseline.case["config"]).unwrap_or_else(|reason| panic!("{name}: {reason}"));
        write_case(&root.join("test/integration/cases"), name, &twin);
        expected_rows.extend(migrated(baseline));
    }
    let actual = serde_json::to_value(Inventory::generate(&root)).unwrap();
    assert_eq!(actual["rows"], json!(expected_rows));
}

#[test]
fn checked_in_logging_cases_match_migrated_rows_and_steps() {
    let baseline = logging_baseline();
    let recorded = recorded_integration_cases();
    for name in LOGGING_CASES {
        let recorded = &recorded[name];
        assert!(!recorded.old_schema, "{name}: must use the participant schema");
        assert_eq!(recorded.rows, migrated(&baseline[name]), "{name}");
        assert_eq!(recorded.case["steps"], baseline[name].case["steps"], "{name}");
    }
}

/// Extracts participant settings and unresolved paths for comparison with the frozen configuration.
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
    let shared_paths = BTreeMap::from([
        (
            "adp-logging-respects-data-plane-log-file",
            vec![("DD_DATA_PLANE_LOG_FILE", "adp-custom.log")],
        ),
        (
            "adp-logging-mac-ignores-core-agent-log-file",
            vec![
                ("DD_LOG_FILE", "coreagent-only-mac.log"),
                ("DD_DATA_PLANE_LOG_FILE", "adp-mac-ignores-core-agent-log-file.log"),
            ],
        ),
    ]);
    for name in LOGGING_CASES {
        let path = root.join(format!("test/integration/cases/{name}/config.yaml"));
        let case: Value = serde_saphyr::from_str(&read_text(&path)).unwrap();
        let (twin, _) =
            participant_twin(&baseline[name].case["config"]).unwrap_or_else(|reason| panic!("{name}: {reason}"));
        let twin: Value = serde_json::from_str(&twin).unwrap();
        let mut expected = unrecorded_logging_case_fields(&twin);
        if let Some(paths) = shared_paths.get(name) {
            for (key, file) in paths {
                let old = format!("/tmp/{file}");
                assert_eq!(expected["participants"]["adp"]["config"]["env"][key], old);
                expected["participants"]["adp"]["config"]["env"][key] =
                    json!(format!("{{{{ self.facts.shared_dir }}}}/{file}"));
                let mut replaced = 0;
                for path in expected["step_paths"].as_array_mut().unwrap() {
                    if *path == old {
                        *path = json!(format!("{{{{ adp.facts.shared_dir }}}}/{file}"));
                        replaced += 1;
                    }
                }
                assert_eq!(replaced, 1, "{name}: {file}");
            }
        }
        assert_eq!(
            unrecorded_logging_case_fields(&case),
            expected,
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
