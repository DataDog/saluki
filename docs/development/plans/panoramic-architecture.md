# Panoramic architecture coverage

## Purpose

This inventory records Panoramic's coverage per (case, subject OS): runtime, CI jobs, assertions and loaded
configuration. Use it to check coverage when changing case schemas or runners.

## Columns

- **Case:** the discovered name, including the variant suffix for a matrix.
- **Suite:** integration or correctness.
- **Subject OS:** Linux, macOS or Windows; independent of the host running the inventory test.
  A participant case must declare `subject_os`.
- **Runtime:** the typed correctness runner's identifier (`docker` or `kubernetes_in_docker`),
  or the native subject participant's realization kind.
- **Status:** selected by CI discovery, or disabled. Participant rows take their status from their directory:
  `test/correctness/disabled/` is compiled but not selected by CI. Typed cases also require `disabled: true`
  there. The inventory requires each case's directory to match its suite and status.
- **CI job:** the job that selects this row. Correctness jobs belong to the `run-correctness-tests` child pipeline.
  Disabled rows have no job. macOS rows list both architectures.
- **Schema:** the file's `type` value, or `participants` for a file that declares participants.
- **Execution:** `old` identifies typed runners; `native` identifies the participant lifecycle.
- **Steps:** assertions and actions, including assertions inside parallel blocks, or the correctness comparison mode.
- **Ignored:** number of key paths the loader drops without error. The probe loads each modified file through the
  case loader and planner. Replacing each value with either a map or a string must
  leave the recorded configuration unchanged. Paths are listed in the JSON.

The [JSON inventory](./panoramic-architecture-inventory.json) also records source paths, timeouts, images before CLI
image overrides, correctness CI mixins, and a `cases` map keyed by `<suite>/<name>`. Each case contains the effective
configuration, integration step counts by kind or correctness comparison options, and ignored key paths. Integration
OS rows share one case entry; participant configurations are keyed by subject OS, and matrix variants have separate
entries. Native rows record artifact references by role name using local defaults, with the courier path normalized
to `x86_64` for a host-independent snapshot. Totals count files once and steps per selected OS row; disabled rows are
separate from selected totals.

## Regenerating

```bash
PANORAMIC_WRITE_COVERAGE_INVENTORY=1 cargo nextest run -p panoramic coverage_inventory
```

Review the diff. Update the inventory whenever a case file, its platforms or its CI job changes.
Without the environment variable, the test compares the generated JSON and coverage tables with the checked-in
versions and reports added or removed row keys and the first differing line in each file. The other inventory tests
check file accounting, case placement by suite and status, case-name uniqueness across both suites, CI job and mixin
definitions, the correctness generator's fail-fast shell options, ignored-key detection, logging-case baseline
equivalence, and migration proofs: their rows, CI links and comparison commit.

## Coverage details

- There are 52 integration files and 48 correctness files, two of them under `test/correctness/disabled/`.
  Integration selects 45 Linux, 33 macOS and 36 Windows rows. Correctness selects 46 Linux rows: six typed Docker,
  33 native Linux-container and seven native Kubernetes-pod cases. Two correctness cases are disabled (`otlp-traces`
  and `otlp-traces-ets`) and live in `test/correctness/disabled/`. The inventory has 162 rows.
- Correctness cases declare only Linux; `run` and `list` report them as skipped for other subject OSes.
  CI runs all selected correctness cases on Linux.
  The seven origin detection variants have separate `dsd-origin-detection-matrix-<variant>` directories: `legacy`,
  `legacy-high-cardinality`, `legacy-optout-disabled`, `legacy-with-client-fields`, `unified`,
  `unified-high-cardinality` and `unified-with-client-fields`. They share configuration and workload files through
  symlinks into `test/correctness/shared/dsd-origin-detection-matrix/`.
- Integration has 27 cases on all three OSes, five on Linux/macOS, three on Linux/Windows, 10 Linux-only, one
  macOS-only and six Windows-only. Every integration `name` equals its directory name. The 52 files contain 218
  assertions, 23 actions and 42 parallel blocks.
- Typed correctness files declare `saluki-images/datadog-agent:testing-release` for baseline and comparison. The generator
  adds `.test-correctness-adp-baseline` for every case needing `agent-adp-linux-image`, including every selected
  correctness case. CI runs with `--artifacts`, binding both sides to that role with the same `bundled-agent-adp` image.
  `every_selected_correctness_case_declares_a_local_baseline_image` checks the declared baseline for the
  `saluki-images/` prefix so a published baseline cannot be silently replaced.
- Correctness baseline and comparison both bind to `agent-adp-linux-image`, matching CI's shared image. Its local
  default is `saluki-images/datadog-agent:testing-devel` (debug), while typed correctness files declare `:testing-release`
  (release). Artifact commands, text listings and native execution use the role binding. Typed runners use it with
  `run --artifacts`; without that flag, they use the files' images, subject to image overrides.
- Integration uses `saluki-images/datadog-agent:testing-devel` on Linux and
  `saluki-images/agent-data-plane:testing-windows` on Windows (bound in CI through `agent-adp-windows-image`).
  macOS uses host processes. `dogstatsd-forwarding`, `dogstatsd-host-tag` and `dogstatsd-intake-metric` declare a
  `datadog-intake` participant using `saluki-images/correctness-tools:latest` on Linux. The two intake metric cases
  also run on macOS with `intake-host-binary` and on Windows with `saluki-images/correctness-tools:testing-windows`
  through `tools-windows-image`.
- Both `dsd-forwarding` and `dsd-forwarding-burst` require captured forwarded DogStatsD packets.
- `datadog_intake.config_path` is ignored in `otlp-traces`, `otlp-traces-ets`, `otlp-traces-probabilistic` and
  `otlp-traces-probabilistic-full-trace-id`, including the two enabled probabilistic cases. The `apm-trace` cases set
  `millstone.config_path`, which the loader consumes.

## Pod host-view contract

Pod host mounts belong to participant definitions, matching the container recipe model. Realization options control
runtime behavior rather than product filesystem paths:

| Setting | Owner | Effect |
| --- | --- | --- |
| `realization.options.host_pid` | Pod backend | Shares the node PID namespace; adds no mounts. |
| `realization.options.cri_socket` | Pod backend | Requires a recipe mount of the node containerd socket; adds no mounts. |
| `config.params.host_view` | `agent-adp` and `agent` pod definitions | Adds the image's read-only proc, cgroup and CRI socket mounts. |

Origin-sensitive pod cases require `host_view: true`. Pod host view requires `host_pid: true`; planning rejects a
mismatch and names the participant. This replaces the earlier behavior where enabling realization options implicitly
mounted product-specific paths. Pod template files retain their
configuration names when rendering defers late references; creation rejects a deferred file unless its recipe
places it as a late file.

## Effective configuration scope

The JSON records the loader's configuration with defaults and matrix environments applied, before CLI image
overrides. For each subject OS, participant cases record participants' names, types, subject flags, realization kinds,
and planned case environment after OS overlays. Procedures retain their fields as written, including unresolved paths.
Use product `config_dump` output from CI runs to compare effective product configurations.

<!-- coverage-inventory:begin -->

## Coverage totals

| Suite | Files | Selected rows (subject OS / runtime) | Disabled rows | Assertions (subject OS) | Actions (subject OS) |
| --- | --- | --- | --- | --- | --- |
| integration | 52 | linux / linux-container: 45; macos / host-process-group: 33; windows / windows-container: 36 | 0 | linux: 193; macos: 141; windows: 155 | linux: 25; macos: 17; windows: 24 |
| correctness | 48 | linux / kubernetes-pod: 7; linux / linux-container: 39 | 2 | linux: 177 | linux: 7 |

Total: 162 rows.

## Coverage rows

