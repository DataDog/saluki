//! Generates the coverage inventory under `docs/development/plans/` from the case loader and CI definitions.
//!
//! Regenerate the Markdown and JSON snapshots with:
//!
//! ```sh
//! PANORAMIC_WRITE_COVERAGE_INVENTORY=1 cargo nextest run -p panoramic coverage_inventory
//! ```

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use panoramic_core::case::{CaseFile, CaseSchema, ProcedureNode, StepNode};
use panoramic_core::layout::Os;
use panoramic_core::plan::Compiled;
use panoramic_core::step::StepKind;
use panoramic_participants::AgentAdp;
use panoramic_steps::compare::analysis::AnalysisMode;
use saluki_error::GenericError;
use serde::Serialize;
use serde_json::{json, Value};

use super::{
    build_runners,
    runner_artifacts::RunnerArtifacts,
    tests::{planner, write_case, LOGGING_PARTICIPANT, PARTICIPANT_FIELDS},
    typed_case::{LoadedCase, LoadedPlan, TypedConfig},
    BuiltCase,
};

const JSON_PATH: &str = "docs/development/plans/panoramic-architecture-inventory.json";
const MARKDOWN_PATH: &str = "docs/development/plans/panoramic-architecture.md";
const COVERAGE_BEGIN: &str = "<!-- coverage-inventory:begin -->";
const COVERAGE_END: &str = "<!-- coverage-inventory:end -->";
const PROOFS_BEGIN: &str = "<!-- migration-proofs:begin -->";
const PROOFS_END: &str = "<!-- migration-proofs:end -->";
const PROOF_PIPELINE_URL: &str = "https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/";
const PROOF_JOB_URL: &str = "https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/";
const PROOF_COMPARISON: &str = "Compared with the inventory at `";
const REGENERATE: &str = "PANORAMIC_WRITE_COVERAGE_INVENTORY=1 cargo nextest run -p panoramic coverage_inventory";
const INTEGRATION: &str = "integration";
const CORRECTNESS: &str = "correctness";
const PARTICIPANTS: &str = "participants";
const SELECTED: &str = "selected";
const DISABLED: &str = "disabled";
const OLD: &str = "old";
const NATIVE: &str = "native";

/// Case directories relative to the repository root, with the suite and status their cases must have.
const CASE_DIRS: [(&str, &str, &str); 3] = [
    ("test/integration/cases", INTEGRATION, SELECTED),
    ("test/correctness/cases", CORRECTNESS, SELECTED),
    ("test/correctness/disabled", CORRECTNESS, DISABLED),
];

/// Coverage rows and effective case configurations recorded by the loader.
#[derive(Serialize)]
struct Inventory {
    rows: Vec<Row>,
    cases: BTreeMap<String, Case>,
}

/// One case's coverage on a subject OS, including its runtime and CI jobs.
#[derive(Serialize)]
struct Row {
    name: String,
    suite: String,
    subject_os: String,
    runtime: String,
    source: String,
    schema: String,
    execution: &'static str,
    status: &'static str,
    timeout: String,
    images: BTreeMap<String, String>,
    ci_jobs: Vec<String>,
    ci_mixins: Vec<String>,
    case: String,
}

/// Loaded configuration and assertion coverage shared by a case's OS rows.
#[derive(Serialize)]
struct Case {
    config: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    steps: Option<Steps>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comparison: Option<Comparison>,
    ignored_fields: Vec<String>,
}

/// Correctness comparison settings, including the count of additional ignored span fields.
#[derive(Serialize)]
struct Comparison {
    additional_span_ignore_fields: usize,
    analysis_mode: AnalysisMode,
    otlp_direct_analysis_mode: bool,
    require_dogstatsd_forwarded_packets: bool,
}

/// Integration assertion and action counts, including assertions in parallel blocks.
#[derive(Default, Serialize)]
struct Steps {
    single_assertions: usize,
    parallel_blocks: usize,
    parallel_assertions: usize,
    actions: usize,
    by_kind: BTreeMap<String, usize>,
}

impl Row {
    fn from_case(
        root: &Path, built: &BuiltCase, subject_os: &str, status: &'static str, schemas: &BTreeMap<String, String>,
        listed: &Value,
    ) -> Self {
        let case = built.plan.case();
        let (runtime, images, execution) = if let Some(runner) = built.runner() {
            (
                runner.runtime(),
                runner.images().into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
                OLD,
            )
        } else {
            let CaseSchema::Participants { file, .. } = case.schema() else {
                panic!("native cases declare participants")
            };
            let subject = file.participants.iter().find(|(_, entry)| entry.subject).unwrap().0;
            let realization = built
                .plan
                .layout()
                .unwrap()
                .iter()
                .find(|(name, _)| name == subject)
                .unwrap()
                .1;
            (
                realization.kind.name().into(),
                built
                    .plan
                    .artifacts()
                    .iter()
                    .map(|binding| (binding.role.name.into(), binding.reference.clone()))
                    .collect(),
                NATIVE,
            )
        };
        let source = relative_path(root, case.source());
        let schema = match case.schema() {
            CaseSchema::Participants { .. } => PARTICIPANTS.to_string(),
            _ => schemas[&source].clone(),
        };
        let suite = case.suite();
        let mut row = Self {
            case: format!("{suite}/{}", case.name()),
            name: case.name().to_string(),
            suite: suite.to_string(),
            subject_os: subject_os.to_string(),
            runtime,
            source,
            schema,
            execution,
            status,
            timeout: format!("{:?}", case.timeout()),
            images,
            ci_jobs: Vec::new(),
            ci_mixins: Vec::new(),
        };
        row.assign_ci(listed);
        row
    }

    /// Assigns the jobs and templates selected by the GitLab definitions and correctness pipeline generator.
    fn assign_ci(&mut self, listed: &Value) {
        if self.status == DISABLED {
            return;
        }
        // Integration jobs: .gitlab/e2e.yml and .gitlab/windows.yml (windows-integration-tests.ps1).
        // Correctness jobs and mixins: ci/tooling/generate-correctness-pipeline.sh.
        match (self.suite.as_str(), self.subject_os.as_str()) {
            (INTEGRATION, "linux") => self.ci_jobs.push("test-integration".to_string()),
            (INTEGRATION, "macos") => self.ci_jobs.extend([
                "test-integration-macos-arm64".to_string(),
                "test-integration-macos-amd64".to_string(),
            ]),
            (INTEGRATION, "windows") => self.ci_jobs.push("test-integration-windows-amd64".to_string()),
            (CORRECTNESS, "linux") => {
                self.ci_jobs
                    .push(format!("test-correctness-{}", self.name.replace('/', "-")));
                self.ci_mixins = correctness_ci_mixins(listed);
            }
            key => panic!("No CI mapping for {key:?}"),
        }
    }
}

/// Chooses correctness job templates from a `list --json` row, mirroring ci/tooling/generate-correctness-pipeline.sh.
fn correctness_ci_mixins(listed: &Value) -> Vec<String> {
    assert_eq!(listed["status"], "runnable", "a correctness CI row must be runnable");
    let layout = listed["layout"]
        .as_array()
        .expect("a runnable row must have a layout array");
    let roles = listed["roles"]
        .as_array()
        .expect("a runnable row must have a roles array");
    let mut mixins = vec![if layout.iter().any(|entry| entry["realization"] == "kubernetes-pod") {
        ".test-correctness-kind-definition".to_string()
    } else {
        ".test-correctness-definition".to_string()
    }];
    if roles.iter().any(|entry| entry["role"] == AgentAdp::LINUX_IMAGE.name) {
        mixins.push(".test-correctness-adp-baseline".to_string());
    }
    mixins
}

