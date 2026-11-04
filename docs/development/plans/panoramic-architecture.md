# Panoramic architecture coverage

## Purpose

This inventory records Panoramic's coverage per (case, subject OS): runtime, CI jobs, assertions and loaded
configuration. Use it to check coverage when changing case schemas or runners.

## Columns

- **Case:** the discovered name, including the variant suffix for a matrix.
- **Suite:** integration or correctness.
- **Subject OS:** Linux, macOS or Windows; independent of the host running the inventory test. A file with
  `type: integration` must declare `runtimes` explicitly because the loader's default depends on the host OS.
  A participant case must declare `subject_os`.
- **Runtime:** the typed runner's identifier (`mac` means macOS; correctness uses `docker` or `kubernetes_in_docker`),
  or the native subject participant's realization kind.
- **Status:** selected by CI discovery, or disabled: the file sets `disabled: true` and lives in
  `test/correctness/disabled/`, which CI does not list. The inventory requires each case's directory to match
  its suite and status.
- **CI job:** the job that selects this row. Correctness jobs belong to the `run-correctness-tests` child pipeline.
  Disabled rows have no job. macOS rows list both architectures.
- **Schema:** the file's `type` value, or `participants` for a file that declares participants.
- **Execution:** `old` identifies typed and bridge runners; `native` identifies the participant lifecycle.
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
definitions, the correctness generator's fail-fast shell options, ignored-key detection, logging-case translation
equivalence, and migration proofs: their rows, CI links and comparison commit.

## Coverage details

- There are 52 integration files and 42 correctness files, two of them under `test/correctness/disabled/`.
  Integration selects 45 Linux, 31 macOS and 34 Windows rows. Correctness selects 46 Linux rows: 39 Docker and seven
  Kubernetes-in-Docker variants. Two correctness cases are disabled (`otlp-traces` and `otlp-traces-ets`) and live in
  `test/correctness/disabled/`. The inventory has 158 rows.
- Correctness cases declare only Linux; `run` and `list` report them as skipped for other subject OSes.
  CI runs all selected correctness cases on Linux.
  The seven variants come from `dsd-origin-detection-matrix`: `legacy`, `legacy-high-cardinality`,
  `legacy-optout-disabled`, `legacy-with-client-fields`, `unified`, `unified-high-cardinality` and
  `unified-with-client-fields`. File counts are not runnable counts.
- Integration has 25 cases on all three OSes, five on Linux/macOS, three on Linux/Windows, 12 Linux-only, one
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
  `saluki-images/agent-data-plane:testing-windows` on Windows (overridden in CI by `--image-override container=$ImageTag`).
  macOS uses host processes. `dogstatsd-forwarding`, `dogstatsd-host-tag` and `dogstatsd-intake-metric` declare a
  `datadog-intake` participant using `saluki-images/correctness-tools:latest`.
- Both `dsd-forwarding` and `dsd-forwarding-burst` require captured forwarded DogStatsD packets.
- `privileged-api-endpoints` declares five `output_contains` keys on `adp_cli` actions that the loader ignores:
  `procedure[6].output_contains`, `procedure[7].output_contains`, `procedure[8].output_contains`,
  `procedure[9].output_contains` and `procedure[10].output_contains`. These keys appear in all three OS rows.
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
| integration | 52 | linux / linux: 34; linux / linux-container: 11; macos / mac: 31; windows / windows: 34 | 0 | linux: 192; macos: 130; windows: 144 | linux: 25; macos: 14; windows: 16 |
| correctness | 42 | linux / docker: 39; linux / kubernetes_in_docker: 7 | 2 | — | — |

