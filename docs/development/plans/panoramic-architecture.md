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
- **Status:** selected by CI discovery, or disabled: the file sets `disabled: true` and lives in
  `test/correctness/disabled/`, which CI does not list. The inventory requires each case's directory to match
  its suite and status.
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

- There are 52 integration files and 42 correctness files, two of them under `test/correctness/disabled/`.
  Integration selects 45 Linux, 33 macOS and 36 Windows rows. Correctness selects 46 Linux rows: 39 Docker and seven
  Kubernetes-in-Docker variants. Two correctness cases are disabled (`otlp-traces` and `otlp-traces-ets`) and live in
  `test/correctness/disabled/`. The inventory has 162 rows.
- Correctness cases declare only Linux; `run` and `list` report them as skipped for other subject OSes.
  CI runs all selected correctness cases on Linux.
  The seven variants come from `dsd-origin-detection-matrix`: `legacy`, `legacy-high-cardinality`,
  `legacy-optout-disabled`, `legacy-with-client-fields`, `unified`, `unified-high-cardinality` and
  `unified-with-client-fields`. File counts are not runnable counts.
- Integration has 27 cases on all three OSes, five on Linux/macOS, three on Linux/Windows, 10 Linux-only, one
  macOS-only and six Windows-only. Every integration `name` equals its directory name. The 52 files contain 218
  assertions, 23 actions and 42 parallel blocks.
- Every correctness baseline and comparison declares `saluki-images/datadog-agent:testing-release`. The generator
  adds `.test-correctness-adp-baseline` for every case needing `agent-adp-linux-image`, including every selected
  correctness case. CI runs with `--artifacts`, binding both sides to that role with the same `bundled-agent-adp` image.
  `every_selected_correctness_case_declares_a_local_baseline_image` checks the declared baseline for the
  `saluki-images/` prefix so a published baseline cannot be silently replaced.
- Correctness baseline and comparison both bind to `agent-adp-linux-image`, matching CI's shared image. Its local
  default is `saluki-images/datadog-agent:testing-devel` (debug), while correctness files declare `:testing-release`
  (release). Artifact commands, text listings and `run --artifacts` use the role binding; without that flag,
  runners use the files' images, subject to image overrides.
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
| correctness | 42 | linux / docker: 37; linux / kubernetes_in_docker: 7; linux / linux-container: 2 | 2 | linux: 8 | linux: 0 |

Total: 162 rows.

## Coverage rows