impl Inventory {
    fn generate(root: &Path) -> Self {
        let mut schemas = BTreeMap::new();
        let mut rows = Vec::new();
        for (suite_dir, ..) in CASE_DIRS {
            for entry in fs::read_dir(root.join(suite_dir)).unwrap() {
                let dir = entry.unwrap().path();
                let path = dir.join("config.yaml");
                if !dir.is_dir() || !path.is_file() {
                    continue;
                }
                let source = relative_path(root, &path);
                let text = read_text(&path);
                let doc: Value = serde_saphyr::from_str(&text).unwrap();
                if let Some(schema) = doc.get("type").and_then(Value::as_str) {
                    schemas.insert(source, schema.to_string());
                }
            }
        }
        // Discover all directories together, per OS, to reject duplicate names across suites and statuses.
        for os in [Os::Linux, Os::MacOs, Os::Windows] {
            let cases = crate::CASE_LOADER
                .discover_cases(&CASE_DIRS.map(|(dir, ..)| root.join(dir)))
                .unwrap_or_else(|error| panic!("{error:#}"));
            let plans: Vec<_> = cases
                .into_iter()
                .filter_map(|case| match planner().compile(case, os).unwrap() {
                    Compiled::Planned(plan) => Some(*plan),
                    Compiled::Excluded(_) => None,
                })
                .collect();
            let built = build_runners(plans, &RunnerArtifacts::Overrides(&[])).unwrap();
            let listed = crate::list_command::listing(&built, &[], os);
            for built in built {
                let case = built.plan.case();
                let status = if case.is_disabled() { DISABLED } else { SELECTED };
                rows.push(Row::from_case(
                    root,
                    &built,
                    os.name(),
                    status,
                    &schemas,
                    &listed[case.name()],
                ));
            }
        }

        // Keep the snapshot stable when generated on either supported host architecture.
        for row in &mut rows {
            if let Some(reference) = row.images.get_mut("courier-linux-binary") {
                if reference == "target/aarch64-unknown-linux-musl/release/panoramic-courier" {
                    *reference = "target/x86_64-unknown-linux-musl/release/panoramic-courier".into();
                }
            }
        }
        rows.sort_by(|a, b| (&a.suite, &a.name, &a.subject_os).cmp(&(&b.suite, &b.name, &b.subject_os)));
        let mut keys = BTreeSet::new();
        for row in &rows {
            let source = &row.source;
            let suite = row.suite.as_str();
            let status = row.status;
            let Some((dir, ..)) = CASE_DIRS
                .iter()
                .find(|(_, expected_suite, expected_status)| *expected_suite == suite && *expected_status == status)
            else {
                panic!("{source}: a {status} {suite} case has no case directory");
            };
            assert!(
                source.starts_with(&format!("{dir}/")),
                "{source}: a {status} {suite} case belongs in {dir}/"
            );
            let key = (&row.suite, &row.name, &row.subject_os);
            assert!(keys.insert(key), "Duplicate inventory row: {key:?}");
        }
        let sources: BTreeSet<_> = rows.iter().map(|row| &row.source).collect();
        let mut cases = BTreeMap::new();
        for source in sources {
            for (key, case) in load_cases(&root.join(source)) {
                assert!(cases.insert(key.clone(), case).is_none(), "Duplicate case: {key}");
            }
        }
        for row in &rows {
            assert!(cases.contains_key(&row.case), "Missing loaded case: {}", row.case);
        }
        Self { rows, cases }
    }

    fn assert_file_accounting(&self, root: &Path) {
        let sources: BTreeSet<_> = self.rows.iter().map(|row| row.source.as_str()).collect();
        for (dir, ..) in CASE_DIRS {
            let mut files = Vec::new();
            collect_case_files(&root.join(dir), &mut files);
            for path in files {
                let source = relative_path(root, &path);
                assert!(
                    sources.contains(source.as_str()),
                    "Case file produces no inventory row: {source}"
                );
            }
        }
    }

    fn markdown(&self) -> String {
        let mut text = String::from(
            "## Coverage totals\n\n\
             | Suite | Files | Selected rows (subject OS / runtime) | Disabled rows | Assertions (subject OS) | Actions (subject OS) |\n\
             | --- | --- | --- | --- | --- | --- |\n",
        );
        for suite in [INTEGRATION, CORRECTNESS] {
            let rows: Vec<_> = self.rows.iter().filter(|r| r.suite == suite).collect();
            let files: BTreeSet<_> = rows.iter().map(|r| &r.source).collect();
            let mut selected = BTreeMap::<String, usize>::new();
            let mut assertions = BTreeMap::<String, usize>::new();
            let mut actions = BTreeMap::<String, usize>::new();
            let mut disabled = 0;
            for row in rows {
                if row.status == DISABLED {
                    disabled += 1;
                    continue;
                }
                *selected
                    .entry(format!("{} / {}", row.subject_os, row.runtime))
                    .or_default() += 1;
                if let Some(steps) = &self.cases[&row.case].steps {
                    *assertions.entry(row.subject_os.clone()).or_default() +=
                        steps.single_assertions + steps.parallel_assertions;
                    *actions.entry(row.subject_os.clone()).or_default() += steps.actions;
                }
            }
            writeln!(
                text,
                "| {suite} | {} | {} | {disabled} | {} | {} |",
                files.len(),
                format_counts(&selected),
                format_counts(&assertions),
                format_counts(&actions),
            )
            .unwrap();
        }
        writeln!(text, "\nTotal: {} rows.\n", self.rows.len()).unwrap();
        text.push_str(
            "## Coverage rows\n\n\
             | Case | Suite | Subject OS | Runtime | Status | CI job | Schema | Execution | Steps | Ignored |\n\
             | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n",
        );
        for row in &self.rows {
            let case = &self.cases[&row.case];
            let steps = if let Some(steps) = &case.steps {
                format!(
                    "assertions: {}, actions: {}",
                    steps.single_assertions + steps.parallel_assertions,
                    steps.actions
                )
            } else {
                let comparison = case.comparison.as_ref().unwrap();
                format!(
                    "compare {}{}",
                    serde_json::to_value(&comparison.analysis_mode)
                        .unwrap()
                        .as_str()
                        .unwrap(),
                    if comparison.require_dogstatsd_forwarded_packets {
                        ", forwarded packets"
                    } else {
                        ""
                    }
                )
            };
            writeln!(
                text,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                row.name,
                row.suite,
                row.subject_os,
                row.runtime,
                row.status,
                if row.ci_jobs.is_empty() {
                    "—".to_string()
                } else {
                    row.ci_jobs.join(", ")
                },
                row.schema,
                row.execution,
                steps,
                case.ignored_fields.len(),
            )
            .unwrap();
        }
        text
    }
}

fn load_cases(path: &Path) -> Vec<(String, Case)> {
    let loaded = crate::CASE_LOADER
        .load_file(path)
        .unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
    let content = read_text(path);
    let directory = path.parent().unwrap().file_name().unwrap().to_str().unwrap();
    let mut cases =
        record_cases(&loaded, &content, directory).unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
    let doc: Value = serde_saphyr::from_str(&content).unwrap();
    let canonical = path.canonicalize().unwrap();
    let suite = loaded.first().expect("a case file loads at least one case").suite();
    let view = |doc| loaded_view(suite, &doc, directory, &canonical);
    let configs: Vec<_> = cases.iter().map(|(key, case)| (key, &case.config)).collect();
    assert_eq!(
        view(doc.clone()).unwrap(),
        serde_json::to_value(configs).unwrap(),
        "{}: its JSON form loads differently from its YAML, so the ignored-key probe would compare against the wrong configuration",
        path.display()
    );
    let ignored = ignored_fields(&doc, &view, path);
    for (_, case) in &mut cases {
        case.ignored_fields.clone_from(&ignored);
    }
    cases
}