| Case | Suite | Subject OS | Runtime | Status | CI job | Schema | Execution | Steps | Ignored |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| apm-trace | correctness | linux | linux-container | selected | test-correctness-apm-trace | participants | native | assertions: 4, actions: 0 | 0 |
| apm-trace-uds | correctness | linux | linux-container | selected | test-correctness-apm-trace-uds | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-added-tags | correctness | linux | linux-container | selected | test-correctness-dsd-added-tags | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-events | correctness | linux | linux-container | selected | test-correctness-dsd-events | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-forwarding | correctness | linux | linux-container | selected | test-correctness-dsd-forwarding | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-forwarding-burst | correctness | linux | linux-container | selected | test-correctness-dsd-forwarding-burst | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-host-tag | correctness | linux | linux-container | selected | test-correctness-dsd-host-tag | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-mapper-blocklist | correctness | linux | linux-container | selected | test-correctness-dsd-mapper-blocklist | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-metric-filterlist-prefix | correctness | linux | linux-container | selected | test-correctness-dsd-metric-filterlist-prefix | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-origin-detection | correctness | linux | linux-container | selected | test-correctness-dsd-origin-detection | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-origin-detection-matrix-legacy | correctness | linux | kubernetes-pod | selected | test-correctness-dsd-origin-detection-matrix-legacy | participants | native | assertions: 3, actions: 1 | 0 |
| dsd-origin-detection-matrix-legacy-high-cardinality | correctness | linux | kubernetes-pod | selected | test-correctness-dsd-origin-detection-matrix-legacy-high-cardinality | participants | native | assertions: 3, actions: 1 | 0 |
| dsd-origin-detection-matrix-legacy-optout-disabled | correctness | linux | kubernetes-pod | selected | test-correctness-dsd-origin-detection-matrix-legacy-optout-disabled | participants | native | assertions: 3, actions: 1 | 0 |
| dsd-origin-detection-matrix-legacy-with-client-fields | correctness | linux | kubernetes-pod | selected | test-correctness-dsd-origin-detection-matrix-legacy-with-client-fields | participants | native | assertions: 3, actions: 1 | 0 |
| dsd-origin-detection-matrix-unified | correctness | linux | kubernetes-pod | selected | test-correctness-dsd-origin-detection-matrix-unified | participants | native | assertions: 3, actions: 1 | 0 |
| dsd-origin-detection-matrix-unified-high-cardinality | correctness | linux | kubernetes-pod | selected | test-correctness-dsd-origin-detection-matrix-unified-high-cardinality | participants | native | assertions: 3, actions: 1 | 0 |
| dsd-origin-detection-matrix-unified-with-client-fields | correctness | linux | kubernetes-pod | selected | test-correctness-dsd-origin-detection-matrix-unified-with-client-fields | participants | native | assertions: 3, actions: 1 | 0 |
| dsd-plain | correctness | linux | linux-container | selected | test-correctness-dsd-plain | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-plain-v3 | correctness | linux | linux-container | selected | test-correctness-dsd-plain-v3 | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-provider-kind | correctness | linux | linux-container | selected | test-correctness-dsd-provider-kind | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-service-checks | correctness | linux | linux-container | selected | test-correctness-dsd-service-checks | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-tag-filterlist | correctness | linux | linux-container | selected | test-correctness-dsd-tag-filterlist | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-timestamp-extension | correctness | linux | linux-container | selected | test-correctness-dsd-timestamp-extension | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-timestamp-extension-custom-window | correctness | linux | linux-container | selected | test-correctness-dsd-timestamp-extension-custom-window | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-udp | correctness | linux | linux-container | selected | test-correctness-dsd-udp | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-uds-stream | correctness | linux | linux-container | selected | test-correctness-dsd-uds-stream | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-v1-api-series | correctness | linux | linux-container | selected | test-correctness-dsd-v1-api-series | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics | correctness | linux | linux-container | selected | test-correctness-otlp-metrics | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-as-type-rate | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-as-type-rate | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-configured-tags | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-configured-tags | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-core-resource-attributes | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-core-resource-attributes | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-cumulative-monotonic | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-cumulative-monotonic | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-ec2-host | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-ec2-host | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-fargate | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-fargate | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-gcp-host | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-gcp-host | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-histogram-counters | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-histogram-counters | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-histogram-nobuckets | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-histogram-nobuckets | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-host | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-host | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-kafka-parity | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-kafka-parity | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-resource-attributes | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-resource-attributes | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-scope-metadata | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-scope-metadata | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-metrics-summary-mode | correctness | linux | linux-container | selected | test-correctness-otlp-metrics-summary-mode | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-traces | correctness | linux | linux-container | disabled | — | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-traces-ets | correctness | linux | linux-container | disabled | — | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-traces-ottl-filtering | correctness | linux | linux-container | selected | test-correctness-otlp-traces-ottl-filtering | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-traces-ottl-transform | correctness | linux | linux-container | selected | test-correctness-otlp-traces-ottl-transform | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-traces-probabilistic | correctness | linux | linux-container | selected | test-correctness-otlp-traces-probabilistic | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-traces-probabilistic-full-trace-id | correctness | linux | linux-container | selected | test-correctness-otlp-traces-probabilistic-full-trace-id | participants | native | assertions: 4, actions: 0 | 0 |
| adp-cmd-port | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 6, actions: 0 | 0 |
| adp-cmd-port | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 6, actions: 0 | 0 |
| adp-cmd-port | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 6, actions: 0 | 0 |
| adp-config-check-exit | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| adp-config-check-exit | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-config-check-exit | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-config-check-no-warn | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| adp-config-check-no-warn | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-config-check-no-warn | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-config-check-warn | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| adp-config-check-warn | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-config-check-warn | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-config-dynamic-basic | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 5, actions: 1 | 0 |
| adp-config-dynamic-basic | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 5, actions: 1 | 0 |
| adp-config-dynamic-basic | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 5, actions: 1 | 0 |
| adp-config-dynamic-runtime | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 5, actions: 1 | 0 |
| adp-config-dynamic-runtime | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 5, actions: 1 | 0 |
| adp-config-dynamic-runtime | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 5, actions: 1 | 0 |
| adp-config-otlp | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 26, actions: 0 | 0 |
| adp-config-otlp | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 26, actions: 0 | 0 |
| adp-config-otlp | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 26, actions: 0 | 0 |
| adp-config-stream | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 9, actions: 0 | 0 |
| adp-config-stream | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 9, actions: 0 | 0 |
| adp-config-stream | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 9, actions: 0 | 0 |
| adp-disabled-exit | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 2, actions: 0 | 0 |
| adp-disabled-exit | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 2, actions: 0 | 0 |
| adp-disabled-exit | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 2, actions: 0 | 0 |
| adp-ipc-no-dns | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 4, actions: 0 | 0 |
| adp-logging-default-path | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 2, actions: 0 | 0 |
| adp-logging-ignores-core-agent-log-file | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| adp-logging-mac-ignores-core-agent-log-file | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-logging-respects-data-plane-log-file | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 2, actions: 0 | 0 |
| adp-logging-respects-data-plane-log-file | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 2, actions: 0 | 0 |
| adp-logging-windows-default-path | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 2, actions: 0 | 0 |
| adp-logging-windows-ignores-core-agent-log-file | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-logging-windows-respects-data-plane-log-file | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 2, actions: 0 | 0 |
| adp-memory-mode-disabled | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-disabled | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-disabled | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-exceeds-limit | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-exceeds-limit | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-exceeds-limit | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-within-limit | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-within-limit | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-within-limit | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-strict-exceeds-limit | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 1, actions: 0 | 0 |
| adp-memory-mode-strict-exceeds-limit | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 1, actions: 0 | 0 |
| adp-memory-mode-strict-exceeds-limit | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 1, actions: 0 | 0 |
| adp-memory-mode-strict-within-limit | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-strict-within-limit | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-strict-within-limit | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| adp-no-pipelines-exit | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 2, actions: 0 | 0 |
| adp-no-pipelines-exit | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 2, actions: 0 | 0 |
| adp-no-pipelines-exit | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 2, actions: 0 | 0 |
| adp-rar-disabled | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| adp-rar-registration | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 1 | 0 |
| adp-rar-registration | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 1 | 0 |
| adp-rar-registration | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 1 | 0 |
| adp-rc-core-disabled | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 5, actions: 0 | 0 |
| adp-rc-no-products | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 4, actions: 0 | 0 |
| apm-bind-custom-hostname | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| apm-bind-custom-hostname | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| basic-startup | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| basic-startup | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| basic-startup | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-autoscale-udp | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-autoscale-udp | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-autoscale-udp | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-bind-custom-hostname | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-bind-custom-hostname | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-bind-host | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-bind-host | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-default-bind | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-default-bind | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-default-bind | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-enabled | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-enabled | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-enabled | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-forwarding | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 13, actions: 5 | 0 |
| dogstatsd-host-tag | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 6, actions: 1 | 0 |
| dogstatsd-host-tag | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 6, actions: 1 | 0 |
| dogstatsd-host-tag | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 6, actions: 1 | 0 |
| dogstatsd-intake-metric | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 4, actions: 2 | 0 |
| dogstatsd-intake-metric | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 4, actions: 2 | 0 |
| dogstatsd-intake-metric | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 4, actions: 2 | 0 |
| dogstatsd-named-pipe-windows | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 9, actions: 6 | 0 |
| dogstatsd-no-listener-continues | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 4, actions: 0 | 0 |
| dogstatsd-no-listener-continues | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 4, actions: 0 | 0 |
| dogstatsd-non-local-overrides-bind-host | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-non-local-overrides-bind-host | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-non-local-overrides-bind-host | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-replay-unix | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 1 | 0 |
| dogstatsd-replay-unix | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 1 | 0 |
| dogstatsd-replay-windows | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 1 | 0 |
| dogstatsd-require-listener-exit | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-require-listener-exit | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 0 | 0 |
| dogstatsd-skip-failed-listener | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 4, actions: 0 | 0 |
| dogstatsd-skip-failed-listener | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 4, actions: 0 | 0 |
| dogstatsd-top | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 5, actions: 1 | 0 |
| dogstatsd-top-windows | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 5, actions: 1 | 0 |
| liveness-container-tags | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 4, actions: 1 | 0 |
| otlp-tls | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 1 | 0 |
| otlp-traces-enabled | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-traces-enabled | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 4, actions: 0 | 0 |
| otlp-traces-enabled | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 4, actions: 0 | 0 |
| privileged-api-endpoints | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 10 | 0 |
| privileged-api-endpoints | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 3, actions: 10 | 0 |
| privileged-api-endpoints | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 3, actions: 10 | 0 |
| telemetry-endpoint | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 5, actions: 0 | 0 |
| telemetry-endpoint | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 5, actions: 0 | 0 |
| telemetry-endpoint | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 5, actions: 0 | 0 |
| unprivileged-api-endpoints | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 5, actions: 0 | 0 |
| unprivileged-api-endpoints | integration | macos | host-process-group | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | native | assertions: 5, actions: 0 | 0 |
| unprivileged-api-endpoints | integration | windows | windows-container | selected | test-integration-windows-amd64 | participants | native | assertions: 5, actions: 0 | 0 |

