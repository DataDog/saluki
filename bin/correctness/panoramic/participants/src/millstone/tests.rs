use panoramic_core::{
    case::FileSource,
    endpoint::test_support::{http_address, unix_stream_address},
    recipe::test_support::{assert_container_command, assert_pod, files, late_files, rendered},
};
use serde_json::json;

use super::*;

const LINUX: Realization = Realization {
    os: Os::Linux,
    kind: RealizationKind::LinuxContainer,
};
const POD: Realization = Realization {
    os: Os::Linux,
    kind: RealizationKind::KubernetesPod,
};
const SEED: [u8; 32] = [
    2, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37, 41, 43, 47, 53, 59, 61, 67, 71, 73, 79, 83, 89, 97, 101, 103, 107, 109,
    113, 127, 131,
];

fn configuration(target: &str) -> Value {
    json!({
        "seed": SEED,
        "target": target,
        "aggregation_bucket_width_secs": 10,
        "volume": 10000,
        "corpus": {"size": 10, "payload": {"static": format!("{target}\n$GROUP\nkeep: 'quotes'\n")}},
    })
}

fn case_values(document: &Value) -> CaseValues {
    CaseValues {
        files: BTreeMap::from([(
            CONFIG_FILE.into(),
            FileSource {
                from: CONFIG_FILE.into(),
                template: true,
            },
        )]),
        file_contents: BTreeMap::from([(
            CONFIG_FILE.into(),
            serde_saphyr::to_string(document).unwrap().into_bytes(),
        )]),
        ..Default::default()
    }
}

#[test]
fn paired_configs_change_only_the_target_and_keep_the_legacy_seed_and_payload() {
    let target = "unix:///airlock/subject/shared/metrics.sock";
    let baseline = "unix:///airlock/baseline/shared/metrics.sock";
    let document = configuration(target);
    let env = BTreeMap::from([
        (TARGET_ENV.into(), target.into()),
        (BASELINE_TARGET_ENV.into(), baseline.into()),
        ("RUST_LOG".into(), "info".into()),
    ]);
    let config = rendered(env.clone(), case_values(&document).file_contents);
    for realization in [LINUX, POD] {
        let recipe = Millstone.recipe(realization, &config).unwrap();
        if realization == LINUX {
            assert_container_command(
                &recipe,
                TOOLS_LINUX_IMAGE,
                &["/bin/sh", WRAPPER_PATH],
                &["/usr/local/bin/millstone", CONFIG_DIR, STATE_DIR, "keep-alive"],
                &env,
            );
        } else {
            assert_pod(
                &recipe,
                "millstone",
                TOOLS_LINUX_IMAGE,
                &["/bin/sh", WRAPPER_PATH],
                &["/usr/local/bin/millstone", CONFIG_DIR, STATE_DIR],
                &env,
            );
            assert_eq!(
                late_files(&recipe),
                ["/etc/millstone/baseline.yaml", "/etc/millstone/subject.yaml"]
            );
        }
        let placed = files(&recipe);
        assert_eq!(placed.len(), 3);
        assert_eq!(placed[0], (WRAPPER_PATH, 0o755, WRAPPER));
        for (index, process, expected_target) in [(1, "subject", target), (2, "baseline", baseline)] {
            let (path, mode, contents) = placed[index];
            assert_eq!(path, format!("{CONFIG_DIR}/{process}.yaml"));
            assert_eq!(mode, 0o644);
            let mut actual: Value = serde_saphyr::from_str(simdutf8::basic::from_utf8(contents).unwrap()).unwrap();
            assert_eq!(actual["target"], expected_target);
            assert_eq!(actual["seed"], json!(SEED));
            actual["target"] = json!(target);
            assert_eq!(actual, document);
        }
    }
}

const LATE_CONTAINER_ID: &str = "{{ self.late.container_id }}";
const LATE_POD_UID: &str = "{{ self.late.pod_uid }}";