/// Records each case under `<suite>/<name>`: its runner configuration and step or comparison coverage.
fn record_cases(cases: &[LoadedCase], content: &str, directory: &str) -> Result<Vec<(String, Case)>, GenericError> {
    let mut recorded = Vec::new();
    for case in cases {
        let effective = match case.schema() {
            CaseSchema::Typed(TypedConfig::Correctness(config)) => Case {
                config: serde_json::to_value(config)?,
                steps: None,
                comparison: Some(Comparison {
                    additional_span_ignore_fields: config.additional_span_ignore_fields.len(),
                    analysis_mode: config.analysis_mode.clone(),
                    otlp_direct_analysis_mode: config.otlp_direct_analysis_mode,
                    require_dogstatsd_forwarded_packets: config.require_dogstatsd_forwarded_packets,
                }),
                ignored_fields: Vec::new(),
            },
            CaseSchema::Participants { file, .. } => {
                let mut steps = Steps::default();
                let mut step_value = |step: &StepNode| {
                    *steps.by_kind.entry(step.name.clone()).or_default() += 1;
                    let mut fields = step.fields.clone();
                    fields.insert("step".into(), json!(step.name));
                    json!(fields)
                };
                let procedure = file
                    .procedure
                    .iter()
                    .map(|node| match node {
                        ProcedureNode::Step(step) => {
                            let factory = panoramic_steps::REGISTERED_STEPS
                                .iter()
                                .find(|factory| factory.name() == step.name)
                                .expect("loaded steps have a registered factory");
                            match factory.kind() {
                                StepKind::Action => steps.actions += 1,
                                StepKind::Assertion => steps.single_assertions += 1,
                            }
                            step_value(step)
                        }
                        ProcedureNode::Parallel(parallel) => {
                            steps.parallel_blocks += 1;
                            steps.parallel_assertions += parallel.len();
                            json!({"parallel": parallel.iter().map(&mut step_value).collect::<Vec<_>>()})
                        }
                    })
                    .collect::<Vec<_>>();
                let mut config = BTreeMap::new();
                for os in case.subject_os() {
                    let loaded = crate::CASE_LOADER
                        .load_content("cases", content, directory, case.source())?
                        .into_iter()
                        .find(|loaded| loaded.name() == case.name())
                        .expect("reloading yields the same case");
                    let plan = planner()
                        .compile(loaded, *os)?
                        .into_plan()
                        .expect("the case declares this subject OS");
                    let participants = participant_records(&plan, file);
                    config.insert(os.name(), json!({"participants": participants, "procedure": procedure}));
                }
                Case {
                    config: json!(config),
                    steps: Some(steps),
                    comparison: None,
                    ignored_fields: Vec::new(),
                }
            }
        };
        let suite = case.suite();
        recorded.push((format!("{suite}/{}", case.name()), effective));
    }
    Ok(recorded)
}

fn participant_records<'a>(plan: &'a LoadedPlan, file: &CaseFile) -> BTreeMap<&'a str, Value> {
    plan.layout()
        .unwrap()
        .iter()
        .map(|(name, realization)| {
            let entry = &file.participants[name];
            (
                name,
                json!({
                    "type": entry.participant_type, "subject": entry.subject,
                    "realization": realization.kind.name(), "env": plan.case_values(name).unwrap().env,
                }),
            )
        })
        .collect()
}

/// Loads `doc` as the file at `source` and returns each case's recorded configuration.
///
/// JSON text is valid YAML and preserves strings as strings. `load_cases` checks it against the original YAML.
fn loaded_view(suite: &str, doc: &Value, directory: &str, source: &Path) -> Result<Value, GenericError> {
    let content = serde_json::to_string(doc)?;
    let cases = crate::CASE_LOADER.load_content(suite, &content, directory, source)?;
    let recorded = record_cases(&cases, &content, directory)?;
    let configs: Vec<_> = recorded.iter().map(|(key, case)| (key, &case.config)).collect();
    Ok(serde_json::to_value(configs)?)
}

/// Finds keys whose replacement by both a map and a string leaves the effective configuration unchanged.
///
/// `view` loads and serializes the case. The string probe distinguishes default-valued structs, which accept
/// maps of unknown keys, from ignored fields. The probe also checks that ignored keys have ignored descendants.
fn ignored_fields(doc: &Value, view: &impl Fn(Value) -> Result<Value, GenericError>, file: &Path) -> Vec<String> {
    let original = view(doc.clone()).unwrap();
    let mut paths = Vec::new();
    mapping_paths(doc, "", "", &mut paths);
    let sentinels = [
        json!({"panoramic-inventory-probe": {}}),
        json!("panoramic-inventory-probe"),
    ];
    let mut ignored = Vec::new();
    for (pointer, dotted) in &paths {
        if pointer == "/type" {
            continue;
        }
        let ignored_here = sentinels.iter().all(|sentinel| {
            let mut probe = doc.clone();
            *probe.pointer_mut(pointer).unwrap() = sentinel.clone();
            view(probe).is_ok_and(|value| value == original)
        });
        if ignored_here {
            ignored.push(dotted.clone());
        }
    }
    ignored.sort();
    for parent in &ignored {
        for (_, path) in &paths {
            if path
                .strip_prefix(parent)
                .is_some_and(|suffix| suffix.starts_with('.') || suffix.starts_with('['))
            {
                assert!(
                    ignored.binary_search(path).is_ok(),
                    "Ignored-key probe is inconsistent at {path} in {}",
                    file.display(),
                );
            }
        }
    }
    ignored
}

fn mapping_paths(value: &Value, pointer: &str, dotted: &str, paths: &mut Vec<(String, String)>) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let pointer = format!("{pointer}/{}", key.replace('~', "~0").replace('/', "~1"));
                let dotted = if dotted.is_empty() {
                    key.clone()
                } else {
                    format!("{dotted}.{key}")
                };
                paths.push((pointer.clone(), dotted.clone()));
                mapping_paths(value, &pointer, &dotted, paths);
            }
        }
        Value::Array(values) => {
            for (i, value) in values.iter().enumerate() {
                mapping_paths(value, &format!("{pointer}/{i}"), &format!("{dotted}[{i}]"), paths);
            }
        }
        _ => {}
    }
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap()
}

fn create_case_dirs(root: &Path) {
    for (dir, ..) in CASE_DIRS {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap().to_str().unwrap().replace('\\', "/")
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn collect_case_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect_case_files(&path, files);
        } else if path.file_name().unwrap() == "config.yaml" {
            files.push(path);
        }
    }
}

fn format_counts(counts: &BTreeMap<String, usize>) -> String {
    if counts.is_empty() {
        "—".to_string()
    } else {
        counts
            .iter()
            .map(|(key, count)| format!("{key}: {count}"))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

fn marked_region<'a>(text: &'a str, begin: &str, end: &str) -> (&'a str, &'a str, &'a str) {
    assert_eq!(text.matches(begin).count(), 1, "Expected one {begin}");
    assert_eq!(text.matches(end).count(), 1, "Expected one {end}");
    let start = text.find(begin).unwrap() + begin.len();
    let stop = text.find(end).unwrap();
    assert!(start < stop, "Reversed markers: {begin}, {end}");
    (&text[..start], &text[start..stop], &text[stop..])
}

fn write_snapshot(path: &Path, text: &str) {
    // Atomic replacement ensures parallel readers see a complete snapshot.
    let mut file = tempfile::NamedTempFile::new_in(path.parent().unwrap()).unwrap();
    file.write_all(text.as_bytes()).unwrap();
    file.persist(path).unwrap();
}

fn row_keys(json: &str) -> BTreeSet<String> {
    let value: Value = serde_json::from_str(json).unwrap();
    value["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            format!(
                "{}/{}/{}",
                row["suite"].as_str().unwrap(),
                row["name"].as_str().unwrap(),
                row["subject_os"].as_str().unwrap()
            )
        })
        .collect()
}

fn first_difference(path: &str, saved: &str, generated: &str) -> Option<String> {
    if saved == generated {
        return None;
    }
    let saved: Vec<_> = saved.split('\n').collect();
    let generated: Vec<_> = generated.split('\n').collect();
    let line = (0..saved.len().max(generated.len()))
        .find(|&i| saved.get(i) != generated.get(i))
        .unwrap();
    Some(format!(
        "{path}:{}\n  saved: {}\n  generated: {}",
        line + 1,
        saved.get(line).unwrap_or(&"<EOF>"),
        generated.get(line).unwrap_or(&"<EOF>"),
    ))
}

