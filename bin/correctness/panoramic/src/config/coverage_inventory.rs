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

use saluki_error::{generic_error, GenericError};
use serde::Serialize;
use serde_json::{json, Value};

use super::{
    build_runners,
    case::{CaseConfig, CaseSchema, Os},
    discover_cases,
    tests::{write_case, LOGGING_INTEGRATION, LOGGING_PARTICIPANT, PARTICIPANT_FIELDS},
    AssertionStep, IntegrationConfig, LINUX_RUNTIME, MAC_RUNTIME, WINDOWS_RUNTIME,
};
use crate::{
    correctness::analysis::AnalysisMode,
    test::{Test, TestSuite},
};

const JSON_PATH: &str = "docs/development/plans/panoramic-architecture-inventory.json";
const MARKDOWN_PATH: &str = "docs/development/plans/panoramic-architecture.md";
const COVERAGE_BEGIN: &str = "<!-- coverage-inventory:begin -->";
const COVERAGE_END: &str = "<!-- coverage-inventory:end -->";
const PROOFS_BEGIN: &str = "<!-- migration-proofs:begin -->";
const PROOFS_END: &str = "<!-- migration-proofs:end -->";
const REGENERATE: &str = "PANORAMIC_WRITE_COVERAGE_INVENTORY=1 cargo nextest run -p panoramic coverage_inventory";
const INTEGRATION: &str = "integration";
const CORRECTNESS: &str = "correctness";
const PARTICIPANTS: &str = "participants";
const SELECTED: &str = "selected";
const DISABLED: &str = "disabled";
const OLD: &str = "old";

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
    suite: &'static str,
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

impl Steps {
    fn from_config(config: &IntegrationConfig) -> Self {
        let mut steps = Self::default();
        for step in &config.procedure {
            let tagged = match step {
                AssertionStep::Single(assertion) => {
                    steps.single_assertions += 1;
                    vec![serde_json::to_value(assertion).unwrap()]
                }
                AssertionStep::Action(action) => {
                    steps.actions += 1;
                    vec![serde_json::to_value(action).unwrap()]
                }
                AssertionStep::Parallel { parallel } => {
                    steps.parallel_blocks += 1;
                    steps.parallel_assertions += parallel.len();
                    parallel.iter().map(|a| serde_json::to_value(a).unwrap()).collect()
                }
            };
            for value in tagged {
                let kind = value.get("assertion").or_else(|| value.get("action")).unwrap();
                *steps.by_kind.entry(kind.as_str().unwrap().to_string()).or_default() += 1;
            }
        }
        steps
    }
}

impl Row {
    fn from_case(
        root: &Path, case: &CaseConfig, runner: &dyn Test, subject_os: &str, status: &'static str,
        schemas: &BTreeMap<String, String>,
    ) -> Self {
        let source = relative_path(root, case.source());
        let schema = match case.schema() {
            CaseSchema::Participants { .. } => PARTICIPANTS.to_string(),
            _ => schemas[&source].clone(),
        };
        let suite = suite_label(case.suite());
        let mut row = Self {
            case: format!("{suite}/{}", case.name()),
            name: case.name().to_string(),
            suite,
            subject_os: subject_os.to_string(),
            runtime: runner.runtime(),
            source,
            schema,
            execution: OLD,
            status,
            timeout: format!("{:?}", case.timeout()),
            images: runner.images().into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            ci_jobs: Vec::new(),
            ci_mixins: Vec::new(),
        };
        row.assign_ci();
        row
    }