<!-- coverage-inventory:end -->

## Logging baseline

The seven logging cases retain a frozen integration-schema baseline in
`bin/correctness/panoramic/src/config/coverage_inventory/logging_baseline.json`. Tests compare rows, steps,
participants, unresolved step paths and default-log-file use against it. Row comparisons allow the participant
schema, native execution, the participant's realization and role-keyed artifacts. Never regenerate the baseline.
The integration translation was removed with the bridge.
The logging cases drop the legacy `58125/udp` exposed port, which only `port_listening` used.

## Case migration notes

The macOS rows of `adp-logging-respects-data-plane-log-file` and
`adp-logging-mac-ignores-core-agent-log-file` run natively. Their log paths use the participant's per-run `shared_dir`,
so `file_contains` reads files from the current run. The Linux row of `adp-logging-respects-data-plane-log-file` also
uses `shared_dir`. Log steps search both processes' stdout and stderr.

The macOS definition declares Agent-internal listeners as endpoints so core can assign every listener's port.
Assigning the Agent OTLP keys enables the Agent's OTLP receivers on macOS, as the Linux defaults already do; the
legacy runner left them off. ADP OTLP proxying replaces the Agent's normal OTLP gRPC listener, so the definition
declares the proxy gRPC endpoint in that mode. The recipe configures the required gRPC protocol at ADP's gRPC
address when the case leaves it unset. Recipes receive their absolute participant directory through
`RenderedConfig::directory`. The Agent and ADP start together: ADP's IPC connect retry re-reads the token for about
20 seconds. Readiness gates consumers. The inventory uses `Planner::with_run` under `test-util` to keep substituted
host paths deterministic.

`dogstatsd-no-listener-continues`, `dogstatsd-require-listener-exit` and `dogstatsd-skip-failed-listener` run natively
on Linux and macOS. Their socket paths remain product configuration under `/dev/null`. The cases assert binding
failures for these unconsumed sockets. Stability watches the Agent and ADP's own exits. `exits_with` reads ADP's first
exit record on Linux and its waited status on macOS.

The skip-failed case checks ADP's UDP listener-started log line to prove the listener bound successfully.
A UDP connect alone succeeds even when the destination has no listener. Log steps search all processes' output,
plus the container log on Linux; the listener-started check names ADP.

## Native Remote Configuration cases

`adp-rc-core-disabled` and `adp-rc-no-products` use the participant schema and native Linux execution.

- Stability catches supervised ADP restarts by watching ADP's own exit record.
- Stability also covers the Agent: omitting `process` watches every declared process.
- `adp-rc-no-products` probes its `otlp_http` endpoint with `port_listening`.

Log steps omit `process` to search every process's output plus the container log.

`adp-rar-disabled` runs natively on Linux with Remote Agent Registry disabled on the Agent. ADP's registration
failure and supervised restarts are expected. ADP logs the registration-failure warning before its bootstrap failure
with `remote agent registry not enabled`. The case matches the registration warning, requires the Agent alone to
stay running for 10 seconds, and checks the participant's logs for panics over the same window.

## Case migration notes

`dogstatsd-host-tag` and `dogstatsd-intake-metric` use the participant schema and native execution on Linux,
macOS and Windows. On macOS the intake is a host process with assigned loopback ports, and the connection delivers
`DD_DD_URL=http://127.0.0.1:<port>`. On Windows it runs from `tools-windows-image` and is reached by participant name.
The Windows image adds `C:\tools\datadog-intake.exe` to the Agent LTSC base already used by the job.

- The intake is a participant connected by `connect: {intake: intake.api}`. The connection delivers `DD_DD_URL`
  and `DD_APM_DD_URL`.
- Sends reach `dogstatsd_udp` through the courier inside ADP's container, or directly on macOS.
- The listener gate is a port-free regex.
- Failed runs no longer capture the intake's contents under `traffic/`; participant artifact collection restores it.
- Stability watches both ADP and Agent exits, through records in containers and waited status on macOS.
- `intake_has_metric` waits out its timeout after a subject crash rather than ending early.
- Assertion and action counts are unchanged: six assertions and one action for `dogstatsd-host-tag`, four assertions
  and two actions for `dogstatsd-intake-metric`.

`dogstatsd-forwarding` also uses the participant schema and native Linux execution. Its procedure is rewritten,
not translated:

- The forward target moves from the Python receiver inside ADP's container to the intake participant, connected by
  `dogstatsd_forward`. ADP runs under its shipped supervisor instead of the Python wrapper.
- The seven script markers become six exact packet assertions and one HTTP assertion over parsed-metric telemetry.
  The HTTP pattern uses the observed `adp__component_events_received_total` name and requires UDP, metrics and a
  positive count on one line.
- A new UDP-listener log gate precedes sends. Payloads include their stream framing; refused stream and Unix socket
  sends retry until accepted.
- Stability watches both the ADP and Agent exit records instead of the wrapper's liveness.
- The case budget rises from 90 to 180 seconds, and the forwarding gate from 15 to 60 seconds, to allow for participant
  startup ordering. Assertion criteria remain unchanged.
- Counts change from 12 assertions and no actions to 13 assertions and five actions.