/// Every integration case and subject OS as first recorded in the coverage inventory; a change here removes or adds
/// coverage.
const ORIGINAL_INTEGRATION_ROWS: &[(&str, &[Os])] = &[
    ("adp-cmd-port", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-config-check-exit", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-config-check-no-warn", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-config-check-warn", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-config-dynamic-basic", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-config-dynamic-runtime", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-config-otlp", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-config-stream", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-disabled-exit", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-ipc-no-dns", &[Os::Linux]),
    ("adp-logging-default-path", &[Os::Linux]),
    ("adp-logging-ignores-core-agent-log-file", &[Os::Linux]),
    ("adp-logging-mac-ignores-core-agent-log-file", &[Os::MacOs]),
    ("adp-logging-respects-data-plane-log-file", &[Os::Linux, Os::MacOs]),
    ("adp-logging-windows-default-path", &[Os::Windows]),
    ("adp-logging-windows-ignores-core-agent-log-file", &[Os::Windows]),
    ("adp-logging-windows-respects-data-plane-log-file", &[Os::Windows]),
    ("adp-memory-mode-disabled", &[Os::Linux, Os::MacOs, Os::Windows]),
    (
        "adp-memory-mode-permissive-exceeds-limit",
        &[Os::Linux, Os::MacOs, Os::Windows],
    ),
    (
        "adp-memory-mode-permissive-within-limit",
        &[Os::Linux, Os::MacOs, Os::Windows],
    ),
    (
        "adp-memory-mode-strict-exceeds-limit",
        &[Os::Linux, Os::MacOs, Os::Windows],
    ),
    (
        "adp-memory-mode-strict-within-limit",
        &[Os::Linux, Os::MacOs, Os::Windows],
    ),
    ("adp-no-pipelines-exit", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-rar-disabled", &[Os::Linux]),
    ("adp-rar-registration", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("adp-rc-core-disabled", &[Os::Linux]),
    ("adp-rc-no-products", &[Os::Linux]),
    ("apm-bind-custom-hostname", &[Os::Linux, Os::Windows]),
    ("basic-startup", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("dogstatsd-autoscale-udp", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("dogstatsd-bind-custom-hostname", &[Os::Linux, Os::Windows]),
    ("dogstatsd-bind-host", &[Os::Linux, Os::Windows]),
    ("dogstatsd-default-bind", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("dogstatsd-enabled", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("dogstatsd-forwarding", &[Os::Linux]),
    ("dogstatsd-host-tag", &[Os::Linux]),
    ("dogstatsd-intake-metric", &[Os::Linux]),
    ("dogstatsd-named-pipe-windows", &[Os::Windows]),
    ("dogstatsd-no-listener-continues", &[Os::Linux, Os::MacOs]),
    (
        "dogstatsd-non-local-overrides-bind-host",
        &[Os::Linux, Os::MacOs, Os::Windows],
    ),
    ("dogstatsd-replay-unix", &[Os::Linux, Os::MacOs]),
    ("dogstatsd-replay-windows", &[Os::Windows]),
    ("dogstatsd-require-listener-exit", &[Os::Linux, Os::MacOs]),
    ("dogstatsd-skip-failed-listener", &[Os::Linux, Os::MacOs]),
    ("dogstatsd-top-windows", &[Os::Windows]),
    ("dogstatsd-top", &[Os::Linux]),
    ("liveness-container-tags", &[Os::Linux]),
    ("otlp-tls", &[Os::Linux]),
    ("otlp-traces-enabled", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("privileged-api-endpoints", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("telemetry-endpoint", &[Os::Linux, Os::MacOs, Os::Windows]),
    ("unprivileged-api-endpoints", &[Os::Linux, Os::MacOs, Os::Windows]),
];

/// Rows added on purpose beyond the original coverage.
const INTENDED_ADDITIONS: &[(&str, &[Os])] = &[
    ("dogstatsd-host-tag", &[Os::MacOs, Os::Windows]),
    ("dogstatsd-intake-metric", &[Os::MacOs, Os::Windows]),
];

#[test]
fn every_original_integration_row_runs_natively() {
    let inventory = Inventory::generate(&repository_root());
    let expected: BTreeSet<_> = ORIGINAL_INTEGRATION_ROWS
        .iter()
        .chain(INTENDED_ADDITIONS)
        .flat_map(|&(name, oses)| oses.iter().map(move |os| (name, os.name())))
        .collect();
    let rows: Vec<_> = inventory.rows.iter().filter(|row| row.suite == INTEGRATION).collect();
    let actual: BTreeSet<_> = rows
        .iter()
        .map(|row| (row.name.as_str(), row.subject_os.as_str()))
        .collect();
    assert_eq!(
        actual, expected,
        "integration coverage must keep every original row and only the intended additions"
    );
    for row in rows {
        let key = (row.name.as_str(), row.subject_os.as_str());
        assert_eq!(row.execution, NATIVE, "{key:?}: must run natively");
        assert_eq!(row.status, SELECTED, "{key:?}");
        let jobs: &[&str] = match row.subject_os.as_str() {
            "linux" => &["test-integration"],
            "macos" => &["test-integration-macos-arm64", "test-integration-macos-amd64"],
            "windows" => &["test-integration-windows-amd64"],
            _ => unreachable!("the row set was checked"),
        };
        assert_eq!(row.ci_jobs, jobs, "{key:?}: original CI jobs must be preserved");
    }
}

#[test]
fn coverage_inventory_matches_the_case_files() {
    let root = repository_root();
    let inventory = Inventory::generate(&root);
    let json = serde_json::to_string_pretty(&inventory).unwrap() + "\n";
    let markdown = read_text(&root.join(MARKDOWN_PATH));
    let (before, _, after) = marked_region(&markdown, COVERAGE_BEGIN, COVERAGE_END);
    let generated_markdown = format!("{before}\n\n{}\n{after}", inventory.markdown());
    if std::env::var_os("PANORAMIC_WRITE_COVERAGE_INVENTORY").is_some() {
        write_snapshot(&root.join(JSON_PATH), &json);
        write_snapshot(&root.join(MARKDOWN_PATH), &generated_markdown);
        return;
    }
    let saved_json = read_text(&root.join(JSON_PATH));
    let differences: Vec<_> = [
        first_difference(JSON_PATH, &saved_json, &json),
        first_difference(MARKDOWN_PATH, &markdown, &generated_markdown),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !differences.is_empty() {
        let saved_keys = row_keys(&saved_json);
        let generated_keys = row_keys(&json);
        panic!(
            "Coverage inventory differs.\nAdded rows: {:?}\nRemoved rows: {:?}\n{}\nRegenerate: {REGENERATE}",
            generated_keys.difference(&saved_keys).collect::<Vec<_>>(),
            saved_keys.difference(&generated_keys).collect::<Vec<_>>(),
            differences.join("\n"),
        );
    }
}

#[test]
fn participant_inventory_records_native_rows_and_per_os_configuration_as_written() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    let yaml = LOGGING_PARTICIPANT
        .replace("linux, macos, windows", "linux, windows")
        .replace(r"C:\adp\adp.log", "{{ adp.facts.default_log_file }}")
        .replace(
            "    config:\n",
            "    config:\n      os: {windows: {env: {DD_API_KEY: windows-key}}}\n",
        );
    write_case(&root.path().join("test/integration/cases"), "alpha", &yaml);
    let inventory = Inventory::generate(&root.path().canonicalize().unwrap());
    assert_eq!(inventory.rows.len(), 2);
    let linux = &inventory.rows[0];
    assert_eq!(linux.subject_os, "linux");
    assert_eq!(linux.execution, NATIVE);
    assert_eq!(linux.runtime, "linux-container");
    assert_eq!(
        linux.images.keys().map(String::as_str).collect::<Vec<_>>(),
        ["agent-adp-linux-image", "courier-linux-binary", "fixup-image"]
    );
    assert_eq!(
        linux.images["agent-adp-linux-image"],
        "saluki-images/datadog-agent:testing-devel"
    );
    assert_eq!(
        linux.images["courier-linux-binary"],
        "target/x86_64-unknown-linux-musl/release/panoramic-courier"
    );
    let windows = &inventory.rows[1];
    assert_eq!(windows.subject_os, "windows");
    assert_eq!(windows.execution, NATIVE);
    assert_eq!(windows.runtime, "windows-container");
    assert_eq!(
        windows.images,
        BTreeMap::from([
            (
                "agent-adp-windows-image".into(),
                "saluki-images/agent-data-plane:testing-windows".into()
            ),
            (
                "courier-windows-binary".into(),
                "target/release/panoramic-courier.exe".into()
            ),
        ])
    );
    let case = &inventory.cases["integration/alpha"];
    let written: Value = serde_saphyr::from_str(&yaml).unwrap();
    for (os, realization) in [("linux", "linux-container"), ("windows", "windows-container")] {
        assert_eq!(case.config[os]["procedure"], written["procedure"]);
        assert_eq!(case.config[os]["participants"]["adp"]["type"], "agent-adp");
        assert_eq!(case.config[os]["participants"]["adp"]["subject"], true);
        assert_eq!(case.config[os]["participants"]["adp"]["realization"], realization);
    }
    assert_eq!(
        case.config["windows"]["participants"]["adp"]["env"]["DD_API_KEY"],
        "windows-key"
    );
    assert_eq!(
        case.config["linux"]["participants"]["adp"]["env"],
        written["participants"]["adp"]["config"]["env"]
    );
    let steps = case.steps.as_ref().unwrap();
    assert_eq!(
        (
            steps.single_assertions,
            steps.parallel_blocks,
            steps.parallel_assertions,
            steps.actions
        ),
        (2, 1, 2, 0)
    );
    assert_eq!(
        steps.by_kind,
        BTreeMap::from([
            ("file_contains".into(), 2),
            ("log_contains".into(), 1),
            ("log_not_contains".into(), 1)
        ])
    );
}

#[test]
fn participant_inventory_counts_actions_by_registered_kind() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    let yaml = LOGGING_PARTICIPANT.replace("linux, macos, windows", "linux").replace(
        "        DD_API_KEY:",
        "        DD_DOGSTATSD_NON_LOCAL_TRAFFIC: \"true\"\n        DD_API_KEY:",
    ) + "  - step: send\n    on: adp\n    endpoint: dogstatsd_udp\n    payload: hello\n    timeout: 1s\n";
    write_case(&root.path().join("test/integration/cases"), "alpha", &yaml);
    let inventory = Inventory::generate(&root.path().canonicalize().unwrap());
    let case = &inventory.cases["integration/alpha"];
    let written: Value = serde_saphyr::from_str(&yaml).unwrap();
    assert_eq!(case.config["linux"]["procedure"], written["procedure"]);
    let steps = case.steps.as_ref().unwrap();
    assert_eq!(
        (
            steps.single_assertions,
            steps.parallel_blocks,
            steps.parallel_assertions,
            steps.actions
        ),
        (2, 1, 2, 1)
    );
    assert_eq!(steps.by_kind["send"], 1);
}

#[test]
fn every_case_file_produces_an_inventory_row() {
    let root = repository_root();
    Inventory::generate(&root).assert_file_accounting(&root);
}

#[test]
#[should_panic(expected = "Case name 'shared' is declared more than once")]
fn inventory_rejects_a_case_name_used_in_both_suites() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    write_case(
        &root.path().join("test/integration/cases"),
        "shared",
        "name: shared\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
    );
    write_case(
        &root.path().join("test/correctness/cases"),
        "shared",
        "type: correctness\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\n",
    );
    Inventory::generate(root.path());
}

#[test]
fn inventory_records_a_case_in_the_disabled_directory_as_disabled_without_ci_jobs() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    create_case_dirs(&root);
    write_case(
        &root.join("test/correctness/disabled"),
        "alpha",
        "type: correctness\ndisabled: true\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\n",
    );

    let inventory = Inventory::generate(&root);
    assert_eq!(inventory.rows.len(), 1);
    let row = &inventory.rows[0];
    assert_eq!(row.status, DISABLED);
    assert!(row.ci_jobs.is_empty());
    assert!(row.ci_mixins.is_empty());
    assert_eq!(row.source, "test/correctness/disabled/alpha/config.yaml");
    inventory.assert_file_accounting(&root);
}

#[test]
#[should_panic(expected = "a disabled correctness case belongs in test/correctness/disabled/")]
fn inventory_rejects_a_disabled_case_in_the_selected_correctness_directory() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    write_case(
        &root.path().join("test/correctness/cases"),
        "alpha",
        "type: correctness\ndisabled: true\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\n",
    );
    Inventory::generate(&root.path().canonicalize().unwrap());
}

#[test]
#[should_panic(expected = "a selected correctness case belongs in test/correctness/cases/")]
fn inventory_rejects_an_enabled_case_in_the_disabled_directory() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    write_case(
        &root.path().join("test/correctness/disabled"),
        "alpha",
        "type: correctness\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\n",
    );
    Inventory::generate(&root.path().canonicalize().unwrap());
}

#[test]
fn inventory_reports_a_participant_case_in_a_correctness_directory_as_native_correctness() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    write_case(
        &root.path().join("test/correctness/cases"),
        "alpha",
        "name: alpha\ntimeout: 1s\nsubject_os: [linux]\nparticipants: {adp: {type: agent-adp, subject: true}}\nprocedure: [{step: log_contains, on: adp, pattern: ready, timeout: 5s}]\n",
    );
    let inventory = Inventory::generate(&root.path().canonicalize().unwrap());
    assert_eq!(inventory.rows.len(), 1);
    let row = &inventory.rows[0];
    assert_eq!(row.name, "alpha");
    assert_eq!(row.suite, CORRECTNESS);
    assert_eq!(row.schema, PARTICIPANTS);
    assert_eq!(row.execution, NATIVE);
    assert_eq!(row.status, SELECTED);
}

#[test]
#[should_panic(expected = "Case name 'shared' is declared more than once")]
fn inventory_rejects_a_case_name_used_in_the_selected_and_disabled_correctness_directories() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    write_case(
        &root.path().join("test/correctness/cases"),
        "shared",
        "type: correctness\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\n",
    );
    write_case(
        &root.path().join("test/correctness/disabled"),
        "shared",
        "type: correctness\ndisabled: true\nruntime: docker\nanalysis_mode: metrics\nbaseline: {image: base}\ncomparison: {image: comp}\n",
    );
    Inventory::generate(&root.path().canonicalize().unwrap());
}

#[test]
#[should_panic(expected = "must be a string: 'correctness' or 'correctness_matrix'")]
fn inventory_reports_the_loader_error_for_a_non_string_type() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    write_case(&root.path().join("test/integration/cases"), "alpha", "type: 3\n");
    Inventory::generate(root.path());
}

/// Returns the first pin missing after its predecessor's match in `text`.
/// Lines are trimmed; a candidate's trailing ` \` is optional.
fn missing_line<'a>(text: &str, lines: &[&'a str]) -> Option<&'a str> {
    let mut candidates = text.lines().map(str::trim);
    lines.iter().copied().find(|line| {
        !candidates.any(|candidate| candidate == line.trim() || candidate.strip_suffix(" \\") == Some(line.trim()))
    })
}