    /// Assigns the jobs and templates selected by the GitLab definitions and correctness pipeline generator.
    fn assign_ci(&mut self) {
        if self.status == DISABLED {
            return;
        }
        // Integration jobs: .gitlab/e2e.yml and .gitlab/windows.yml (windows-integration-tests.ps1).
        // Correctness jobs and mixins: ci/tooling/generate-correctness-pipeline.sh.
        match (self.suite, self.subject_os.as_str()) {
            (INTEGRATION, "linux") => self.ci_jobs.push("test-integration".to_string()),
            (INTEGRATION, "macos") => self.ci_jobs.extend([
                "test-integration-macos-arm64".to_string(),
                "test-integration-macos-amd64".to_string(),
            ]),
            (INTEGRATION, "windows") => self.ci_jobs.push("test-integration-windows-amd64".to_string()),
            (CORRECTNESS, "linux") => {
                self.ci_jobs
                    .push(format!("test-correctness-{}", self.name.replace('/', "-")));
                self.ci_mixins.push(
                    if self.runtime == "kubernetes_in_docker" {
                        ".test-correctness-kind-definition"
                    } else {
                        ".test-correctness-definition"
                    }
                    .to_string(),
                );
                if self.images["baseline"].starts_with("saluki-images/") {
                    self.ci_mixins.push(".test-correctness-adp-baseline".to_string());
                }
            }
            key => panic!("No CI mapping for {key:?}"),
        }
    }
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
                    if suite_dir == "test/integration/cases" {
                        assert!(
                            doc.get("runtimes").is_some(),
                            "{source} must declare runtimes: the loader default depends on the host OS",
                        );
                    }
                    schemas.insert(source, schema.to_string());
                }
            }
        }
        // Discover all directories together to reject duplicate names across suites and statuses.
        let cases =
            discover_cases(&CASE_DIRS.map(|(dir, ..)| root.join(dir))).unwrap_or_else(|error| panic!("{error:#}"));
        let (integration_cases, correctness_cases): (Vec<_>, Vec<_>) =
            cases.iter().partition(|case| case.suite() == TestSuite::Integration);
        for (runtime, os) in [
            (LINUX_RUNTIME, Os::Linux),
            (MAC_RUNTIME, Os::MacOs),
            (WINDOWS_RUNTIME, Os::Windows),
        ] {
            let eligible: Vec<_> = integration_cases
                .iter()
                .copied()
                .filter(|case| case.is_eligible_for(runtime))
                .collect();
            let runners = build_runners(eligible.iter().copied(), runtime, &[]).unwrap();
            for (case, runner) in eligible.into_iter().zip(runners) {
                rows.push(Row::from_case(
                    root,
                    case,
                    runner.as_ref(),
                    os.name(),
                    SELECTED,
                    &schemas,
                ));
            }
        }
        for case in correctness_cases {
            let runners = build_runners(std::slice::from_ref(case), LINUX_RUNTIME, &[]).unwrap();
            let status = if case.is_eligible_for(LINUX_RUNTIME) {
                SELECTED
            } else {
                DISABLED
            };
            rows.push(Row::from_case(
                root,
                case,
                runners[0].as_ref(),
                Os::Linux.name(),
                status,
                &schemas,
            ));
        }

        rows.sort_by(|a, b| (&a.suite, &a.name, &a.subject_os).cmp(&(&b.suite, &b.name, &b.subject_os)));
        let mut keys = BTreeSet::new();
        for row in &rows {
            let source = &row.source;
            let suite = row.suite;
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

fn suite_label(suite: TestSuite) -> &'static str {
    match suite {
        TestSuite::Integration => INTEGRATION,
        TestSuite::Correctness => CORRECTNESS,
    }
}