`adp-ipc-no-dns` uses native Linux execution. Its resolver-clearing hook is supplied through `cont-init.d/` and
runs with a `50-` prefix instead of `01-`. The unused exposed UDP port is removed. Stability watches both ADP's
and the Agent's exit records instead of container liveness. Environment, assertion count and timeout are unchanged.

`otlp-tls` uses native Linux execution. Its PEM is supplied through `files/`, and the four certificate/key settings
use the `files_dir` fact. The HTTP check reaches the named HTTPS endpoint through its assigned published port;
explicit exposed ports are removed. The gRPC handshake command uses `exec`, with the same exit-code and timeout
criteria; a failed execution mechanism now errors instead of failing. Stability watches both ADP's and the Agent's
exit records. Assertion/action counts and timeouts are unchanged.

`dogstatsd-top` uses native Linux execution. Its Python script and fixtures are supplied through `files/`, and the
script locates fixtures next to itself. A listener-log regex replaces the UDP port probe, which could not prove a
bind; non-local traffic remains disabled and the unused exposed UDP port is removed. The script uses `exec`, with
the same exit-code and timeout criteria; execution-mechanism failures now error instead of failing. Stability
watches both ADP's and the Agent's exit records. Assertion/action counts and timeouts are unchanged.

`liveness-container-tags` uses native Linux execution with three participants. A Linux `fixture` serves the fake
Docker API instead of wrapping the Agent entrypoint. It receives ADP's container ID after ADP is created; ADP starts
only after the fixture's Unix socket and the real mock intake are ready. The fake API's three Python tests run through
`exec` on the fixture. Execution-mechanism failures now error instead of failing. The marker-file assertions become
`intake_has_metric` and `intake_has_service_check`, preserving the hostname, value/status, image and label tags, and
nonempty version-tag criteria. `max_age: 30s` preserves timestamp freshness, including rejecting excessive future
skew; the gauge's latest point supplies both value and timestamp. ADP still sends v1-series payloads with gzip.
Stability now observes both ADP and Agent exit records. The case timeout rises from 120 to 180 seconds for ordered
participant startup. ADP shares the host cgroup namespace to discover its own container ID, with no host binds.
Assertion and action counts remain four and one.

`adp-config-dynamic-basic`, `adp-config-dynamic-runtime`, `adp-config-otlp`, `adp-config-stream`,
`adp-rar-registration` and `privileged-api-endpoints` use native participant execution on all three OSes.
Configuration assertions use `config_dump` with the source/runtime view, and mutations use the Agent's
`config_set` operation with a 30-second timeout. CLI operations check output, including all five
`privileged-api-endpoints` assertions. CLI mechanism failures, including timeouts, error rather than fail.
Stability observes both ADP and Agent processes. The OTLP ports under test are pinned product configuration,
including on macOS.

`dogstatsd-replay-unix` uses native execution on Linux and macOS; `dogstatsd-replay-windows` uses it on Windows.
The Unix socket lives in the participant's shared directory. Endpoint sends carry the payload bytes, with a
one-second delay between Unix sends. Capture, replay and statistics use participant operations. Unix payloads
use one connection per send; Windows sends its two newline-terminated metrics together. Mechanism failures
error, product failures fail, and stability watches both processes.

## Host operation execution

With `OpContext::exec_process`, host operations run a declared process's
artifact-bound program with replacement arguments, in its environment and participant directory. Other realizations
return a harness error. This keeps host artifact paths inside the backend and product CLI forms inside the type.

## Environment-only case migration notes

The following cases use native participants on Linux, macOS and Windows:
`adp-cmd-port`, `adp-config-check-exit`, `adp-config-check-no-warn`, `adp-config-check-warn`,
`adp-disabled-exit`, `adp-memory-mode-disabled`, `adp-memory-mode-permissive-exceeds-limit`,
`adp-memory-mode-permissive-within-limit`, `adp-memory-mode-strict-exceeds-limit`,
`adp-memory-mode-strict-within-limit`, `basic-startup`, `dogstatsd-autoscale-udp`, `dogstatsd-enabled`,
`telemetry-endpoint` and `unprivileged-api-endpoints`. Their environments, descriptions and timeouts are unchanged.
`adp-cmd-port` still pins `DD_CMD_PORT` to 7777.

- Stability watches both Agent and ADP processes on every OS. Legacy execution watched the container on Linux
  and only ADP on macOS.
- `exits_with` reads ADP's first exit record in Linux and Windows containers, or its waited status on macOS,
  rather than a supervisor log line. `adp-disabled-exit` gains an explicit exit-code-0 assertion after its existing
  log check, increasing coverage.
- API checks name `api` instead of a published localhost port. Exposed-port declarations are removed.
- `dogstatsd-enabled` and `dogstatsd-autoscale-udp` replace a UDP probe that could not fail with ADP's UDP
  listener-started log line. Autoscale keeps one listener gate instead of also checking a generic listener line.
  This fixes coverage of whether the UDP listener actually bound.
- Log steps search both processes' output, preserving Linux coverage and widening Windows coverage to the Agent.
  The two listener gates explicitly name ADP.

`adp-no-pipelines-exit` checks the no-pipelines log assertion and ADP's first exit with code 1, matching the
case's error-exit contract. It runs natively on Linux, macOS and Windows. The exit assertion has a 60-second
timeout; the case has a 120-second timeout.

`adp-config-check-warn` escapes the log separator in its regex: the intended line contains both the named key and
its unsupported-key warning, separated by a literal pipe.

### Bind addresses and Windows payloads

`dogstatsd-bind-host`, `dogstatsd-bind-custom-hostname` and `apm-bind-custom-hostname` assert listener addresses
on Linux and Windows. Own-IP substitution and `extra_hosts` configure the bind address; the assertion reads the
backend's IP late fact. Stability watches both declared processes. The cases cover the issue-#1331 bind-host behavior.

`dogstatsd-named-pipe-windows` uses six `send` actions and nine assertions. The oversize payload uses `repeat: 10000`.
Each send opens one connection. Retries can duplicate delivery, but the assertions check presence or absence of
distinct strings, not counts.

`dogstatsd-top-windows` runs `files/run_dogstatsd_top_test.ps1` with `exec` through `files_dir` and checks its result
file. Its listener-log regex, shared with `dogstatsd-top`, proves that a UDP listener started.
Stability watches both processes.

`dogstatsd-default-bind` and `dogstatsd-non-local-overrides-bind-host` assert fixed `:58125` listener patterns and
pin `DD_DOGSTATSD_PORT` to that value in the macOS overlay. The port is product configuration under test, and CI runs
host cases sequentially. `otlp-traces-enabled` pins its OTLP ports and names `otlp_grpc` and `otlp_http` for probes.
These cases watch both declared processes for stability on every OS.

### Coverage ledger

The ledger test pins all 52 original integration cases and their subject OSes from `0668d6db67`, checks the rows
individually, and preserves each original CI job, including both macOS architectures. The only intended additions
are `dogstatsd-host-tag` and `dogstatsd-intake-metric` on macOS and Windows. Every integration row must use native
execution, with no exceptions. The legacy Linux integration runner remains implemented, with no integration cases
selecting it.

## Migration proofs

When changing a case's schema or runner, add CI evidence per (case, subject OS, execution). Native rows may say
`Pending CI.` until CI runs. Each completed proof names the tested commit and links its pipeline, links the passing CI
jobs, identifies the runner and execution value, and names the commit whose inventory the case was compared with.
Link CI runs; don't copy their logs into the repository.

<!-- migration-proofs:begin -->