#[track_caller]
fn assert_lines_in_order(file: &str, text: &str, lines: &[&str]) {
    if let Some(line) = missing_line(text, lines) {
        panic!("{file}: CI line changed or out of order: {line}");
    }
}

/// Restricts pins to one YAML job or Make target, including repeated declarations of that target.
#[track_caller]
fn assert_ci_block_lines_in_order(file: &str, text: &str, lines: &[&str]) {
    let (name, _) = lines[0]
        .split_once(':')
        .expect("the first pin must be a job or target header");
    let block: Vec<_> = text
        .lines()
        .skip_while(|line| *line != lines[0])
        .take_while(|line| {
            line.split_once(':').is_none_or(|(candidate, _)| {
                candidate == name || candidate.contains(char::is_whitespace) || candidate.starts_with('#')
            })
        })
        .collect();
    assert_lines_in_order(file, &block.join("\n"), lines);
}

#[test]
fn ci_line_pins_reject_commands_moved_to_another_job_or_target() {
    let root = repository_root();
    for (file, header, command) in [
        (
            ".gitlab/e2e.yml",
            "test-integration:",
            "- target/release/panoramic artifacts pull -d test/integration/cases --subject-os linux --artifacts target/panoramic-artifacts.toml",
        ),
        (
            "Makefile",
            "test-integration: build-panoramic build-courier-linux build-datadog-agent-image build-correctness-tools-image",
            r#"@target/release/panoramic run --subject-os linux -d $(shell pwd)/test/integration/cases $(if $(PANORAMIC_LOG_DIR),-l $(PANORAMIC_LOG_DIR)) $(PANORAMIC_ARTIFACTS_ARG)"#,
        ),
    ] {
        let text = read_text(&root.join(file));
        let pins = [header, command];
        assert_ci_block_lines_in_order(file, &text, &pins);
        let (before, after) = text.split_once(header).unwrap();
        let moved = format!(
            "{before}{header}{}\npin-test-later:\n\t{command}\n",
            after.replacen(command, "", 1)
        );
        let error = std::panic::catch_unwind(|| assert_ci_block_lines_in_order(file, &moved, &pins))
            .expect_err("a command in another job or target must not satisfy the pin");
        assert_eq!(
            error.downcast_ref::<String>().unwrap(),
            &format!("{file}: CI line changed or out of order: {command}")
        );
    }
}