fn load_cases(path: &Path) -> Vec<(String, Case)> {
    let loaded = CaseConfig::load_file(path).unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
    let mut cases = record_cases(&loaded).unwrap_or_else(|error| panic!("{}: {error:#}", path.display()));
    let doc: Value = serde_saphyr::from_str(&read_text(path)).unwrap();
    let directory = path.parent().unwrap().file_name().unwrap().to_str().unwrap();
    let canonical = path.canonicalize().unwrap();
    let view = |doc| loaded_view(&doc, directory, &canonical);
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
fn record_cases(cases: &[CaseConfig]) -> Result<Vec<(String, Case)>, GenericError> {
    let mut recorded = Vec::new();
    for case in cases {
        let effective = match case.schema() {
            CaseSchema::Integration(config) => Case {
                config: serde_json::to_value(config)?,
                steps: Some(Steps::from_config(config)),
                comparison: None,
                ignored_fields: Vec::new(),
            },
            CaseSchema::Correctness(config) => Case {
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
            CaseSchema::Participants { .. } => {
                let mut subject_os = case.subject_os().iter();
                let first_os = *subject_os.next().expect("loaded cases declare at least one subject OS");
                let translated = case.integration_config(first_os)?;
                let config = serde_json::to_value(&translated)?;
                for os in subject_os {
                    if serde_json::to_value(case.integration_config(*os)?)? != config {
                        return Err(generic_error!(
                            "case '{}' translates to different integration configurations on {} and {}; the coverage inventory records one configuration per case",
                            case.name(), first_os.name(), os.name()
                        ));
                    }
                }
                Case {
                    config,
                    steps: Some(Steps::from_config(&translated)),
                    comparison: None,
                    ignored_fields: Vec::new(),
                }
            }
        };
        let suite = suite_label(case.suite());
        recorded.push((format!("{suite}/{}", case.name()), effective));
    }
    Ok(recorded)
}

/// Loads `doc` as the file at `source` and returns each case's recorded configuration.
///
/// JSON text is valid YAML and preserves strings as strings. `load_cases` checks it against the original YAML.
fn loaded_view(doc: &Value, directory: &str, source: &Path) -> Result<Value, GenericError> {
    let content = serde_json::to_string(doc)?;
    let cases = CaseConfig::load_content(&content, directory, source)?;
    let recorded = record_cases(&cases)?;
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
fn a_participant_case_and_its_integration_twin_produce_the_same_inventory_apart_from_the_schema_column() {
    for (participant_yaml, integration_yaml, oses) in [
        (
            LOGGING_PARTICIPANT.to_string(),
            LOGGING_INTEGRATION.to_string(),
            vec!["linux", "macos", "windows"],
        ),
        (
            LOGGING_PARTICIPANT
                .replace("linux, macos, windows", "linux")
                .replace(r"C:\adp\adp.log", "{{ adp.facts.default_log_file }}"),
            LOGGING_INTEGRATION
                .replace("linux, mac, windows", "linux")
                .replace(r"C:\adp\adp.log", "/var/log/datadog/agent-data-plane.log"),
            vec!["linux"],
        ),
    ] {
        let participant_root = tempfile::tempdir().unwrap();
        let integration_root = tempfile::tempdir().unwrap();
        for (root, yaml) in [
            (participant_root.path(), &participant_yaml),
            (integration_root.path(), &integration_yaml),
        ] {
            create_case_dirs(root);
            write_case(&root.join("test/integration/cases"), "alpha", yaml);
        }
        let mut participant =
            serde_json::to_value(Inventory::generate(&participant_root.path().canonicalize().unwrap())).unwrap();
        let integration =
            serde_json::to_value(Inventory::generate(&integration_root.path().canonicalize().unwrap())).unwrap();
        let participant_rows = participant["rows"].as_array_mut().unwrap();
        let integration_rows = integration["rows"].as_array().unwrap();
        assert_eq!(participant_rows.len(), oses.len());
        assert_eq!(integration_rows.len(), oses.len());
        for ((participant, integration), os) in participant_rows.iter_mut().zip(integration_rows).zip(oses) {
            assert_eq!(participant["subject_os"], os);
            assert_eq!(participant["schema"], PARTICIPANTS);
            assert_eq!(participant["execution"], OLD);
            assert_eq!(integration["schema"], INTEGRATION);
            participant["schema"] = integration["schema"].clone();
        }
        assert_eq!(participant, integration);
    }
}

#[test]
#[should_panic(
    expected = "case 'alpha' translates to different integration configurations on linux and windows; the coverage inventory records one configuration per case"
)]
fn inventory_rejects_participant_cases_whose_configuration_differs_by_os() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    let yaml = LOGGING_PARTICIPANT
        .replace("linux, macos, windows", "linux, windows")
        .replace(r"C:\adp\adp.log", "{{ adp.facts.default_log_file }}");
    write_case(&root.path().join("test/integration/cases"), "alpha", &yaml);
    Inventory::generate(&root.path().canonicalize().unwrap());
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
        "type: integration\nname: shared\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n",
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
#[should_panic(expected = "a selected integration case belongs in test/integration/cases/")]
fn inventory_rejects_an_integration_case_in_a_correctness_directory() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    write_case(
        &root.path().join("test/correctness/cases"),
        "alpha",
        "type: integration\nname: alpha\ntimeout: 1s\nruntimes: [linux]\nprocedure: []\n",
    );
    Inventory::generate(&root.path().canonicalize().unwrap());
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
#[should_panic(expected = "must be a string: 'integration', 'correctness' or 'correctness_matrix'")]
fn inventory_reports_the_loader_error_for_a_non_string_type() {
    let root = tempfile::tempdir().unwrap();
    create_case_dirs(root.path());
    write_case(&root.path().join("test/integration/cases"), "alpha", "type: 3\n");
    Inventory::generate(root.path());
}

#[test]
fn ci_jobs_in_the_inventory_are_defined() {
    let root = repository_root();
    let inventory = Inventory::generate(&root);
    let e2e = read_text(&root.join(".gitlab/e2e.yml"));
    let windows = read_text(&root.join(".gitlab/windows.yml"));
    let mixins = read_text(&root.join(".gitlab/correctness-mixins.yml"));
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
    let script = read_text(&root.join("ci/tooling/generate-correctness-pipeline.sh"));
    let position = |rule: &str| {
        script
            .lines()
            .position(|line| line.trim() == rule || line.trim() == format!("{rule} \\"))
    };
    let list_rule = "./target/release/panoramic list -d test/correctness/cases --json";
    for rule in [
        "set -euo pipefail",
        list_rule,
        "if [[ \"${runtime}\" == \"kubernetes_in_docker\" ]]; then",
        "base_mixin=\".test-correctness-kind-definition\"",
        "base_mixin=\".test-correctness-definition\"",
        "if [[ \"${baseline}\" == saluki-images/* ]]; then",
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
    let integration: Value = serde_saphyr::from_str(
        r#"
type: integration
name: probe
timeout: 30s
runtimes: [linux]
container: {host_cgroup_namespace: false}
intake: {enabled: false}
env: {DD_API_KEY: test}
extra: {nested: 1}
procedure:
  - action: adp_cli
    args: [status]
    output_contains: healthy
  - assertion: log_contains
    pattern: ready
    regex: false
    timeout: 1s
  - assertion: adp_config_key_equals
    key: example
    value: {nested: [{key: expected}]}
    timeout: 1s
"#,
    )
    .unwrap();
    assert_eq!(
        ignored_fields(
            &integration,
            &|doc| loaded_view(&doc, "probe", Path::new("probe/config.yaml")),
            Path::new("probe/config.yaml"),
        ),
        ["extra", "extra.nested", "procedure[0].output_contains"],
    );
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
            &|doc| loaded_view(&doc, "probe", Path::new("probe/config.yaml")),
            Path::new("probe/config.yaml"),
        ),
        ["datadog_intake.config_path"],
    );
    correctness["type"] = json!("correctness_matrix");
    correctness["variants"] = json!([{ "name": "one", "env": {"DD_TAGS": "a:b"} }]);
    assert_eq!(
        ignored_fields(
            &correctness,
            &|doc| loaded_view(&doc, "probe", Path::new("probe/config.yaml")),
            Path::new("probe/config.yaml"),
        ),
        ["datadog_intake.config_path"],
    );
    let mut participant: Value = serde_saphyr::from_str(LOGGING_PARTICIPANT).unwrap();
    assert_eq!(
        ignored_fields(
            &participant,
            &|doc| loaded_view(&doc, "alpha", Path::new("alpha/config.yaml")),
            Path::new("alpha/config.yaml"),
        ),
        Vec::<String>::new(),
    );
    let baseline = loaded_view(&participant, "alpha", Path::new("alpha/config.yaml")).unwrap();
    participant["participants"]["adp"]["config"]["env"]["DD_API_KEY"] = json!("x");
    let changed = loaded_view(&participant, "alpha", Path::new("alpha/config.yaml")).unwrap();
    assert_ne!(changed, baseline);
    assert_eq!(changed[0][1]["env"]["DD_API_KEY"], "x");
}