| Case | Subject OS | Execution | Proof |
| --- | --- | --- | --- |
| adp-logging-default-path | linux | native | Pending CI. Before: Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351927) passed on the integration runner (execution `old`). Compared with the inventory at `b975a993e0`. |
| adp-logging-ignores-core-agent-log-file | linux | native | Pending CI. Before: Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351927) passed on the integration runner (execution `old`). Compared with the inventory at `b975a993e0`. |
| adp-logging-respects-data-plane-log-file | linux | native | Pending CI. Before: Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351927) passed on the integration runner (execution `old`). Compared with the inventory at `b975a993e0`. |
| adp-logging-respects-data-plane-log-file | macos | native | Pending CI. Before: Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration-macos-arm64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351928) and [test-integration-macos-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351929) passed on the macOS host-process runner (execution `old`). Compared with the inventory at `b975a993e0`. Caveat: the case writes to a fixed `/tmp` path that nothing deletes, so `file_contains` could match a file left by an earlier run on the same host. Compared with the inventory at `97f0507856`. |
| adp-logging-mac-ignores-core-agent-log-file | macos | native | Pending CI. Before: Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration-macos-arm64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351928) and [test-integration-macos-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351929) passed on the macOS host-process runner (execution `old`). Compared with the inventory at `b975a993e0`. Caveat: the case writes to fixed `/tmp` paths that nothing deletes, so `file_contains` could match a file left by an earlier run on the same host. Compared with the inventory at `97f0507856`. |
| adp-logging-windows-default-path | windows | native | Pending CI. Before: Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration-windows-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351930) passed on the integration runner (execution `old`). Compared with the inventory at `b975a993e0`. |
| adp-logging-windows-ignores-core-agent-log-file | windows | native | Pending CI. Before: Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration-windows-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351930) passed on the integration runner (execution `old`). Compared with the inventory at `b975a993e0`. |
| adp-logging-windows-respects-data-plane-log-file | windows | native | Pending CI. Before: Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration-windows-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351930) passed on the integration runner (execution `old`). Compared with the inventory at `b975a993e0`. |
| adp-rc-core-disabled | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `6187914497`. |
| adp-rc-no-products | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `6187914497`. |
| dogstatsd-forwarding | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `79e8059480`. |
| dogstatsd-host-tag | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `79e8059480`. |
| dogstatsd-intake-metric | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `79e8059480`. |
| adp-ipc-no-dns | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `213e0bec60`. |
| otlp-tls | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `213e0bec60`. |
| dogstatsd-top | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `213e0bec60`. |
| liveness-container-tags | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `213e0bec60`. |
| dogstatsd-no-listener-continues | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `2624b2c308`. |
| dogstatsd-no-listener-continues | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `2624b2c308`. |
| dogstatsd-require-listener-exit | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `2624b2c308`. |
| dogstatsd-require-listener-exit | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `2624b2c308`. |
| dogstatsd-skip-failed-listener | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `2624b2c308`. |
| dogstatsd-skip-failed-listener | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `2624b2c308`. |
| dogstatsd-host-tag | macos | native | Pending CI. Before: none; the case ran only on Linux. Compared with the inventory at `d37e52ec98`. |
| dogstatsd-host-tag | windows | native | Pending CI. Before: none; the case ran only on Linux. Compared with the inventory at `d37e52ec98`. |
| dogstatsd-intake-metric | macos | native | Pending CI. Before: none; the case ran only on Linux. Compared with the inventory at `d37e52ec98`. |
| dogstatsd-intake-metric | windows | native | Pending CI. Before: none; the case ran only on Linux. Compared with the inventory at `d37e52ec98`. |
| adp-config-dynamic-basic | linux | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-dynamic-basic | macos | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-dynamic-basic | windows | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-dynamic-runtime | linux | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-dynamic-runtime | macos | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-dynamic-runtime | windows | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-otlp | linux | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-otlp | macos | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-otlp | windows | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-stream | linux | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-stream | macos | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-config-stream | windows | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-rar-registration | linux | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-rar-registration | macos | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-rar-registration | windows | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| privileged-api-endpoints | linux | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| privileged-api-endpoints | macos | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| privileged-api-endpoints | windows | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| dogstatsd-replay-unix | linux | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| dogstatsd-replay-unix | macos | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| dogstatsd-replay-windows | windows | native | Pending CI. Compared with the inventory at `4e9948b5ed` (execution `old`). |
| adp-cmd-port | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-cmd-port | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-cmd-port | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-config-check-exit | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-config-check-exit | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-config-check-exit | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-config-check-no-warn | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-config-check-no-warn | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-config-check-no-warn | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-config-check-warn | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-config-check-warn | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-config-check-warn | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-disabled-exit | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-disabled-exit | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-disabled-exit | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-disabled | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-disabled | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-disabled | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-permissive-exceeds-limit | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-permissive-exceeds-limit | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-permissive-exceeds-limit | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-permissive-within-limit | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-permissive-within-limit | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-permissive-within-limit | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-strict-exceeds-limit | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-strict-exceeds-limit | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-strict-exceeds-limit | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-strict-within-limit | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-strict-within-limit | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| adp-memory-mode-strict-within-limit | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| basic-startup | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| basic-startup | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| basic-startup | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| dogstatsd-autoscale-udp | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| dogstatsd-autoscale-udp | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| dogstatsd-autoscale-udp | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| dogstatsd-enabled | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| dogstatsd-enabled | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| dogstatsd-enabled | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| telemetry-endpoint | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| telemetry-endpoint | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| telemetry-endpoint | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| unprivileged-api-endpoints | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `4e9948b5ed`. |
| unprivileged-api-endpoints | macos | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-macos-arm64` and `test-integration-macos-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| unprivileged-api-endpoints | windows | native | Pending CI. Before: passed with execution `old` in the last green `test-integration-windows-amd64` run before this change. Compared with the inventory at `4e9948b5ed`. |
| dogstatsd-bind-host | linux | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-bind-host | windows | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-bind-custom-hostname | linux | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-bind-custom-hostname | windows | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| apm-bind-custom-hostname | linux | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| apm-bind-custom-hostname | windows | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-named-pipe-windows | windows | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-top-windows | windows | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-default-bind | linux | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-default-bind | macos | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-default-bind | windows | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-non-local-overrides-bind-host | linux | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-non-local-overrides-bind-host | macos | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| dogstatsd-non-local-overrides-bind-host | windows | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| otlp-traces-enabled | linux | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| otlp-traces-enabled | macos | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| otlp-traces-enabled | windows | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| adp-no-pipelines-exit | linux | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| adp-no-pipelines-exit | macos | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| adp-no-pipelines-exit | windows | native | Pending CI. Compared with the inventory at `36f1a365cf`. |
| adp-rar-disabled | linux | native | Pending CI. Two native Linux/lima runs passed: ADP logged registration failure and restarted after exit 1 while the Agent stayed running for 10 seconds; no panic matched. Compared with the inventory at `26f84fe969`. |
| dsd-plain | linux | native | Pending CI. Legacy and native Linux/lima runs passed against the same image; paired captures contain 10,000 identical payloads. Compared with `778c155cee`. |
| dsd-origin-detection | linux | native | Pending CI. Legacy passed; native host-view file placement fails on rootless lima with `remount-ro /host/proc: operation not permitted`. With the required origin-tag assertion, a scratch run without host view fails on both sides for missing `image_name` (42.786 s case elapsed). Legacy Docker metadata collection was also unavailable; host view and actual enrichment remain unproven locally. Compared with `778c155cee`. |
| dsd-added-tags | linux | native | Pending CI. Legacy Linux/lima passed (68.228 s wall); native at `a18ec636c4` passed (55.457 s wall). Earlier paired captures have matching input digests and only documented config differences. Compared with `5003c5f6d6`. |
| dsd-events | linux | native | Pending CI. Native Linux/lima at `a18ec636c4` passed (46.500 s wall); env, product files and workload match the mechanical conversion. Compared with `5003c5f6d6`. |
| dsd-host-tag | linux | native | Pending CI. Native Linux/lima at `a18ec636c4` passed (53.995 s wall); env, product files and workload match the mechanical conversion. Compared with `5003c5f6d6`. |
| dsd-mapper-blocklist | linux | native | Pending CI. Native Linux/lima at `a18ec636c4` passed (44.090 s wall); env, product files and workload match the mechanical conversion. Compared with `5003c5f6d6`. |
| dsd-metric-filterlist-prefix | linux | native | Pending CI. Legacy passed (62.719 s wall); native host-view placement fails with `remount-ro /host/proc: operation not permitted`. Before the origin assertion, scratch without host view passed (48.825 s), with 176 matching contexts/tag keys per side, matching input digest and only documented config differences. Requiring `image_name` now makes that scratch fail on both sides (48.694 s case elapsed). Host view and enrichment remain unproven locally. Compared with `5003c5f6d6`. |
| dsd-plain-v3 | linux | native | Pending CI. Legacy Linux/lima passed (68.408 s wall); native at `a18ec636c4` failed comparison (52.955 s wall; 3025 metric mismatches); an unchanged rerun failed (52.778 s wall; 28 metric mismatches). Earlier paired captures have matching input digests and only documented config differences. Compared with `5003c5f6d6`. |
| dsd-provider-kind | linux | native | Pending CI. Native Linux/lima at `a18ec636c4` failed comparison (49.341 s wall; 28 metric mismatches); an unchanged rerun passed (53.955 s wall); env, product files and workload match the mechanical conversion. Compared with `5003c5f6d6`. |
| dsd-service-checks | linux | native | Pending CI. Native Linux/lima at `a18ec636c4` passed (56.350 s wall); env, product files and workload match the mechanical conversion. Compared with `5003c5f6d6`. |
| dsd-tag-filterlist | linux | native | Pending CI. Legacy passed (66.420 s wall); native host-view placement fails with `remount-ro /host/proc: operation not permitted`. Before the origin assertion, scratch without host view passed (54.403 s), with 178 matching contexts/tag keys per side, matching input digest and only documented config differences. Requiring `image_name` now makes that scratch fail on both sides (48.708 s case elapsed). Host view and enrichment remain unproven locally. Compared with `5003c5f6d6`. |
| dsd-timestamp-extension | linux | native | Pending CI. Native Linux/lima at `a18ec636c4` passed (95.092 s wall); env, product files and workload match the mechanical conversion. Compared with `5003c5f6d6`. |
| dsd-timestamp-extension-custom-window | linux | native | Pending CI. Native Linux/lima at `a18ec636c4` passed (111.201 s wall); env, product files and workload match the mechanical conversion. Compared with `5003c5f6d6`. |
| dsd-v1-api-series | linux | native | Pending CI. Native Linux/lima at `a18ec636c4` failed comparison (48.738 s wall; 3026 metric mismatches); an unchanged rerun passed (49.764 s wall); env, product files and workload match the mechanical conversion. Compared with `5003c5f6d6`. |
| dsd-uds-stream | linux | native | Pending CI. Legacy Linux/lima passed (74.538 s wall); native at `a18ec636c4` failed comparison (58.298 s wall; 11 metric mismatches); an unchanged rerun passed (55.605 s wall). Earlier paired captures have matching input digests and only documented config differences. Compared with `5003c5f6d6`. |
| dsd-udp | linux | native | Pending CI. Legacy Linux/lima passed (78.216 s wall); native at `a18ec636c4` failed comparison (57.952 s wall; 672 metric mismatches); an unchanged rerun passed (50.661 s wall). Earlier paired captures have matching input digests and only documented config differences. Compared with `5003c5f6d6`. |
| dsd-forwarding | linux | native | Pending CI. Legacy Linux/lima passed (72.205 s wall); native at `a18ec636c4` passed (49.683 s wall). Earlier paired captures have matching input digests and only documented config differences. Those captures contain 100/6897 Agent/ADP packets with the same 6897 messages per side. Compared with `5003c5f6d6`. |
| dsd-forwarding-burst | linux | native | Pending CI. Legacy Linux/lima passed (68.668 s wall); native at `a18ec636c4` passed (48.843 s wall). Earlier paired captures have matching input digests and only documented config differences. Those captures contain 10/2447 Agent/ADP packets with the same 2447 messages per side. Compared with `5003c5f6d6`. |
| otlp-metrics | linux | native | Pending CI. Legacy/native Linux/lima passed (66.579→51.544 s wall); input digests and OTLP runtime settings match. Config dumps differ only in documented routing, defaults and host roots. Compared with `56bdfaf8fb`. |
| otlp-metrics-as-type-rate | linux | native | Pending CI. Legacy/native Linux/lima passed (67.201→46.844 s wall); input digests and OTLP runtime settings match. Config dumps differ only in documented routing, defaults and host roots. Compared with `56bdfaf8fb`. |
| otlp-metrics-configured-tags | linux | native | Pending CI. Native Linux/lima passed (49.147 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-core-resource-attributes | linux | native | Pending CI. Native Linux/lima passed (40.388 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-cumulative-monotonic | linux | native | Pending CI. Native Linux/lima passed (49.461 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-ec2-host | linux | native | Pending CI. Native Linux/lima passed (39.990 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-fargate | linux | native | Pending CI. Native Linux/lima passed (40.571 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-gcp-host | linux | native | Pending CI. Native Linux/lima passed (39.149 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-histogram-counters | linux | native | Pending CI. Legacy/native Linux/lima passed (71.531→40.096 s wall); input digests and OTLP runtime settings match. Config dumps differ only in documented routing, defaults and host roots. Compared with `56bdfaf8fb`. |
| otlp-metrics-histogram-nobuckets | linux | native | Pending CI. Native Linux/lima passed (39.929 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-host | linux | native | Pending CI. Native Linux/lima passed (42.175 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-kafka-parity | linux | native | Pending CI. Native Linux/lima passed (46.469 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-resource-attributes | linux | native | Pending CI. Native Linux/lima passed (41.403 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-scope-metadata | linux | native | Pending CI. Native Linux/lima passed (40.133 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-metrics-summary-mode | linux | native | Pending CI. Native Linux/lima passed (40.285 s wall); paired sender digests match. Plan-level env and product/workload bytes match after the address rewrites. Compared with `56bdfaf8fb`. |
| otlp-traces-ottl-filtering | linux | native | Pending CI. Legacy passed (68.330 s), as did an isolated dump-capture repeat (66.019 s); native passed (47.744 s). Inputs match; collector settings differ only in the delivered exporter URL. Configuration differences are limited to documented routing and type defaults. Compared with `4a461d30e5`. |
| otlp-traces-ottl-transform | linux | native | Pending CI. Native Linux/lima passed (59.719 s). Paired input digests match; environments, collector processors, corpus, comparison mode and ignore fields preserve the legacy case. Compared with `4a461d30e5`. |
| otlp-traces-probabilistic | linux | native | Pending CI. Legacy/native Linux/lima passed (69.378→47.945 s wall), with identical 1,000-payload input digests. Sampling settings and ignore fields are unchanged; ignored intake config_path removed. Compared with `4a461d30e5`. |
| otlp-traces-probabilistic-full-trace-id | linux | native | Pending CI. Native Linux/lima passed (51.046 s). Paired input digests match; full-trace-ID sampling settings and ignore fields are unchanged; ignored intake config_path removed. Compared with `4a461d30e5`. |
| apm-trace | linux | native | Pending CI. Legacy/native Linux/lima passed (65.064→46.667 s wall), with identical 100-payload input digests and unchanged TCP/socket ownership. Compared with `4a461d30e5`. |
| apm-trace-uds | linux | native | Pending CI. Legacy passed (74.284 s); a dump-capture repeat was interrupted by the enclosing validation tool timeout before comparison; the next repeat passed (69.708 s). Checked-in native host view errored on the rootless /host/proc remount (2.087 s). No-host-view attempt 1 was interrupted after the injected APM port caused ADP to collide with the trace-agent (147.874 s); the participant default was fixed without changing the case or comparison. Attempt 2 errored because another invocation held runner ports 20000/20001 (4.089 s). Attempt 3 failed with 8 mismatched stats groups (58.304 s); unchanged attempt 4 passed (49.662 s). Legacy/final native inputs match; both have 17,057 spans per side and matching tag-key sets. Container enrichment remains unproven locally. Compared with `4a461d30e5`. |
| otlp-traces | linux | native | Pending CI. Not selected; compiled only. Linux list reports runnable; the inventory retains disabled status and no CI jobs or mixins. The disabling TODO is retained and ignored intake config_path removed. Compared with `4a461d30e5`. |
| otlp-traces-ets | linux | native | Pending CI. Not selected; compiled only. Linux list reports runnable; the inventory retains disabled status and no CI jobs or mixins. Error-tracking settings and the disabling TODO are retained; ignored intake config_path removed. Compared with `4a461d30e5`. |
| dsd-origin-detection-matrix-legacy | linux | native | Pending CI. Before: passed with execution `old` as dsd-origin-detection-matrix/legacy in the last green `test-correctness-dsd-origin-detection-matrix-legacy` run before this change. Compared with the inventory at `b5babaec96`. |
| dsd-origin-detection-matrix-unified | linux | native | Pending CI. Before: passed with execution `old` as dsd-origin-detection-matrix/unified in the last green `test-correctness-dsd-origin-detection-matrix-unified` run before this change. Compared with the inventory at `b5babaec96`. |
| dsd-origin-detection-matrix-legacy-with-client-fields | linux | native | Pending CI. Before: passed with execution `old` as dsd-origin-detection-matrix/legacy-with-client-fields in the last green `test-correctness-dsd-origin-detection-matrix-legacy-with-client-fields` run before this change. Compared with the inventory at `b5babaec96`. |
| dsd-origin-detection-matrix-unified-with-client-fields | linux | native | Pending CI. Before: passed with execution `old` as dsd-origin-detection-matrix/unified-with-client-fields in the last green `test-correctness-dsd-origin-detection-matrix-unified-with-client-fields` run before this change. Compared with the inventory at `b5babaec96`. |
| dsd-origin-detection-matrix-legacy-high-cardinality | linux | native | Pending CI. Before: passed with execution `old` as dsd-origin-detection-matrix/legacy-high-cardinality in the last green `test-correctness-dsd-origin-detection-matrix-legacy-high-cardinality` run before this change. Compared with the inventory at `b5babaec96`. |
| dsd-origin-detection-matrix-unified-high-cardinality | linux | native | Pending CI. Before: passed with execution `old` as dsd-origin-detection-matrix/unified-high-cardinality in the last green `test-correctness-dsd-origin-detection-matrix-unified-high-cardinality` run before this change. Compared with the inventory at `b5babaec96`. |
| dsd-origin-detection-matrix-legacy-optout-disabled | linux | native | Pending CI. Before: passed with execution `old` as dsd-origin-detection-matrix/legacy-optout-disabled in the last green `test-correctness-dsd-origin-detection-matrix-legacy-optout-disabled` run before this change. Compared with the inventory at `b5babaec96`. |
<!-- migration-proofs:end -->

### Kubernetes origin detection evidence

The seven flat cases preserve the former matrix's per-side environment overlays, workload, timeouts, sender-exit
checks and metric comparison. Only delivered endpoint paths and late container/pod identity references replace the
legacy placeholders. `host_view: true` mounts the node's proc, cgroup filesystem and containerd socket; `host_pid`
shares the node PID namespace; `cri_socket` requires the node socket to exist and be mounted by the recipe.
The Millstone round-trip test reads the shared workload directly with those references intact. Comparisons also
require at least one workload context with `image_name` on each side, plus `container_id` for both high-cardinality
variants, so losing origin enrichment on both sides cannot produce a passing comparison.

With the required-tag assertions applied to `0e559af67e`, a native kind run of `legacy-high-cardinality` passed on
its first attempt in 47.580 seconds, with zero metric mismatches. Both sides emitted 3,028 workload contexts, all
carrying `image_name` and `container_id`. No retry was needed.

The local runs and controls below predate the rebase onto the base used by `a1b1087b88`; the proof rows compare
inventories against the current pre-migration ancestor `b5babaec96`, not runtime evidence gathered on that tree.
Local Linux/kind validation ran each variant twice, serially, using the same `testing-devel` Agent/ADP image and
`correctness-tools:latest` tools image. Six of fourteen runs passed; these results are not an all-green correctness
proof. A zero mismatch count means the case passed; every nonzero count below was a comparison failure.

| Variant | Run 1 seconds | Run 1 mismatches | Run 2 seconds | Run 2 mismatches |
| --- | ---: | ---: | ---: | ---: |
| legacy | 52.819 | 3,027 | 55.708 | 26 |
| legacy-high-cardinality | 51.269 | 26 | 50.210 | 0 |
| legacy-optout-disabled | 56.966 | 0 | 50.242 | 0 |
| legacy-with-client-fields | 51.179 | 8,567 | 49.085 | 70 |
| unified | 49.174 | 26 | 50.124 | 0 |
| unified-high-cardinality | 49.710 | 0 | 51.059 | 0 |
| unified-with-client-fields | 50.181 | 8,438 | 49.160 | 71 |

The 26-mismatch `legacy` repeat reproduces the earlier histogram-derived gauge bucket split: the Agent emitted one
bucket and ADP two. The first `legacy-high-cardinality` and `unified` runs had the same 26-gauge pattern. The second
client-field runs had 70 and 71 such histogram-gauge mismatches. The larger first-run failures also affected rates,
ordinary gauges and sketches; their cause remains unresolved. No workload, assertion or comparison tolerance was
changed to make these runs pass.

Two controls used the original matrix files, unchanged, before that rebase and with the same artifacts. Legacy
`legacy` passed in 55.037 seconds. Legacy `legacy-with-client-fields` failed in 50.434 seconds with 71 histogram-gauge
mismatches, again one Agent bucket versus two ADP buckets. That reproduces the client-field bucket-split failure
without the migration; it does not explain the broader first-run failures.

Decoded captures from all fourteen native runs had matching workload contexts and origin-tag counts and values
between baseline and subject. The five variants without client fields had 3,028 contexts per side, each carrying
`image_id`, `image_name`, `image_tag` and `short_image`; only the two high-cardinality variants also had `container_id`
on all contexts. Each client-field variant had 9,084 contexts per side, image tags on 6,056 and `container_id` on 3,028.
No side emitted `container_name`, `pod_name`, `kube_namespace`, `kube_container_name` or `pod_uid` on workload metrics.
All fourteen runs reported no harness issues and reclaimed every participant, namespace and node directory.

A scratch copy of `legacy`, with only the subject's tag cardinality changed to `high`, failed as expected in 50.743
seconds: 3,028 contexts appeared only in each side. Both decoded captures, their manifest and ten container/process
log files survived the comparison failure. The reclaim ledger and direct checks confirmed removal of all five
participants, Services, the namespace and node directory. The scratch case is not part of the checked-in suite.

Inventory checks still select 46 Linux correctness rows, with seven native Kubernetes-pod rows and no matrix
expansion. All non-matrix rows and effective configurations are unchanged. CI generation changed only these seven
selection variables, with job names, ordering and mixins preserved:

```diff
-    CORRECTNESS_TEST_CASE: dsd-origin-detection-matrix/<variant>
+    CORRECTNESS_TEST_CASE: dsd-origin-detection-matrix-<variant>
```

Unit tests of the four Panoramic packages, `make check-docs` and the kind scenarios passed.

Earlier pre-rebase paired proofs recorded these case wall times: `legacy` 55.445 seconds old → 51.811 seconds native;
`legacy-high-cardinality` 51.014 → 48.870 seconds. Together with the controls and repeats above, these are the retained
measurements. No legacy timing was recorded for `legacy-optout-disabled`, `unified`, `unified-high-cardinality` or
`unified-with-client-fields`.

The earlier paired proofs also recorded effective configuration differences for both variants: five leaves in the
baseline Agent, fifteen in the subject Agent and fourteen in ADP. They were limited to delivered intake/APM URLs,
socket paths, explicit baseline ADP disablement, `/host/proc` and the existing Linux listener defaults. No other
configuration leaf or workload/origin setting differed. The recipes explicitly set both `DD_PROC_ROOT=/host/proc`
and `DD_CGROUP_ROOT=/host/sys/fs/cgroup`; only the subject receives port-isolation defaults. This is the retained
summary of the pre-rebase comparison, not a fresh comparison at HEAD; its raw config dumps are no longer retained.

### DogStatsD discovery and configuration evidence

Compared with `5003c5f6d6`, all 46 discovered correctness names remain runnable and their artifact-role arrays are
unchanged. The sixteen DogStatsD cases now use five named Linux-container participants. All 162 inventory rows keep
their CI jobs and mixins byte-for-byte. All fourteen cases without host view were rerun on `a18ec636c4`, one per
invocation: 9 passed initially; 4 of the 5 comparison failures passed on unchanged reruns. `dsd-plain-v3` still fails
comparison. Their rows retain both attempts and the new wall times. Every sender pair has matching input counts and digests.
The two host-view rows retain their earlier negative origin-tag proof; host view and enrichment remain pending CI.
Earlier captures from eight legacy/native pairs match input digests and have no configuration differences beyond
the documented addresses, listener defaults, baseline ADP disablement and host roots.

### OTLP metrics discovery and configuration evidence

Compared with `56bdfaf8fb`, all 46 correctness names remain runnable with unchanged artifact-role arrays; exactly
fifteen layouts become participant cases. All 162 inventory rows retain their CI jobs and mixins. Every converted
case preserves its environment and product/workload file bytes after the intake and target-address rewrites.
All fifteen native runs pass and their paired senders have identical input counts and digests.

Legacy/native pairs for `otlp-metrics`, `otlp-metrics-as-type-rate` and `otlp-metrics-histogram-counters` also match
input digests: respectively 299/1/1 payloads, 1,592,458/112/108 bytes, and FNV-1a digests `7c2c8ef4ca382913`,
`243e7e41e0f01ebe`, `e31e35923096c394`. Wall times, including the capture wrapper, appear in the proof rows.

Each pair's Agent dumps differ in three baseline and twelve subject leaves: delivered intake/APM URLs, explicit
baseline ADP disablement, and the existing isolated listener and GUI defaults. ADP source dumps differ in 14/7/7
leaves and runtime dumps in 8/6/6; after normalizing routing, listener defaults and host roots, no difference remains.
In both runners, the baseline's file-defined gRPC ports stay 54317/54317/4317, and the subject's Agent receivers move
to loopback ports 14317/14318. ADP's OTLP runtime configuration is identical, including its gRPC ports 54317/54317/4317.
The two standalone source dumps gain the native Agent HTTP default `0.0.0.0:54318`; ADP's HTTP receiver stays at
`localhost:6318`. This source-only default does not change the receiver used by either sender.

### OTLP trace and APM discovery and configuration evidence

Compared with `4a461d30e5`, the six selected trace cases now declare five Linux-container participants. The same
46 selected correctness names remain runnable with unchanged artifact-role arrays, and all 162 inventory rows keep
their CI jobs and mixins. Only `dsd-origin-detection-matrix` remains typed. The two disabled OTLP files compile as
ordinary participants: `list -d test/correctness/disabled --subject-os linux` reports both runnable, but their
inventory status remains disabled and CI does not select them. Neither was executed. Their disabling TODOs remain.
The two probabilistic cases and both disabled files drop `datadog_intake.config_path`, which the legacy loader ignored.

All six selected cases pass locally, with `apm-trace-uds` run from a scratch copy omitting only host view. Its first
completed comparison failed with eight stats-group differences; an unchanged retry passed. Every attempt, including
setup errors and interrupted captures, is recorded in its proof row. No workload, comparison or ignore field was
changed to obtain a pass. Host-view and container-enrichment proof remain pending CI.

Legacy/native pairs for `otlp-traces-ottl-filtering`, `otlp-traces-probabilistic`, `apm-trace` and `apm-trace-uds`
match seeds, counts, payload bytes and digests. Each OTLP pair contains 1,000 payloads and 1,000,824 bytes, digest
`aee9ab0c871300e5`; each APM pair contains 100 payloads, digest `d580876d7e24944e`. Native sender pairs match too.
The successful APM UDS pair has 17,057 spans on each side, with equal tag-key sets, without proving container tags.

OTTL filtering and APM UDS configuration dumps differ only in delivered intake URLs, participant shared paths,
existing type port/GUI defaults, baseline ADP disablement and the no-host-view copy's host roots. The OTTL collector
settings differ only in the exporter URL; its listener port is derived from the placed file rather than a separate
endpoint declaration. The standalone ADP source dump drops the unused file-only APM URL; the Agent receives it
through its intake connection.

APM receiver ownership matches legacy: TCP uses the baseline trace-agent on 8126 and ADP on 8126 proxying to the
subject trace-agent on 8127. UDS uses the baseline trace-agent's shared socket and ADP's subject shared socket,
proxying to `/var/run/datadog/apm.socket`; ADP's TCP receiver remains disabled. The native APM port default initially
masked the case's ADP-only port `0` through the Agent config stream, causing both processes to bind 58126.
The participant now omits that injected default whenever the case supplies `ADP_DD_APM_RECEIVER_PORT`.
Explicit case environment values remain unchanged; a unit test guards the default-selection rule.

### Linux comparison evidence

Compared with `778c155cee` on rootless lima, using the same `testing-devel` image (`d1b1efc01354`) and tools image
(`c6b554d2d05b`). Runner-reported elapsed times include cleanup: `dsd-plain` 67.990 s legacy → 42.317 s native;
`dsd-origin-detection` 63.971 s legacy → 49.888 s for a scratch native run without host view. The checked-in origin
case keeps host view: Docker file placement rejects its read-only `/host/proc` mount with `remount-ro`, flags
`0x5021`, `operation not permitted`. Both failed and successful runs reclaimed all registered resources.

All four captures per case (legacy/native × baseline/subject), after the analyzer's telemetry filter, have the same
3,028 metric contexts and tags. None has container/image origin tags: legacy already reports the Docker socket
unreachable and disables its Docker metadata collector. This establishes workload/tag preservation, not successful
container enrichment. Both native senders and the legacy baseline have 10,000 payloads, 81,111,860 payload bytes,
and the same framed-payload FNV-1a digest `2f49534daf07d378`.

Agent configuration dumps differ in four baseline leaves and fourteen subject leaves: delivered intake/socket
addresses, explicit ADP disablement on the baseline, and existing native listener defaults on the subject. ADP runtime
dumps differ in six leaves for plain: intake URL, socket path, command port 5001→55001, API ports 5100/5101→55100/55101,
and APM port 8126→58126. The scratch origin run adds two host-root differences because host view was disabled.
After accounting for those addresses, listener defaults and scratch host roots, no runtime difference remains;
hostname, DogStatsD port 0, worker count 1, origin detection mode and workload are preserved.


Native runs use fresh participant directories and reclaim them after each case.

### Required origin tags

The metrics compare option `required_metric_tag_keys` requires a workload context on each side with nonempty
values for all listed keys. It runs after internal-telemetry filtering, never combines tags across contexts, and
writes both decoded captures before checking. An empty list preserves ordinary comparison. Unit tests exercise
both enriched sides, neither side, either side alone, empty values, telemetry-only tags and tags split across
contexts, plus malformed options and non-metrics rejection.

`dsd-origin-detection` requires `image_name`, a low-cardinality tag supplied by container discovery and absent
from its generated payloads. The production host mounts remain enabled. A rootless-lima scratch copy differing
only by omission of the two `host_view` parameters failed with exit 1: both sides lacked this tag on all 3,028
workload contexts. Case elapsed was 42.786 s (43.278 s wrapper wall). Both decoded captures and the input payload
were retained, every reported collection path existed, and all five observed containers were reclaimed. This is a
negative assertion proof, not proof of host view or successful enrichment; the rootful/CI proof remains pending.

The other two host-view cases, `dsd-tag-filterlist` and `dsd-metric-filterlist-prefix`, require the same tag. Their
scratch copies also differ only by omission of the two `host_view` parameters. Both fail for missing `image_name`
on both sides, retaining all captures with no collection issues or stale reported paths. The tag-filterlist run
has 178 workload contexts per side and takes 48.708 s case elapsed (49.246 s wall); metric-filterlist-prefix has
176 per side and takes 48.694 s (49.231 s wall). All ten observed containers were reclaimed. Their include/drop
policies can remove origin tags from some contexts, so the assertion requires one fully tagged workload context
per side rather than requiring tags on every context.