#[test]
fn missing_line_returns_the_first_pinned_line_absent_after_its_predecessor() {
    let fixture = " first\n  second \\\n  third  \n";
    for (lines, expected) in [
        (vec!["first", "second", "third"], None),
        (vec!["first", "missing", "third"], Some("missing")),
        (vec!["second", "first"], Some("first")),
        (vec![" second "], None),
        (vec!["first", "first"], Some("first")),
        (vec![], None),
    ] {
        assert_eq!(missing_line(fixture, &lines), expected, "pins: {lines:?}");
    }
}

#[test]
fn correctness_ci_mixins_follow_the_listed_realization_and_roles() {
    for (realization, roles, expected) in [
        (
            "linux-container",
            json!([{"role": "agent-adp-linux-image"}]),
            vec![".test-correctness-definition", ".test-correctness-adp-baseline"],
        ),
        (
            "kubernetes-pod",
            json!([{"role": "tools-linux-image"}, {"role": "agent-adp-linux-image"}]),
            vec![".test-correctness-kind-definition", ".test-correctness-adp-baseline"],
        ),
        (
            "linux-container",
            json!([{"role": "tools-linux-image"}]),
            vec![".test-correctness-definition"],
        ),
        ("kubernetes-pod", json!([]), vec![".test-correctness-kind-definition"]),
    ] {
        let listed = json!({
            "status": "runnable",
            "layout": [{"realization": "linux-container"}, {"realization": realization}],
            "roles": roles,
        });
        assert_eq!(correctness_ci_mixins(&listed), expected, "{listed}");
    }
}

#[test]
#[should_panic(expected = "a runnable row must have a layout array")]
fn correctness_ci_mixins_reject_a_listed_row_without_a_layout() {
    correctness_ci_mixins(&json!({"status": "runnable", "layout": null, "roles": []}));
}

#[test]
#[should_panic(expected = "a runnable row must have a roles array")]
fn correctness_ci_mixins_reject_a_listed_row_without_roles() {
    correctness_ci_mixins(&json!({"status": "runnable", "layout": [], "roles": null}));
}

#[test]
#[should_panic(expected = "a correctness CI row must be runnable")]
fn correctness_ci_mixins_reject_a_skipped_row() {
    correctness_ci_mixins(&json!({"status": "skipped", "layout": null, "roles": null}));
}

#[test]
fn the_listener_failure_cases_run_natively_on_linux_and_macos() {
    let root = repository_root().join("test/integration/cases");
    for name in [
        "dogstatsd-no-listener-continues",
        "dogstatsd-require-listener-exit",
        "dogstatsd-skip-failed-listener",
    ] {
        for os in [Os::Linux, Os::MacOs] {
            let case = crate::CASE_LOADER
                .load_file(&root.join(name).join("config.yaml"))
                .unwrap()
                .remove(0);
            assert_eq!(
                case.subject_os().iter().copied().collect::<Vec<_>>(),
                [Os::Linux, Os::MacOs]
            );
            let plan = planner().compile(case, os).unwrap().into_plan().unwrap();
            assert_eq!(plan.case().name(), name);
        }
    }
}

#[test]
fn every_selected_correctness_case_declares_a_local_baseline_image() {
    let inventory = Inventory::generate(&repository_root());
    for row in inventory
        .rows
        .iter()
        .filter(|row| row.suite == CORRECTNESS && row.status == SELECTED)
    {
        assert!(
            row.images["baseline"].starts_with("saluki-images/"),
            "{}: CI's manifest binds both baseline and comparison to agent-adp-linux-image, so a published baseline would be silently replaced; declare a saluki-images/ baseline",
            row.name,
        );
        assert!(
            row.ci_mixins
                .iter()
                .any(|mixin| mixin == ".test-correctness-adp-baseline"),
            "{} must extend .test-correctness-adp-baseline",
            row.name,
        );
    }
}

#[test]
fn container_daemon_tests_run_in_a_separate_ci_job() {
    let e2e: Value = serde_saphyr::from_str(&read_text(&repository_root().join(".gitlab/e2e.yml"))).unwrap();
    let daemon = &e2e["test-container-daemon"];
    assert_eq!(daemon["needs"], json!([]));
    let image = daemon["variables"]["AIRLOCK_TEST_IMAGE"].as_str().unwrap();
    assert_eq!(daemon["variables"]["PANORAMIC_TEST_IMAGE"], image);
    let script = daemon["script"].as_array().unwrap();
    assert!(script.contains(&json!(format!("docker pull {image}"))));
    let integration = e2e["test-integration"]["script"].as_array().unwrap();
    for command in ["make test-airlock-daemon", "make test-panoramic-daemon"] {
        assert!(script.contains(&json!(command)), "{command}");
        assert!(!integration.contains(&json!(command)), "{command}");
    }
}

#[test]
fn the_integration_job_runs_the_binaries_build_panoramic_binary_ships() {
    // test-integration runs the panoramic and courier binaries that build-panoramic-binary ships, without compiling.
    let e2e: Value = serde_saphyr::from_str(&read_text(&repository_root().join(".gitlab/e2e.yml"))).unwrap();
    let integration = &e2e["test-integration"];
    assert!(integration["needs"]
        .as_array()
        .unwrap()
        .contains(&json!({"job": "build-panoramic-binary", "artifacts": true})));
    let script: Vec<_> = integration["script"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command.as_str().unwrap())
        .collect();
    assert_eq!(
        script
            .iter()
            .copied()
            .filter(|command| command.starts_with("make "))
            .collect::<Vec<_>>(),
        ["make run-integration-quick PANORAMIC_ARTIFACTS=target/panoramic-artifacts.toml"]
    );
    assert!(script.iter().all(|command| !command.contains("cargo ")));
}