#[test]
fn loaded_view_fails_for_a_participant_case_the_translation_cannot_run() {
    let doc: Value = serde_saphyr::from_str(
        &PARTICIPANT_FIELDS.replace("subject: true", "subject: true, realization: {kind: linux-container}"),
    )
    .unwrap();
    let error = loaded_view(&doc, "alpha", Path::new("alpha/config.yaml")).unwrap_err();
    assert_eq!(
        error.to_string(),
        "Cannot run case 'alpha' from alpha/config.yaml: \
         participant 'adp' sets 'realization', which the integration runner does not support"
    );
}

#[test]
fn migration_proofs_name_inventoried_rows() {
    let root = repository_root();
    let inventory = Inventory::generate(&root);
    let markdown = read_text(&root.join(MARKDOWN_PATH));
    let (_, proofs, _) = marked_region(&markdown, PROOFS_BEGIN, PROOFS_END);
    let mut lines = proofs.lines().filter(|line| !line.trim().is_empty());
    assert_eq!(lines.next(), Some("| Case | Subject OS | Proof |"));
    assert_eq!(lines.next(), Some("| --- | --- | --- |"));
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
        assert_eq!(columns.len(), 3, "Invalid proof row: {line}");
        assert!(!columns[2].is_empty(), "Missing migration proof: {line}");
        proven_rows.insert((columns[0], columns[1]));
        let matches = inventory
            .rows
            .iter()
            .filter(|row| row.name == columns[0] && row.subject_os == columns[1])
            .count();
        assert_eq!(
            matches, 1,
            "Migration proof must name exactly one inventory row: {} / {}",
            columns[0], columns[1],
        );
    }
    for row in inventory.rows.iter().filter(|row| row.schema == PARTICIPANTS) {
        assert!(
            proven_rows.contains(&(row.name.as_str(), row.subject_os.as_str())),
            "Participant-schema row must have a migration proof: {} / {}",
            row.name,
            row.subject_os,
        );
    }
}

mod bridge_equivalence;