Total: 158 rows.

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
| dsd-origin-detection | correctness | linux | docker | selected | test-correctness-dsd-origin-detection | correctness | old | compare metrics | 0 |
| dsd-origin-detection-matrix/legacy | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-legacy | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/legacy-high-cardinality | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-legacy-high-cardinality | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/legacy-optout-disabled | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-legacy-optout-disabled | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/legacy-with-client-fields | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-legacy-with-client-fields | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/unified | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-unified | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/unified-high-cardinality | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-unified-high-cardinality | correctness_matrix | old | compare metrics | 0 |
| dsd-origin-detection-matrix/unified-with-client-fields | correctness | linux | kubernetes_in_docker | selected | test-correctness-dsd-origin-detection-matrix-unified-with-client-fields | correctness_matrix | old | compare metrics | 0 |
| dsd-plain | correctness | linux | docker | selected | test-correctness-dsd-plain | correctness | old | compare metrics | 0 |
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
| adp-cmd-port | integration | linux | linux | selected | test-integration | integration | old | assertions: 6, actions: 0 | 0 |
| adp-cmd-port | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 6, actions: 0 | 0 |
| adp-cmd-port | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 6, actions: 0 | 0 |
| adp-config-check-exit | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| adp-config-check-exit | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-config-check-exit | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-config-check-no-warn | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| adp-config-check-no-warn | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-config-check-no-warn | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-config-check-warn | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| adp-config-check-warn | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-config-check-warn | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-config-dynamic-basic | integration | linux | linux | selected | test-integration | integration | old | assertions: 5, actions: 1 | 0 |
| adp-config-dynamic-basic | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 5, actions: 1 | 0 |
| adp-config-dynamic-basic | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 5, actions: 1 | 0 |
| adp-config-dynamic-runtime | integration | linux | linux | selected | test-integration | integration | old | assertions: 5, actions: 1 | 0 |
| adp-config-dynamic-runtime | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 5, actions: 1 | 0 |
| adp-config-dynamic-runtime | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 5, actions: 1 | 0 |
| adp-config-otlp | integration | linux | linux | selected | test-integration | integration | old | assertions: 26, actions: 0 | 0 |
| adp-config-otlp | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 26, actions: 0 | 0 |
| adp-config-otlp | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 26, actions: 0 | 0 |
| adp-config-stream | integration | linux | linux | selected | test-integration | integration | old | assertions: 9, actions: 0 | 0 |
| adp-config-stream | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 9, actions: 0 | 0 |
| adp-config-stream | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 9, actions: 0 | 0 |
| adp-disabled-exit | integration | linux | linux | selected | test-integration | integration | old | assertions: 1, actions: 0 | 0 |
| adp-disabled-exit | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 1, actions: 0 | 0 |
| adp-disabled-exit | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 1, actions: 0 | 0 |
| adp-ipc-no-dns | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 4, actions: 0 | 0 |
| adp-logging-default-path | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 2, actions: 0 | 0 |
| adp-logging-ignores-core-agent-log-file | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 0 | 0 |
| adp-logging-mac-ignores-core-agent-log-file | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | old | assertions: 3, actions: 0 | 0 |
| adp-logging-respects-data-plane-log-file | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 2, actions: 0 | 0 |
| adp-logging-respects-data-plane-log-file | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | old | assertions: 2, actions: 0 | 0 |
| adp-logging-windows-default-path | integration | windows | windows | selected | test-integration-windows-amd64 | participants | old | assertions: 2, actions: 0 | 0 |
| adp-logging-windows-ignores-core-agent-log-file | integration | windows | windows | selected | test-integration-windows-amd64 | participants | old | assertions: 3, actions: 0 | 0 |
| adp-logging-windows-respects-data-plane-log-file | integration | windows | windows | selected | test-integration-windows-amd64 | participants | old | assertions: 2, actions: 0 | 0 |
| adp-memory-mode-disabled | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-disabled | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-disabled | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-exceeds-limit | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-exceeds-limit | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-exceeds-limit | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-within-limit | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-within-limit | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-permissive-within-limit | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-strict-exceeds-limit | integration | linux | linux | selected | test-integration | integration | old | assertions: 1, actions: 0 | 0 |
| adp-memory-mode-strict-exceeds-limit | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 1, actions: 0 | 0 |
| adp-memory-mode-strict-exceeds-limit | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 1, actions: 0 | 0 |
| adp-memory-mode-strict-within-limit | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-strict-within-limit | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-memory-mode-strict-within-limit | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| adp-no-pipelines-exit | integration | linux | linux | selected | test-integration | integration | old | assertions: 1, actions: 0 | 0 |
| adp-no-pipelines-exit | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 1, actions: 0 | 0 |
| adp-no-pipelines-exit | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 1, actions: 0 | 0 |
| adp-rar-disabled | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| adp-rar-registration | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 1 | 0 |
| adp-rar-registration | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 1 | 0 |
| adp-rar-registration | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 1 | 0 |
| adp-rc-core-disabled | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 5, actions: 0 | 0 |
| adp-rc-no-products | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 4, actions: 0 | 0 |
| apm-bind-custom-hostname | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| apm-bind-custom-hostname | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| basic-startup | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| basic-startup | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| basic-startup | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-autoscale-udp | integration | linux | linux | selected | test-integration | integration | old | assertions: 4, actions: 0 | 0 |
| dogstatsd-autoscale-udp | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 4, actions: 0 | 0 |
| dogstatsd-autoscale-udp | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 4, actions: 0 | 0 |
| dogstatsd-bind-custom-hostname | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-bind-custom-hostname | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-bind-host | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-bind-host | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-default-bind | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-default-bind | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-default-bind | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-enabled | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-enabled | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-enabled | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-forwarding | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 13, actions: 5 | 0 |
| dogstatsd-host-tag | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 6, actions: 1 | 0 |
| dogstatsd-intake-metric | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 4, actions: 2 | 0 |
| dogstatsd-named-pipe-windows | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 9, actions: 1 | 0 |
| dogstatsd-no-listener-continues | integration | linux | linux | selected | test-integration | integration | old | assertions: 4, actions: 0 | 0 |
| dogstatsd-no-listener-continues | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 4, actions: 0 | 0 |
| dogstatsd-non-local-overrides-bind-host | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-non-local-overrides-bind-host | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-non-local-overrides-bind-host | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-replay-unix | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 1 | 0 |
| dogstatsd-replay-unix | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 1 | 0 |
| dogstatsd-replay-windows | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 1 | 0 |
| dogstatsd-require-listener-exit | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-require-listener-exit | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 0 | 0 |
| dogstatsd-skip-failed-listener | integration | linux | linux | selected | test-integration | integration | old | assertions: 4, actions: 0 | 0 |
| dogstatsd-skip-failed-listener | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 4, actions: 0 | 0 |
| dogstatsd-top | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 5, actions: 1 | 0 |
| dogstatsd-top-windows | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 5, actions: 1 | 0 |
| liveness-container-tags | integration | linux | linux | selected | test-integration | integration | old | assertions: 4, actions: 1 | 0 |
| otlp-tls | integration | linux | linux-container | selected | test-integration | participants | native | assertions: 3, actions: 1 | 0 |
| otlp-traces-enabled | integration | linux | linux | selected | test-integration | integration | old | assertions: 4, actions: 0 | 0 |
| otlp-traces-enabled | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 4, actions: 0 | 0 |
| otlp-traces-enabled | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 4, actions: 0 | 0 |
| privileged-api-endpoints | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 10 | 5 |
| privileged-api-endpoints | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 3, actions: 10 | 5 |
| privileged-api-endpoints | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 3, actions: 10 | 5 |
| telemetry-endpoint | integration | linux | linux | selected | test-integration | integration | old | assertions: 5, actions: 0 | 0 |
| telemetry-endpoint | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 5, actions: 0 | 0 |
| telemetry-endpoint | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 5, actions: 0 | 0 |
| unprivileged-api-endpoints | integration | linux | linux | selected | test-integration | integration | old | assertions: 5, actions: 0 | 0 |
| unprivileged-api-endpoints | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | integration | old | assertions: 5, actions: 0 | 0 |
| unprivileged-api-endpoints | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 5, actions: 0 | 0 |