#[cfg(unix)]
#[test]
fn quick_integration_ci_target_runs_with_the_manifest_without_building() {
    let root = repository_root();
    let output = std::process::Command::new("make")
        .current_dir(&root)
        .env_remove("MAKEFLAGS")
        .env_remove("MFLAGS")
        .env_remove("MAKELEVEL")
        .env_remove("PANORAMIC_ARTIFACTS")
        .args([
            "--no-print-directory",
            "--dry-run",
            "run-integration-quick",
            "PANORAMIC_ARTIFACTS=target/panoramic-artifacts.toml",
            "PANORAMIC_LOG_DIR=integration-logs",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let commands = stdout
        .lines()
        .filter(|line| !line.starts_with("echo "))
        .collect::<Vec<_>>();
    let [command] = commands.as_slice() else {
        panic!("expected one command, got {commands:?}");
    };
    // Make expands `$(shell pwd)`, which may name the checkout through a symlink (macOS `/var` is
    // `/private/var`), so compare the case directory by what it resolves to.
    let (before, rest) = command.split_once(" -d ").unwrap();
    let (cases, after) = rest.split_once(' ').unwrap();
    assert_eq!(before, "target/release/panoramic run --subject-os linux");
    assert_eq!(
        std::fs::canonicalize(cases).unwrap(),
        std::fs::canonicalize(root.join("test/integration/cases")).unwrap()
    );
    assert_eq!(
        after,
        "-l integration-logs --artifacts \"target/panoramic-artifacts.toml\""
    );
}

#[test]
fn ci_binds_the_courier_its_jobs_build() {
    let root = repository_root();
    let e2e: Value = serde_saphyr::from_str(&read_text(&root.join(".gitlab/e2e.yml"))).unwrap();
    let mixins: Value = serde_saphyr::from_str(&read_text(&root.join(".gitlab/correctness-mixins.yml"))).unwrap();
    let expected_path = "target/x86_64-unknown-linux-musl/release/panoramic-courier";
    assert_eq!(
        e2e["build-panoramic-binary"]["artifacts"]["paths"],
        json!(["target/release/panoramic", expected_path])
    );
    for job in [&e2e["test-integration"], &mixins[".test-correctness-definition"]] {
        let bindings: Vec<_> = job["script"]
            .as_array()
            .unwrap()
            .iter()
            .map(|command| command.as_str().unwrap())
            .filter(|command| command.starts_with("target/release/panoramic artifacts render "))
            .flat_map(str::split_whitespace)
            .filter_map(|argument| argument.strip_prefix("courier-linux-binary="))
            .collect();
        assert_eq!(bindings, [expected_path]);
    }
    assert_lines_in_order(
        "Makefile",
        &read_text(&root.join("Makefile")),
        &[r#"COURIER_LINUX_TARGET ?= $(shell rustc -vV | sed -n 's/^host: \([^-]*\)-.*/\1/p')-unknown-linux-musl"#],
    );
}

#[test]
fn ci_jobs_in_the_inventory_are_defined() {
    let root = repository_root();
    let inventory = Inventory::generate(&root);
    let e2e = read_text(&root.join(".gitlab/e2e.yml"));
    let windows = read_text(&root.join(".gitlab/windows.yml"));
    let mixins = read_text(&root.join(".gitlab/correctness-mixins.yml"));
    let makefile = read_text(&root.join("Makefile"));
    let windows_script = read_text(&root.join("ci/tooling/windows-integration-tests.ps1"));
    let has_job = |text: &str, job: &str| text.lines().any(|line| line == format!("{job}:"));
    assert!(
        has_job(&e2e, "run-correctness-tests"),
        "Missing correctness child pipeline"
    );
    assert!(
        e2e.lines().any(|line| {
            line.trim() == "- bash ci/tooling/generate-correctness-pipeline.sh > generated-correctness.yml"
        }),
        "CI must run the correctness generator without masking its exit status"
    );
    for row in &inventory.rows {
        if row.suite == INTEGRATION {
            let ci = if row.subject_os == "windows" { &windows } else { &e2e };
            for job in &row.ci_jobs {
                assert!(has_job(ci, job), "Missing CI job: {job}");
            }
        }
        for mixin in &row.ci_mixins {
            assert!(has_job(&mixins, mixin), "Missing CI mixin: {mixin}");
        }
    }
    assert_ci_block_lines_in_order(
        ".gitlab/e2e.yml",
        &e2e,
        &[
            "build-panoramic-binary:",
            "- make build-panoramic",
            "- make build-courier-linux",
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            "build-courier-linux: check-rust-build-tools",
            "@rustup target add $(COURIER_LINUX_TARGET)",
            "@cargo build --profile release --package panoramic-courier --bin panoramic-courier --target $(COURIER_LINUX_TARGET)",
        ],
    );
    assert_ci_block_lines_in_order(
        ".gitlab/e2e.yml",
        &e2e,
        &[
            r#"test-integration:"#,
            r#"- target/release/panoramic artifacts render"#,
            r#"--set agent-adp-linux-image=${SALUKI_IMAGE_REPO_BASE}/bundled-agent-adp:${CI_COMMIT_SHA}-${CI_PIPELINE_ID}"#,
            r#"--set tools-linux-image=${SALUKI_IMAGE_REPO_BASE}/correctness-tools:${CI_COMMIT_SHA}"#,
            r#"--set fixup-image=registry.ddbuild.io/alpine:latest"#,
            r#"--set courier-linux-binary=target/x86_64-unknown-linux-musl/release/panoramic-courier"#,
            r#"--output target/panoramic-artifacts.toml"#,
            r#"- target/release/panoramic artifacts pull -d test/integration/cases --subject-os linux --artifacts target/panoramic-artifacts.toml"#,
            r#"- make run-integration-quick PANORAMIC_ARTIFACTS=target/panoramic-artifacts.toml"#,
        ],
    );
    assert_ci_block_lines_in_order(
        ".gitlab/correctness-mixins.yml",
        &mixins,
        &[
            r#".test-correctness-definition:"#,
            r#"- target/release/panoramic artifacts render"#,
            r#"--set agent-adp-linux-image=${SALUKI_IMAGE_REPO_BASE}/bundled-agent-adp:${CI_COMMIT_SHA}-${PARENT_PIPELINE_ID}"#,
            r#"--set tools-linux-image=${SALUKI_IMAGE_REPO_BASE}/correctness-tools:${CI_COMMIT_SHA}"#,
            r#"--set fixup-image=registry.ddbuild.io/alpine:latest"#,
            r#"--set courier-linux-binary=target/x86_64-unknown-linux-musl/release/panoramic-courier"#,
            r#"--output target/panoramic-artifacts.toml"#,
            r#"- target/release/panoramic artifacts pull -d $(pwd)/test/correctness/cases --subject-os linux --artifacts target/panoramic-artifacts.toml"#,
            r#"- target/release/panoramic run -d $(pwd)/test/correctness/cases -t ${CORRECTNESS_TEST_CASE} --subject-os linux --artifacts target/panoramic-artifacts.toml --no-tui"#,
        ],
    );
    assert_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"PANORAMIC_ARTIFACTS_ARG = $(if $(filter environment%,$(origin PANORAMIC_ARTIFACTS)),$(error PANORAMIC_ARTIFACTS must be set on the make command line, not in the environment),$(if $(PANORAMIC_ARTIFACTS),--artifacts "$(PANORAMIC_ARTIFACTS)"))"#,
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"test-correctness: build-panoramic build-courier-linux build-correctness-tools-image build-datadog-agent-image-release"#,
            r#"@target/release/panoramic run --subject-os linux -d $(shell pwd)/test/correctness/cases $(if $(PANORAMIC_PARALLELISM),-p $(PANORAMIC_PARALLELISM)) $(PANORAMIC_ARTIFACTS_ARG)"#,
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"test-correctness-case: build-panoramic build-courier-linux build-correctness-tools-image build-datadog-agent-image-release"#,
            r#"@target/release/panoramic run --subject-os linux -d $(shell pwd)/test/correctness/cases -t $(CASE) --no-tui $(PANORAMIC_ARTIFACTS_ARG)"#,
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"test-integration: build-panoramic build-courier-linux build-datadog-agent-image build-correctness-tools-image"#,
            r#"@target/release/panoramic run --subject-os linux -d $(shell pwd)/test/integration/cases $(if $(PANORAMIC_LOG_DIR),-l $(PANORAMIC_LOG_DIR)) $(PANORAMIC_ARTIFACTS_ARG)"#,
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"test-integration-quick: build-panoramic build-courier-linux"#,
            r#"@$(MAKE) run-integration-quick"#,
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"run-integration-quick: ## Runs ADP integration tests using built binaries and images"#,
            r#"@target/release/panoramic run --subject-os linux -d $(shell pwd)/test/integration/cases $(if $(PANORAMIC_LOG_DIR),-l $(PANORAMIC_LOG_DIR)) $(PANORAMIC_ARTIFACTS_ARG)"#,
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"list-integration-tests: build-panoramic"#,
            r#"@target/release/panoramic list --subject-os linux -d $(shell pwd)/test/integration/cases"#,
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"test-integration-macos-run: ## Runs macOS cases with already-built binaries and the artifact manifest; narrow with CASE=<name>."#,
            r#"@target/release/panoramic run -d "$(CURDIR)/test/integration/cases""#,
            r#"--subject-os macos"#,
            r#"$(PANORAMIC_ARTIFACTS_ARG)"#,
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            "build-intake-host: check-rust-build-tools",
            "@cargo build --profile release --package datadog-intake --bin datadog-intake",
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"render-macos-test-artifacts: build-panoramic"#,
            r#"@target/release/panoramic artifacts render"#,
            r#"--set adp-host-binary="$(CURDIR)/target/$(BUILD_PROFILE)/agent-data-plane""#,
            r#"--set agent-host-install="$(MACOS_TEST_AGENT_INSTALL_DIR)""#,
            r#"--set intake-host-binary="$(CURDIR)/target/release/datadog-intake""#,
            r#"--output "$(MACOS_TEST_ARTIFACTS)""#,
        ],
    );
    assert_ci_block_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"test-integration-macos-ci: override PANORAMIC_ARTIFACTS = $(MACOS_TEST_ARTIFACTS)"#,
            r#"test-integration-macos-ci: build-panoramic build-adp-host build-intake-host provision-macos-test-env render-macos-test-artifacts test-integration-macos-run ## CI entry point: builds binaries, provisions the Agent and cert, renders artifacts, and runs macOS integration tests"#,
        ],
    );
    assert_lines_in_order(
        "ci/tooling/windows-integration-tests.ps1",
        &windows_script,
        &[
            r#"Invoke-Native cargo build --release --package datadog-intake --bin datadog-intake"#,
            r#"Build-WindowsToolsImage -ImageTag $ToolsImageTag"#,
            r#"$Manifest = Join-Path $env:CARGO_TARGET_DIR "panoramic-artifacts.toml""#,
            r#"Invoke-Native -FilePath $Panoramic -Arguments @("artifacts", "render", "--set", "agent-adp-windows-image=$ImageTag", "--set", "tools-windows-image=$ToolsImageTag", "--set", "courier-windows-binary=$Courier", "--output", $Manifest)"#,
            r#""run","#,
            r#""--subject-os", "windows","#,
            r#""--artifacts", $Manifest,"#,
            r#"Invoke-Native -FilePath $Panoramic -Arguments $PanoramicArgs"#,
        ],
    );
    for file in ["agent-data-plane.exe", "auth_token", "ipc_cert.pem"] {
        assert!(
            windows_script.contains(&format!("COPY {file} ")),
            "Windows image must contain {file}"
        );
    }
    let script = read_text(&root.join("ci/tooling/generate-correctness-pipeline.sh"));
    let position = |rule: &str| {
        script
            .lines()
            .position(|line| line.trim() == rule || line.trim() == format!("{rule} \\"))
    };
    let list_rule = "./target/release/panoramic list -d test/correctness/cases --subject-os linux --json";
    for rule in [
        "set -euo pipefail",
        list_rule,
        r#"| jq -r 'to_entries[] | select(if .value.status == "skipped" then false elif .value.status == "runnable" then true else error("unknown status for \(.key)") end) | "\(.key)\t\(any(.value.layout[]; .realization == "kubernetes-pod"))\t\(any(.value.roles[]; .role == "agent-adp-linux-image"))"'"#,
        r#"| while IFS=$'\t' read -r case pod agent_adp_image; do"#,
        r#"if [[ "${pod}" == "true" ]]; then"#,
        "base_mixin=\".test-correctness-kind-definition\"",
        "base_mixin=\".test-correctness-definition\"",
        r#"if [[ "${agent_adp_image}" == "true" ]]; then"#,
        "extends=\"[${base_mixin}, .test-correctness-adp-baseline]\"",
        "extends=\"[${base_mixin}]\"",
        r#"job_name="${case//\//-}""#,
        "test-correctness-${job_name}:",
    ] {
        assert!(position(rule).is_some(), "CI generator rule changed: {rule}");
    }
    // Without pipefail the pipeline's status is the while loop's, so a failing list would silently drop jobs.
    assert!(
        position("set -euo pipefail") < position(list_rule),
        "CI generator must enable fail-fast shell options before listing cases"
    );
}