| Case | Suite | Subject OS | Runtime | Status | CI job | Schema | Execution | Steps | Ignored |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| apm-trace | correctness | linux | docker | selected | test-correctness-apm-trace | correctness | old | compare traces | 0 |
| apm-trace-uds | correctness | linux | docker | selected | test-correctness-apm-trace-uds | correctness | old | compare traces | 0 |
| dsd-added-tags | correctness | linux | docker | selected | test-correctness-dsd-added-tags | correctness | old | compare metrics | 0 |
| dsd-events | correctness | linux | docker | selected | test-correctness-dsd-events | correctness | old | compare events | 0 |
| dsd-forwarding | correctness | linux | docker | selected | test-correctness-dsd-forwarding | correctness | old | compare metrics, forwarded packets | 0 |
| dsd-forwarding-burst | correctness | linux | docker | selected | test-correctness-dsd-forwarding-burst | correctness | old | compare metrics, forwarded packets | 0 |
| dsd-host-tag | correctness | linux | docker | selected | test-correctness-dsd-host-tag | correctness | old | compare metrics | 0 |
| dsd-mapper-blocklist | correctness | linux | docker | selected | test-correctness-dsd-mapper-blocklist | correctness | old | compare metrics | 0 |
| dsd-metric-filterlist-prefix | correctness | linux | docker | selected | test-correctness-dsd-metric-filterlist-prefix | correctness | old | compare metrics | 0 |
| dsd-origin-detection | correctness | linux | linux-container | selected | test-correctness-dsd-origin-detection | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-origin-detection-matrix/legacy | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-legacy | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/legacy-high-cardinality | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-legacy-high-cardinality | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/legacy-optout-disabled | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-legacy-optout-disabled | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/legacy-with-client-fields | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-legacy-with-client-fields | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/unified | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-unified | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/unified-high-cardinality | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-unified-high-cardinality | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/unified-with-client-fields | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-unified-with-client-fields | correctness_matrix | old | compare metrics | 0 |
| dsd-plain | correctness | linux | linux-container | selected | test-correctness-dsd-plain | participants | native | assertions: 4, actions: 0 | 0 |
| dsd-plain-v3 | correctness | linux | docker | selected | test-correctness-dsd-plain-v3 | correctness | old | compare metrics | 0 |
| dsd-provider-kind | correctness | linux | docker | selected | test-correctness-dsd-provider-kind | correctness | old | compare metrics | 0 |
| dsd-service-checks | correctness | linux | docker | selected | test-correctness-dsd-service-checks | correctness | old | compare service_checks | 0 |
| dsd-tag-filterlist | correctness | linux | docker | selected | test-correctness-dsd-tag-filterlist | correctness | old | compare metrics | 0 |
| dsd-timestamp-extension | correctness | linux | docker | selected | test-correctness-dsd-timestamp-extension | correctness | old | compare metrics | 0 |
| dsd-timestamp-extension-custom-window | correctness | linux | docker | selected | test-correctness-dsd-timestamp-extension-custom-window | correctness | old | compare metrics | 0 |
| dsd-udp | correctness | linux | docker | selected | test-correctness-dsd-udp | correctness | old | compare metrics | 0 |
| dsd-uds-stream | correctness | linux | docker | selected | test-correctness-dsd-uds-stream | correctness | old | compare metrics | 0 |
| dsd-v1-api-series | correctness | linux | docker | selected | test-correctness-dsd-v1-api-series | correctness | old | compare metrics | 0 |
| otlp-metrics | correctness | linux | docker | selected | test-correctness-otlp-metrics | correctness | old | compare metrics | 0 |
| otlp-metrics-as-type-rate | correctness | linux | docker | selected | test-correctness-otlp-metrics-as-type-rate | correctness | old | compare metrics | 0 |
| otlp-metrics-configured-tags | correctness | linux | docker | selected | test-correctness-otlp-metrics-configured-tags | correctness | old | compare metrics | 0 |
| otlp-metrics-core-resource-attributes | correctness | linux | docker | selected | test-correctness-otlp-metrics-core-resource-attributes | correctness | old | compare metrics | 0 |
| otlp-metrics-cumulative-monotonic | correctness | linux | docker | selected | test-correctness-otlp-metrics-cumulative-monotonic | correctness | old | compare metrics | 0 |
| otlp-metrics-ec2-host | correctness | linux | docker | selected | test-correctness-otlp-metrics-ec2-host | correctness | old | compare metrics | 0 |
| otlp-metrics-fargate | correctness | linux | docker | selected | test-correctness-otlp-metrics-fargate | correctness | old | compare metrics | 0 |
| otlp-metrics-gcp-host | correctness | linux | docker | selected | test-correctness-otlp-metrics-gcp-host | correctness | old | compare metrics | 0 |
| otlp-metrics-histogram-counters | correctness | linux | docker | selected | test-correctness-otlp-metrics-histogram-counters | correctness | old | compare metrics | 0 |
| otlp-metrics-histogram-nobuckets | correctness | linux | docker | selected | test-correctness-otlp-metrics-histogram-nobuckets | correctness | old | compare metrics | 0 |
| otlp-metrics-host | correctness | linux | docker | selected | test-correctness-otlp-metrics-host | correctness | old | compare metrics | 0 |
| otlp-metrics-kafka-parity | correctness | linux | docker | selected | test-correctness-otlp-metrics-kafka-parity | correctness | old | compare metrics | 0 |
| otlp-metrics-resource-attributes | correctness | linux | docker | selected | test-correctness-otlp-metrics-resource-attributes | correctness | old | compare metrics | 0 |
| otlp-metrics-scope-metadata | correctness | linux | docker | selected | test-correctness-otlp-metrics-scope-metadata | correctness | old | compare metrics | 0 |
| otlp-metrics-summary-mode | correctness | linux | docker | selected | test-correctness-otlp-metrics-summary-mode | correctness | old | compare metrics | 0 |
| otlp-traces | correctness | linux | docker | disabled | — | correctness | old | compare traces | 1 |
| otlp-traces-ets | correctness | linux | docker | disabled | — | correctness | old | compare traces | 1 |
| otlp-traces-ottl-filtering | correctness | linux | docker | selected | test-correctness-otlp-traces-ottl-filtering | correctness | old | compare traces | 0 |
| otlp-traces-ottl-transform | correctness | linux | docker | selected | test-correctness-otlp-traces-ottl-transform | correctness | old | compare traces | 0 |
| otlp-traces-probabilistic | correctness | linux | docker | selected | test-correctness-otlp-traces-probabilistic | correctness | old | compare traces | 1 |
| otlp-traces-probabilistic-full-trace-id | correctness | linux | docker | selected | test-correctness-otlp-traces-probabilistic-full-trace-id | correctness | old | compare traces | 1 |
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
<!-- migration-proofs:end -->

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