<!-- coverage-inventory:end -->

## Integration runner translation

The bridge runs `agent-adp` participant cases on macOS and Windows. It accepts one participant configured through
`config.env`, `log_contains` and `log_not_contains` with `process` omitted, and `file_contains`. Linux participant cases
run natively. File paths may reference participant facts. The bridge translates log steps to `stream: both`.

The seven logging cases retain a frozen integration-schema baseline in
`bin/correctness/panoramic/src/config/coverage_inventory/logging_baseline.json`. Tests compare each case's translation
on every declared subject OS against its baseline configuration, except `container.exposed_ports: []`. This comparison
also runs on Linux, independently of the selected execution route. Row comparisons allow the participant schema and,
on Linux, native execution, `linux-container` runtime and role-keyed artifacts. Never regenerate the frozen baseline.
The logging cases drop the legacy `58125/udp` exposed port, which only `port_listening` used.

`adp-disabled-exit` and `adp-no-pipelines-exit` also fit the supported subset, but their log line stands in for a
process exit. They keep the old schema until exits are observed directly.

## Native Remote Configuration cases

`adp-rc-core-disabled` and `adp-rc-no-products` use the participant schema and native Linux execution.

- Stability catches supervised ADP restarts by watching ADP's own exit record.
- Stability also covers the Agent: omitting `process` watches every declared process.
- `adp-rc-no-products` probes its `otlp_http` endpoint with `port_listening`.

