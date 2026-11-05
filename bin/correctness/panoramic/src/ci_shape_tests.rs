//! Checks that CI builds, binds and runs the intended test artifacts.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::{json, Value};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .unwrap()
}

fn read_text(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
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
    let file = ".gitlab/e2e.yml";
    let text = read_text(&root.join(file));
    let render = "- target/release/panoramic artifacts render";
    let pull = "- target/release/panoramic artifacts pull -d test/integration/cases --subject-os linux --artifacts target/panoramic-artifacts.toml";
    let pins = ["test-integration:", render, pull];
    assert_ci_block_lines_in_order(file, &text, &pins);
    let reordered = text
        .replacen(render, "__render_command__", 1)
        .replacen(pull, render, 1)
        .replacen("__render_command__", pull, 1);
    let error = std::panic::catch_unwind(|| assert_ci_block_lines_in_order(file, &reordered, &pins))
        .expect_err("pulling before rendering must not satisfy the ordered pins");
    assert_eq!(
        error.downcast_ref::<String>().unwrap(),
        &format!("{file}: CI line changed or out of order: {pull}")
    );
}

#[test]
fn ci_panoramic_environment_variables_have_consumers() {
    use clap::CommandFactory as _;

    let root = repository_root();
    let command = crate::cli::Cli::command();
    let run = command.find_subcommand("run").unwrap();
    let variable = regex::Regex::new(r"\bPANORAMIC_[A-Za-z0-9_]*\b").unwrap();
    // A Make command-line argument is not an environment input; exporting the same name must still fail.
    let make_argument = regex::Regex::new(r"^\s*(?:-\s+)?make\s+[\w-]+\s+(PANORAMIC_ARTIFACTS)=[^\s;&|]+\s*$").unwrap();
    let test_variables = [
        "PANORAMIC_TEST_IMAGE",
        "PANORAMIC_TEST_WINDOWS_IMAGE",
        "PANORAMIC_TEST_WINDOWS_COURIER",
    ];
    for (directory, extension) in [(".gitlab", "yml"), ("ci/tooling", "ps1")] {
        for entry in fs::read_dir(root.join(directory)).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|ext| ext != extension) {
                continue;
            }
            let text = read_text(&path);
            for (index, line) in text.lines().enumerate() {
                if line.trim_start().starts_with('#') {
                    continue;
                }
                let make_argument = make_argument.captures(line).and_then(|captures| captures.get(1));
                for name in variable.find_iter(line) {
                    if make_argument.is_some_and(|argument| argument.range() == name.range()) {
                        continue;
                    }
                    let name = name.as_str();
                    assert!(
                        test_variables.contains(&name)
                            || run
                                .get_arguments()
                                .any(|arg| arg.get_env() == Some(std::ffi::OsStr::new(name))),
                        "{}:{}: CI variable {name} must be read by the CLI or an explicitly listed test target",
                        path.display(),
                        index + 1
                    );
                }
            }
        }
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
fn ci_jobs_bind_and_run_their_artifact_manifests() {
    let root = repository_root();
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
    for job in [
        "test-integration",
        "test-integration-macos-arm64",
        "test-integration-macos-amd64",
    ] {
        assert!(has_job(&e2e, job), "Missing CI job: {job}");
    }
    assert!(has_job(&windows, "test-integration-windows-amd64"));
    let parsed: Value = serde_saphyr::from_str(&mixins).unwrap();
    assert_eq!(parsed[".test-correctness-definition"]["timeout"], "10m");
    assert_eq!(parsed[".test-correctness-kind-definition"]["timeout"], "20m");

    for (file, text, pins) in [
        (".gitlab/e2e.yml", e2e.as_str(), &[
            "build-panoramic-binary:",
            "- make build-panoramic",
            "- make build-courier-linux",
        ][..]),
        ("Makefile", makefile.as_str(), &[
            "build-courier-linux: check-rust-build-tools",
            "@rustup target add $(COURIER_LINUX_TARGET)",
            "@cargo build --profile release --package panoramic-courier --bin panoramic-courier --target $(COURIER_LINUX_TARGET)",
        ][..]),
        (".gitlab/e2e.yml", e2e.as_str(), &[
            r#"test-integration:"#,
            r#"- target/release/panoramic artifacts render"#,
            r#"--set agent-adp-linux-image=${SALUKI_IMAGE_REPO_BASE}/bundled-agent-adp:${CI_COMMIT_SHA}-${CI_PIPELINE_ID}"#,
            r#"--set tools-linux-image=${SALUKI_IMAGE_REPO_BASE}/correctness-tools:${CI_COMMIT_SHA}"#,
            r#"--set fixup-image=registry.ddbuild.io/alpine:latest"#,
            r#"--set courier-linux-binary=target/x86_64-unknown-linux-musl/release/panoramic-courier"#,
            r#"--output target/panoramic-artifacts.toml"#,
            r#"- target/release/panoramic artifacts pull -d test/integration/cases --subject-os linux --artifacts target/panoramic-artifacts.toml"#,
            r#"- make run-integration-quick PANORAMIC_ARTIFACTS=target/panoramic-artifacts.toml"#,
        ][..]),
        (".gitlab/correctness-mixins.yml", mixins.as_str(), &[
            r#".test-correctness-definition:"#,
            r#"- target/release/panoramic artifacts render"#,
            r#"--set agent-adp-linux-image=${SALUKI_IMAGE_REPO_BASE}/bundled-agent-adp:${CI_COMMIT_SHA}-${PARENT_PIPELINE_ID}"#,
            r#"--set tools-linux-image=${SALUKI_IMAGE_REPO_BASE}/correctness-tools:${CI_COMMIT_SHA}"#,
            r#"--set fixup-image=registry.ddbuild.io/alpine:latest"#,
            r#"--set courier-linux-binary=target/x86_64-unknown-linux-musl/release/panoramic-courier"#,
            r#"--output target/panoramic-artifacts.toml"#,
            r#"- target/release/panoramic artifacts pull -d $(pwd)/test/correctness/cases --subject-os linux --artifacts target/panoramic-artifacts.toml"#,
            r#"- target/release/panoramic run -d $(pwd)/test/correctness/cases -t ${CORRECTNESS_TEST_CASE} --subject-os linux --artifacts target/panoramic-artifacts.toml --no-tui"#,
        ][..]),
        ("Makefile", makefile.as_str(), &[
            r#"render-correctness-test-artifacts: build-panoramic"#,
            r#"@target/release/panoramic artifacts render"#,
            r#"--set agent-adp-linux-image=saluki-images/datadog-agent:testing-release"#,
            r#"--output "$(CORRECTNESS_TEST_ARTIFACTS)""#,
        ][..]),
        ("Makefile", makefile.as_str(), &[
            r#"test-correctness: render-correctness-test-artifacts build-courier-linux build-correctness-tools-image build-datadog-agent-image-release"#,
            r#"@target/release/panoramic run --subject-os linux -d $(shell pwd)/test/correctness/cases $(if $(PANORAMIC_PARALLELISM),-p $(PANORAMIC_PARALLELISM)) $(or $(PANORAMIC_ARTIFACTS_ARG),--artifacts "$(CORRECTNESS_TEST_ARTIFACTS)")"#,
        ][..]),
        ("Makefile", makefile.as_str(), &[
            r#"test-correctness-case: render-correctness-test-artifacts build-courier-linux build-correctness-tools-image build-datadog-agent-image-release"#,
            r#"@target/release/panoramic run --subject-os linux -d $(shell pwd)/test/correctness/cases -t $(CASE) --no-tui $(or $(PANORAMIC_ARTIFACTS_ARG),--artifacts "$(CORRECTNESS_TEST_ARTIFACTS)")"#,
        ][..]),
        ("Makefile", makefile.as_str(), &[
            r#"test-integration: build-panoramic build-courier-linux build-datadog-agent-image build-correctness-tools-image"#,
            r#"@target/release/panoramic run --subject-os linux -d $(shell pwd)/test/integration/cases $(if $(PANORAMIC_LOG_DIR),-l $(PANORAMIC_LOG_DIR)) $(PANORAMIC_ARTIFACTS_ARG)"#,
        ][..]),
        ("Makefile", makefile.as_str(), &[
            r#"test-integration-quick: build-panoramic build-courier-linux"#,
            r#"@$(MAKE) run-integration-quick"#,
        ][..]),
        ("Makefile", makefile.as_str(), &[
            r#"run-integration-quick: ## Runs ADP integration tests using built binaries and images"#,
            r#"@target/release/panoramic run --subject-os linux -d $(shell pwd)/test/integration/cases $(if $(PANORAMIC_LOG_DIR),-l $(PANORAMIC_LOG_DIR)) $(PANORAMIC_ARTIFACTS_ARG)"#,
        ][..]),
        ("Makefile", makefile.as_str(), &[
            r#"list-integration-tests: build-panoramic"#,
            r#"@target/release/panoramic list --subject-os linux -d $(shell pwd)/test/integration/cases"#,
        ][..]),
        ("Makefile", makefile.as_str(), &[
            r#"test-integration-macos-run: ## Runs macOS cases with already-built binaries and the artifact manifest; narrow with CASE=<name>."#,
            r#"@target/release/panoramic run -d "$(CURDIR)/test/integration/cases""#,
            r#"--subject-os macos"#,
            r#"$(PANORAMIC_ARTIFACTS_ARG)"#,
        ][..]),
        ("Makefile", makefile.as_str(), &[
            "build-intake-host: check-rust-build-tools",
            "@cargo build --profile release --package datadog-intake --bin datadog-intake",
        ][..]),
        ("Makefile", makefile.as_str(), &[
            r#"render-macos-test-artifacts: build-panoramic"#,
            r#"@target/release/panoramic artifacts render"#,
            r#"--set adp-host-binary="$(CURDIR)/target/$(BUILD_PROFILE)/agent-data-plane""#,
            r#"--set agent-host-install="$(MACOS_TEST_AGENT_INSTALL_DIR)""#,
            r#"--set intake-host-binary="$(CURDIR)/target/release/datadog-intake""#,
            r#"--output "$(MACOS_TEST_ARTIFACTS)""#,
        ][..]),
        ("Makefile", makefile.as_str(), &[
            r#"test-integration-macos-ci: override PANORAMIC_ARTIFACTS = $(MACOS_TEST_ARTIFACTS)"#,
            r#"test-integration-macos-ci: build-panoramic build-adp-host build-intake-host provision-macos-test-env render-macos-test-artifacts test-integration-macos-run ## CI entry point: builds binaries, provisions the Agent and cert, renders artifacts, and runs macOS integration tests"#,
        ][..]),
    ] {
        assert_ci_block_lines_in_order(file, text, pins);
    }
    assert_lines_in_order(
        "Makefile",
        &makefile,
        &[
            r#"PANORAMIC_ARTIFACTS_ARG = $(if $(filter environment%,$(origin PANORAMIC_ARTIFACTS)),$(error PANORAMIC_ARTIFACTS must be set on the make command line, not in the environment),$(if $(PANORAMIC_ARTIFACTS),--artifacts "$(PANORAMIC_ARTIFACTS)"))"#,
        ],
    );
    assert_lines_in_order(
        "Makefile",
        &makefile,
        &[r#"CORRECTNESS_TEST_ARTIFACTS ?= $(CURDIR)/target/panoramic-artifacts-correctness.toml"#],
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
        r#"| jq -r 'to_entries[] | select(if .value.status == "skipped" then false elif .value.status == "runnable" then true else error("unknown status for \(.key)") end) | "\(.key)\t\(any(.value.layout[]; .realization == "kubernetes-pod"))"'"#,
        r#"| while IFS=$'\t' read -r case pod; do"#,
        r#"if [[ "${pod}" == "true" ]]; then"#,
        "base_mixin=\".test-correctness-kind-definition\"",
        "base_mixin=\".test-correctness-definition\"",
        "extends: [${base_mixin}]",
        "test-correctness-${case}:",
        "CORRECTNESS_TEST_CASE: ${case}",
    ] {
        assert!(position(rule).is_some(), "CI generator rule changed: {rule}");
    }
    // Without pipefail the pipeline's status is the while loop's, so a failing list would silently drop jobs.
    assert!(
        position("set -euo pipefail") < position(list_rule),
        "CI generator must enable fail-fast shell options before listing cases"
    );
}