#[test]
fn ignored_key_probe_reports_only_keys_the_loader_drops() {
    let mut correctness: Value = serde_saphyr::from_str(
        r#"
type: correctness
runtime: docker
analysis_mode: metrics
millstone: {image: saluki-images/correctness-tools:latest, config_path: millstone.yaml}
datadog_intake: {image: saluki-images/correctness-tools:latest, config_path: intake.yaml}
baseline: {image: baseline, env: {DD_API_KEY: test}}
comparison: {image: comparison}
"#,
    )
    .unwrap();
    assert_eq!(
        ignored_fields(
            &correctness,
            &|doc| loaded_view("cases", &doc, "probe", Path::new("probe/config.yaml")),
            Path::new("probe/config.yaml"),
        ),
        ["datadog_intake.config_path"],
    );
    correctness["type"] = json!("correctness_matrix");
    correctness["variants"] = json!([{ "name": "one", "env": {"DD_TAGS": "a:b"} }]);
    assert_eq!(
        ignored_fields(
            &correctness,
            &|doc| loaded_view("cases", &doc, "probe", Path::new("probe/config.yaml")),
            Path::new("probe/config.yaml"),
        ),
        ["datadog_intake.config_path"],
    );
    let mut participant: Value = serde_saphyr::from_str(LOGGING_PARTICIPANT).unwrap();
    assert_eq!(
        ignored_fields(
            &participant,
            &|doc| loaded_view("cases", &doc, "alpha", Path::new("alpha/config.yaml")),
            Path::new("alpha/config.yaml"),
        ),
        Vec::<String>::new(),
    );
    let baseline = loaded_view("cases", &participant, "alpha", Path::new("alpha/config.yaml")).unwrap();
    participant["participants"]["adp"]["config"]["env"]["DD_API_KEY"] = json!("x");
    let changed = loaded_view("cases", &participant, "alpha", Path::new("alpha/config.yaml")).unwrap();
    assert_ne!(changed, baseline);
    assert_eq!(changed[0][1]["linux"]["participants"]["adp"]["env"]["DD_API_KEY"], "x");
}

#[test]
fn loaded_view_records_participant_realizations() {
    let doc: Value = serde_saphyr::from_str(
        &PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: linux-container}"),
    )
    .unwrap();
    let view = loaded_view("cases", &doc, "alpha", Path::new("alpha/config.yaml")).unwrap();
    assert_eq!(
        view[0][1]["linux"]["participants"]["adp"]["realization"],
        "linux-container"
    );
}

#[test]
fn migration_proofs_name_inventoried_rows_and_their_ci_evidence() {
    let root = repository_root();
    let inventory = Inventory::generate(&root);
    let markdown = read_text(&root.join(MARKDOWN_PATH));
    let (_, proofs, _) = marked_region(&markdown, PROOFS_BEGIN, PROOFS_END);
    check_migration_proofs(&inventory, proofs);
}

fn check_migration_proofs(inventory: &Inventory, proofs: &str) {
    let mut lines = proofs.lines().filter(|line| !line.trim().is_empty());
    assert_eq!(lines.next(), Some("| Case | Subject OS | Execution | Proof |"));
    assert_eq!(lines.next(), Some("| --- | --- | --- | --- |"));
    let mut proven_rows = BTreeSet::new();
    for line in lines {
        assert!(
            line.starts_with('|') && line.ends_with('|'),
            "Invalid proof row: {line}"
        );
        let columns: Vec<_> = line[1..line.len() - 1]
            .split('|')
            .map(|s| s.trim().trim_matches('`'))
            .collect();
        assert_eq!(columns.len(), 4, "Invalid proof row: {line}");
        let proof = columns[3];
        if proof.starts_with("Pending CI.") {
            assert_eq!(columns[2], NATIVE, "Only native rows may have a pending proof: {line}");
        } else {
            assert!(
                proof.contains(PROOF_PIPELINE_URL),
                "Migration proof must link its pipeline: {line}"
            );
            assert!(
                proof.contains(PROOF_JOB_URL),
                "Migration proof must link a passing job: {line}"
            );
            assert!(
                proof.contains(PROOF_COMPARISON),
                "Migration proof must name its comparison commit: {line}"
            );
        }
        assert!(
            proven_rows.insert((columns[0], columns[1], columns[2])),
            "Duplicate migration proof: {line}"
        );
        let matches = inventory
            .rows
            .iter()
            .filter(|row| row.name == columns[0] && row.subject_os == columns[1] && row.execution == columns[2])
            .count();
        assert_eq!(
            matches, 1,
            "Migration proof must name exactly one inventory row: {} / {}",
            columns[0], columns[1],
        );
    }
    for row in inventory.rows.iter().filter(|row| row.schema == PARTICIPANTS) {
        assert!(
            proven_rows.contains(&(row.name.as_str(), row.subject_os.as_str(), row.execution)),
            "Participant-schema row must have a migration proof: {} / {}",
            row.name,
            row.subject_os,
        );
    }
}

#[test]
fn migration_proofs_reject_wrong_execution_pending_old_rows_and_missing_participant_rows() {
    let root = repository_root();
    let inventory = Inventory::generate(&root);
    let markdown = read_text(&root.join(MARKDOWN_PATH));
    let (_, proofs, _) = marked_region(&markdown, PROOFS_BEGIN, PROOFS_END);
    for (invalid, expected) in [
        (
            proofs.replacen("| native | Pending CI.", "| old | Pending CI.", 1),
            "Only native rows may have a pending proof",
        ),
        (
            proofs.replacen("| native | Pending CI. Before: Tested", "| old | Tested", 1),
            "Migration proof must name exactly one inventory row",
        ),
        (
            proofs
                .lines()
                .filter(|line| !line.starts_with("| adp-logging-default-path |"))
                .collect::<Vec<_>>()
                .join("\n"),
            "Participant-schema row must have a migration proof",
        ),
    ] {
        let panic = std::panic::catch_unwind(|| check_migration_proofs(&inventory, &invalid)).unwrap_err();
        let message = panic.downcast::<String>().unwrap();
        assert!(message.contains(expected), "{message}");
    }
}

mod logging_baseline;