Log steps omit `process` to search every process's output plus the container log.

`adp-rar-disabled` uses legacy execution and observes container liveness.

## Case migration notes

`dogstatsd-host-tag` and `dogstatsd-intake-metric` use the participant schema and native Linux execution.

- The intake is a participant connected by `connect: {intake: intake.api}`. The connection delivers `DD_DD_URL`
  and `DD_APM_DD_URL`.
- Sends reach `dogstatsd_udp` through the courier inside ADP's container instead of a published port.
- The listener gate is a port-free regex.
- Failed runs no longer capture the intake's contents under `traffic/`; participant artifact collection restores it.
- Stability watches both the ADP and Agent exit records.
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
| adp-logging-respects-data-plane-log-file | macos | old | Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration-macos-arm64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351928) and [test-integration-macos-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351929) passed on the macOS host-process runner (execution `old`). Compared with the inventory at `b975a993e0`. Caveat: the case writes to a fixed `/tmp` path that nothing deletes, so `file_contains` could match a file left by an earlier run on the same host. |
| adp-logging-mac-ignores-core-agent-log-file | macos | old | Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration-macos-arm64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351928) and [test-integration-macos-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351929) passed on the macOS host-process runner (execution `old`). Compared with the inventory at `b975a993e0`. Caveat: the case writes to fixed `/tmp` paths that nothing deletes, so `file_contains` could match a file left by an earlier run on the same host. |
| adp-logging-windows-default-path | windows | old | Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration-windows-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351930) passed on the integration runner (execution `old`). Compared with the inventory at `b975a993e0`. |
| adp-logging-windows-ignores-core-agent-log-file | windows | old | Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration-windows-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351930) passed on the integration runner (execution `old`). Compared with the inventory at `b975a993e0`. |
| adp-logging-windows-respects-data-plane-log-file | windows | old | Tested `33e3e697c5` in [pipeline 149203111](https://gitlab.ddbuild.io/DataDog/saluki/-/pipelines/149203111): [test-integration-windows-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351930) passed on the integration runner (execution `old`). Compared with the inventory at `b975a993e0`. |
| adp-rc-core-disabled | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `18554ba44c`. |
| adp-rc-no-products | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `18554ba44c`. |
| dogstatsd-forwarding | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `0a52942c17`. |
| dogstatsd-host-tag | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `0a52942c17`. |
| dogstatsd-intake-metric | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `0a52942c17`. |
| adp-ipc-no-dns | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `d1efbab740`. |
| otlp-tls | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `d1efbab740`. |
| dogstatsd-top | linux | native | Pending CI. Before: passed with execution `old` in the last green `test-integration` run before this change. Compared with the inventory at `d1efbab740`. |
<!-- migration-proofs:end -->

The macOS proofs carry a caveat: the integration runner's macOS host processes share the host's `/tmp`, and nothing
deletes the log files these cases write there. A stale file can satisfy `file_contains` until macOS cases run on a
host process backend that gives each run its own files.