// YAML serialization must preserve quotes so numeric late IDs stay strings.
#[test]
fn pod_recipe_keeps_late_references_as_strings_in_the_real_origin_detection_matrix() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../test/correctness/shared/dsd-origin-detection-matrix/millstone.yaml");
    let fixture = std::fs::read_to_string(path).unwrap();
    let target_reference = "{{ self.connections.target.url }}";
    let subject = "unixgram:///airlock/subject/shared/metrics.sock";
    let baseline = "unixgram:///airlock/baseline/shared/metrics.sock";
    for placeholder in [target_reference, LATE_CONTAINER_ID, LATE_POD_UID] {
        assert!(
            fixture.contains(placeholder),
            "{placeholder} missing from the matrix millstone.yaml"
        );
    }
    let templated = fixture.replace(target_reference, subject);
    let env = BTreeMap::from([
        (TARGET_ENV.into(), subject.into()),
        (BASELINE_TARGET_ENV.into(), baseline.into()),
    ]);
    let recipe = Millstone
        .recipe(
            POD,
            &rendered(env, BTreeMap::from([(CONFIG_FILE.into(), templated.into_bytes())])),
        )
        .unwrap();
    let placed = files(&recipe);
    assert_eq!(placed.len(), 3);

    // Includes IDs that a plain scalar would turn into an integer, a float and a different string.
    for (container_id, pod_uid) in [
        (
            "0123456789012345678901234567890123456789012345678901234567890123",
            "1234567890",
        ),
        (
            "1234567890123456789012345678901234567890123456789012345678901234",
            "12e4",
        ),
        (
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "0a1b2c3d-4e5f-6789-abcd-ef0123456789",
        ),
    ] {
        let resolved = fixture
            .replace(target_reference, subject)
            .replace(LATE_CONTAINER_ID, container_id)
            .replace(LATE_POD_UID, pod_uid);
        let mut expected: Value = serde_saphyr::from_str(&resolved).unwrap();
        for (index, target) in [(1, subject), (2, baseline)] {
            let (_, _, contents) = placed[index];
            let contents = simdutf8::basic::from_utf8(contents)
                .unwrap()
                .replace(LATE_CONTAINER_ID, container_id)
                .replace(LATE_POD_UID, pod_uid);
            let actual: Value = serde_saphyr::from_str(&contents).unwrap();
            let payload = &actual["corpus"]["payload"]["dogstatsd"];
            assert_eq!(
                payload["container_ids"][0],
                Value::String(container_id.into()),
                "{contents}"
            );
            assert_eq!(
                payload["external_data"][0],
                Value::String(format!("pu-{pod_uid},cn-millstone,it-false")),
                "{contents}"
            );
            assert_eq!(actual["target"], target);
            expected["target"] = json!(target);
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn both_connections_are_required_and_deliver_disjoint_target_urls() {
    let consumes = Millstone.consumes(LINUX, &EffectiveConfig::default());
    assert_eq!(
        consumes.keys().map(String::as_str).collect::<Vec<_>>(),
        ["baseline_target", "target"]
    );
    for (name, key) in [("baseline_target", BASELINE_TARGET_ENV), ("target", TARGET_ENV)] {
        let endpoint = &consumes[name];
        assert!(endpoint.required);
        assert_eq!(
            endpoint.accepts,
            [
                EndpointKind::Tcp,
                EndpointKind::Udp,
                EndpointKind::Http,
                EndpointKind::Grpc,
                EndpointKind::UnixDatagram,
                EndpointKind::UnixStream
            ],
        );
        let Delivery::Env { keys, value } = &endpoint.delivery;
        assert_eq!(keys, &[key.into()].into());
        for address in [
            http_address("receiver", 8126),
            unix_stream_address("/airlock/receiver/shared/input.sock"),
        ] {
            assert_eq!(value(&address), BTreeMap::from([(key.into(), address.url().into())]));
        }
    }
}

#[test]
fn configuration_requires_a_templated_target_and_a_valid_seed() {
    for target in [
        "{{ self.connections.target.url }}",
        "{{self.connections.target.url}}",
        "{{  self.connections.target.url\t}}",
        "{{ self.connections.target.url }}/",
        "{{self.connections.target.url}}/pkg.Service/Export",
        "{{ self.connections.target.url }}/v0.4/traces?a=b#c",
    ] {
        Millstone
            .validate_config(LINUX, &case_values(&configuration(target)))
            .unwrap();
    }
    let valid = configuration("{{ self.connections.target.url }}");
    for (document, message) in [
        (json!([]), "must contain a mapping"),
        (json!({"seed": SEED}), "key 'target'"),
        (json!({"seed": SEED, "target": 42}), "key 'target'"),
        (json!({"seed": SEED, "target": ""}), "key 'target'"),
        (configuration("udp://fixed:8125"), "self.connections.target.url"),
        (
            configuration("{{ self.connections.baseline_target.url }}"),
            "self.connections.target.url",
        ),
        (configuration("{{ self.late.ip }}"), "self.connections.target.url"),
        (
            configuration("{{ self.connections.target.url }}x"),
            "self.connections.target.url",
        ),
        (
            configuration("{{ self.connections.target.url }}:81"),
            "self.connections.target.url",
        ),
        (
            configuration("{{ self.connections.target.url }}@host/"),
            "self.connections.target.url",
        ),
        (
            configuration("{{ self.connections.target.url }}?a=b"),
            "self.connections.target.url",
        ),
        (
            configuration("{{ self.connections.target.url }}/{{ self.late.ip }}"),
            "self.connections.target.url",
        ),
        (
            configuration("{{ self.connections.target.url }}/a{{"),
            "self.connections.target.url",
        ),
        // Reject unmatched closing delimiters before template rendering.
        (
            configuration("{{ self.connections.target.url }}/a}}"),
            "self.connections.target.url",
        ),
        (
            configuration(" {{ self.connections.target.url }}/a"),
            "self.connections.target.url",
        ),
        (
            configuration("/a{{ self.connections.target.url }}"),
            "self.connections.target.url",
        ),
        (
            configuration("{{ self.connections.target.url /a"),
            "self.connections.target.url",
        ),
    ] {
        let error = Millstone.validate_config(LINUX, &case_values(&document)).unwrap_err();
        assert!(error.contains(message), "{error}");
    }
    for seed in [
        Value::Null,
        json!([]),
        json!(vec![0; 31]),
        json!(vec![0; 33]),
        json!(vec![256; 32]),
        json!(vec![-1; 32]),
        json!("seed"),
    ] {
        let mut document = valid.clone();
        document["seed"] = seed;
        let error = Millstone.validate_config(LINUX, &case_values(&document)).unwrap_err();
        assert_eq!(error, "millstone.yaml key 'seed' must be an array of 32 bytes (0–255)");
    }
    let mut config = case_values(&valid);
    config.files.get_mut(CONFIG_FILE).unwrap().template = false;
    assert_eq!(
        Millstone.validate_config(LINUX, &config).unwrap_err(),
        "millstone requires config.files.millstone.yaml with template: true"
    );
    assert_eq!(
        Millstone.validate_config(LINUX, &CaseValues::default()).unwrap_err(),
        "millstone requires config.files.millstone.yaml with template: true"
    );
    for (contents, message) in [
        (vec![255], "must be UTF-8"),
        (b"target: [".to_vec(), "cannot parse millstone.yaml"),
    ] {
        let mut config = case_values(&valid);
        config.file_contents.insert(CONFIG_FILE.into(), contents);
        let error = Millstone.validate_config(LINUX, &config).unwrap_err();
        assert!(error.contains(message), "{error}");
    }
}

#[test]
fn a_recipe_applies_the_rendered_path_to_both_senders() {
    let path = "/opentelemetry.proto.collector.metrics.v1.MetricsService/Export";
    let env = BTreeMap::from([
        (TARGET_ENV.into(), "http://subject:4317".into()),
        (BASELINE_TARGET_ENV.into(), "http://baseline:54317".into()),
    ]);
    let document = configuration(&format!("http://subject:4317{path}"));
    let config = rendered(env, case_values(&document).file_contents);
    for realization in [LINUX, POD] {
        let recipe = Millstone.recipe(realization, &config).unwrap();
        let placed = files(&recipe);
        for (index, expected_target) in [
            (1, format!("http://subject:4317{path}")),
            (2, format!("http://baseline:54317{path}")),
        ] {
            let actual: Value = serde_saphyr::from_str(simdutf8::basic::from_utf8(placed[index].2).unwrap()).unwrap();
            assert_eq!(actual["target"], expected_target);
            assert_eq!(actual["seed"], json!(SEED));
        }
    }
}

#[test]
fn a_recipe_rejects_missing_deliveries_and_a_target_that_bypasses_its_connection() {
    let document = configuration("udp://subject:8125");
    let env = BTreeMap::from([
        (TARGET_ENV.into(), "udp://subject:8125".into()),
        (BASELINE_TARGET_ENV.into(), "udp://baseline:8125".into()),
    ]);
    for key in [TARGET_ENV, BASELINE_TARGET_ENV] {
        let mut env = env.clone();
        env.remove(key);
        let error = Millstone
            .recipe(LINUX, &rendered(env, case_values(&document).file_contents))
            .err()
            .unwrap();
        assert!(error.contains(key), "{error}");
    }
    for target in [
        "udp://unconnected:8125",
        "udp://subject:8125x",
        "udp://subject:81250/path",
        "udp://subject:8125@unconnected:8125/path",
        "udp://subject:8125.unconnected/path",
        "udp://subject:8125?query",
        "udp://subject:8125/{{ self.late.ip }}",
        "udp://subject:8125/a}}",
        "udp://subject:812/path",
    ] {
        let error = Millstone
            .recipe(
                LINUX,
                &rendered(env.clone(), case_values(&configuration(target)).file_contents),
            )
            .err()
            .unwrap();
        assert_eq!(
            error, "millstone.yaml key 'target' must be the delivered target URL plus an optional path",
            "{target}"
        );
    }
}

#[test]
fn process_channels_and_capture_destinations_keep_both_senders_distinct() {
    assert_eq!(Millstone.name(), "millstone");
    assert_eq!(Millstone.processes(), &["baseline", "subject"]);
    for (spec, process) in Millstone.process_specs(LINUX).iter().zip(Millstone.processes()) {
        assert_eq!(spec.name, *process);
        assert_eq!(spec.exit, ExitChannel::Record { file: process });
        assert_eq!(
            spec.output,
            OutputChannel::File {
                file: if *process == "baseline" {
                    "baseline.log"
                } else {
                    "subject.log"
                },
                stream: OutputStream::Merged,
            }
        );
    }
    assert_eq!(Millstone.process_specs(LINUX).len(), 2);
    assert_eq!(Millstone.process_specs(POD), Millstone.process_specs(LINUX));
    assert!(Millstone.collect(POD).is_empty());
    let collected = Millstone.collect(LINUX);
    assert_eq!(collected.len(), 3);
    for (index, filename) in [(0, "input.jsonl.zst"), (1, "input-manifest.json")] {
        let file = &collected[index];
        assert_eq!(file.path, format!("{STATE_DIR}/baseline/{filename}"));
        assert_eq!(file.name, filename);
        assert_eq!(file.destination, CollectDestination::Traffic);
    }
    assert_eq!(collected[2].path, format!("{STATE_DIR}/subject/input-manifest.json"));
    assert_eq!(collected[2].name, "input-manifest.json");
    assert_eq!(collected[2].kind, "subject_input_manifest");
    assert_eq!(collected[2].destination, CollectDestination::Participant);
}

#[test]
fn linux_containers_and_pods_are_supported_without_extra_parameters_or_operations() {
    assert_eq!(
        Millstone.supports(),
        &[
            (Os::Linux, RealizationKind::LinuxContainer),
            (Os::Linux, RealizationKind::KubernetesPod),
        ]
    );
    assert_eq!(Millstone.config_files(), &["millstone.yaml"]);
    assert!(Millstone.params().is_empty());
    for realization in [LINUX, POD] {
        assert_eq!(Millstone.artifacts(realization), &[TOOLS_LINUX_IMAGE]);
        assert!(Millstone.operations(realization).is_empty());
        assert!(Millstone.facts(realization).is_empty());
        assert!(Millstone
            .endpoints(realization, &EffectiveConfig::default())
            .unwrap()
            .is_empty());
    }
    let config = rendered(BTreeMap::new(), BTreeMap::new());
    for os in [Os::Linux, Os::MacOs, Os::Windows] {
        for kind in [
            RealizationKind::LinuxContainer,
            RealizationKind::WindowsContainer,
            RealizationKind::HostProcessGroup,
            RealizationKind::KubernetesPod,
        ] {
            let realization = Realization { os, kind };
            if [LINUX, POD].contains(&realization) {
                continue;
            }
            assert!(Millstone.artifacts(realization).is_empty());
            assert!(Millstone.collect(realization).is_empty());
            assert!(Millstone.process_specs(realization).is_empty());
            assert!(Millstone.consumes(realization, &EffectiveConfig::default()).is_empty());
            let error = Millstone.recipe(realization, &config).err().unwrap();
            assert_eq!(
                error,
                format!(
                    "participant type 'millstone' has no recipe for {} on {}",
                    kind.name(),
                    os.name()
                )
            );
        }
    }
}

#[cfg(unix)]
mod shell {
    use std::{
        fs,
        os::unix::fs::PermissionsExt as _,
        path::PathBuf,
        process::{Child, Command, ExitStatus},
        thread,
        time::{Duration, Instant},
    };

    use tempfile::TempDir;

    use super::*;

    // Allow for slow process startup on shared hosts.
    const WAIT_BOUND: Duration = Duration::from_secs(60);

    #[test]
    fn the_wrapper_rejects_unknown_modes_before_starting_senders() {
        let output = Command::new("/bin/sh")
            .args([
                "-c",
                simdutf8::basic::from_utf8(WRAPPER).unwrap(),
                "run.sh",
                "unused",
                "unused",
                "unused",
                "keep-alvie",
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(String::from_utf8(output.stderr).unwrap(), "unknown mode 'keep-alvie'\n");
    }

    struct Wrapper {
        directory: PathBuf,
        child: Child,
        /// Removes the scratch directory after `Drop` has stopped the wrapper.
        _scratch: TempDir,
    }

    impl Wrapper {
        fn start(failing: &str, keep_alive: bool) -> Self {
            let scratch = tempfile::tempdir().unwrap();
            let directory = scratch.path().canonicalize().unwrap();
            fs::write(directory.join("run.sh"), WRAPPER).unwrap();
            let fake = directory.join("fake millstone");
            fs::write(
                &fake,
                r#"#!/bin/sh
process=$(basename "$1" .yaml)
printf '%s\n' "$@" > "$FAKE_DIR/$process.args"
printf '%s stdout\n' "$process"
printf '%s stderr\n' "$process" >&2
if [ "$FAILING_PROCESS" = "$process" ]; then exit 7; fi
if [ "$2" = '--traffic-capture-dir' ]; then
    mkdir -p "$3"
    printf 'capture' > "$3/input.jsonl.zst"
    printf '{"complete":true}' > "$3/input-manifest.json"
    exit 0
fi
[ "$2" = '--completion-file' ] || exit 8
[ "$4" = '--traffic-capture-dir' ] || exit 9
mkdir -p "$5"
printf 'capture' > "$5/input.jsonl.zst"
printf '{"complete":true}' > "$5/input-manifest.json"
: > "$3"
printf 'Millstone finished sending. Waiting for shutdown.\n'
while [ ! -f "$FAKE_DIR/stop" ]; do sleep 0.05; done
"#,
            )
            .unwrap();
            fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
            let mut command = Command::new("/bin/sh");
            command
                .arg(directory.join("run.sh"))
                .arg(fake)
                .arg(directory.join("config"))
                .arg(directory.join("state"));
            if keep_alive {
                command.arg("keep-alive");
            }
            let child = command
                .env("PANORAMIC_EXIT_DIR", directory.join("exit"))
                .env("PANORAMIC_LOG_DIR", directory.join("log"))
                .env("FAKE_DIR", &directory)
                .env("FAILING_PROCESS", failing)
                .spawn()
                .unwrap();
            Self {
                directory,
                child,
                _scratch: scratch,
            }
        }

        fn wait_for(&mut self, relative: &str) -> String {
            let deadline = Instant::now() + WAIT_BOUND;
            loop {
                if let Ok(contents) = fs::read_to_string(self.directory.join(relative)) {
                    if !contents.is_empty() {
                        return contents;
                    }
                }
                assert!(Instant::now() < deadline, "wrapper did not write {relative}");
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn wait(&mut self) -> ExitStatus {
            let deadline = Instant::now() + WAIT_BOUND;
            loop {
                if let Some(status) = self.child.try_wait().unwrap() {
                    return status;
                }
                assert!(Instant::now() < deadline, "wrapper did not exit");
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn terminate(&self) {
            Command::new("/bin/kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status()
                .unwrap();
        }

        fn assert_record(&mut self, process: &str, code: u32, signal: u32) {
            let record = self.wait_for(&format!("exit/{process}"));
            let pattern = format!(r"\A{code} {signal} [0-9]+\.[0-9]{{1,9}}\n\z");
            assert!(regex::Regex::new(&pattern).unwrap().is_match(&record), "{record:?}");
        }
    }

    impl Drop for Wrapper {
        fn drop(&mut self) {
            let _ = fs::write(self.directory.join("stop"), "");
            if self.child.try_wait().ok().flatten().is_none() {
                self.terminate();
                let deadline = Instant::now() + WAIT_BOUND;
                while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    #[test]
    fn wrapper_keeps_both_senders_alive_and_records_shutdown_independently() {
        let mut wrapper = Wrapper::start("", true);
        for process in ["baseline", "subject"] {
            wrapper.wait_for(&format!("state/{process}/input-manifest.json"));
            let log = wrapper.wait_for(&format!("log/{process}.log"));
            // The manifest precedes completion output, so wait for that line separately.
            let deadline = Instant::now() + WAIT_BOUND;
            while !fs::read_to_string(wrapper.directory.join(format!("log/{process}.log")))
                .unwrap()
                .contains("Millstone finished sending")
            {
                assert!(Instant::now() < deadline, "completion line missing: {log}");
                thread::sleep(Duration::from_millis(10));
            }
            let log = wrapper.wait_for(&format!("log/{process}.log"));
            assert!(log.contains(&format!("{process} stdout")));
            assert!(log.contains(&format!("{process} stderr")));
            assert!(!wrapper.directory.join(format!("exit/{process}")).exists());
            let args = wrapper.wait_for(&format!("{process}.args"));
            assert_eq!(
                args.lines().map(str::to_owned).collect::<Vec<_>>(),
                [
                    wrapper
                        .directory
                        .join(format!("config/{process}.yaml"))
                        .display()
                        .to_string(),
                    "--completion-file".into(),
                    wrapper
                        .directory
                        .join(format!("state/{process}.complete"))
                        .display()
                        .to_string(),
                    "--traffic-capture-dir".into(),
                    wrapper.directory.join(format!("state/{process}")).display().to_string(),
                ]
            );
        }
        assert!(wrapper.child.try_wait().unwrap().is_none());
        wrapper.terminate();
        assert!(wrapper.wait().success());
        for process in ["baseline", "subject"] {
            wrapper.assert_record(process, 256, 15);
        }
    }

    #[test]
    fn wrapper_without_keep_alive_lets_both_senders_exit_after_sending() {
        let mut wrapper = Wrapper::start("", false);
        assert!(wrapper.wait().success());
        for process in ["baseline", "subject"] {
            wrapper.assert_record(process, 0, 0);
            assert!(wrapper
                .directory
                .join(format!("state/{process}/input-manifest.json"))
                .exists());
            assert!(!wrapper.directory.join(format!("state/{process}.complete")).exists());
            let log = wrapper.wait_for(&format!("log/{process}.log"));
            assert!(log.contains(&format!("{process} stdout")));
            assert!(log.contains(&format!("{process} stderr")));
            let args = wrapper.wait_for(&format!("{process}.args"));
            assert_eq!(
                args.lines().map(str::to_owned).collect::<Vec<_>>(),
                [
                    wrapper
                        .directory
                        .join(format!("config/{process}.yaml"))
                        .display()
                        .to_string(),
                    "--traffic-capture-dir".into(),
                    wrapper.directory.join(format!("state/{process}")).display().to_string(),
                ]
            );
        }
    }

    #[test]
    fn either_senders_failure_is_recorded_without_waiting_for_the_other() {
        for (failed, survivor) in [("baseline", "subject"), ("subject", "baseline")] {
            let mut wrapper = Wrapper::start(failed, true);
            wrapper.assert_record(failed, 7, 0);
            wrapper.wait_for(&format!("state/{survivor}/input-manifest.json"));
            assert!(!wrapper.directory.join(format!("exit/{survivor}")).exists());
            assert!(wrapper.child.try_wait().unwrap().is_none());
            fs::write(wrapper.directory.join("stop"), "").unwrap();
            assert_eq!(wrapper.wait().code(), Some(7));
            wrapper.assert_record(survivor, 0, 0);
            for process in [failed, survivor] {
                let log = wrapper.wait_for(&format!("log/{process}.log"));
                assert!(log.contains(&format!("{process} stdout")));
                assert!(log.contains(&format!("{process} stderr")));
            }
        }
    }
}
