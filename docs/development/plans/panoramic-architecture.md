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
- **Runtime:** the loader's runtime identifier (`mac` means macOS; correctness uses `docker` or `kubernetes_in_docker`).
- **Status:** selected by CI discovery, or disabled: the file sets `disabled: true` and lives in
  `test/correctness/disabled/`, which CI does not list. The inventory requires each case's directory to match
  its suite and status.
- **CI job:** the job that selects this row. Correctness jobs belong to the `run-correctness-tests` child pipeline.
  Disabled rows have no job. macOS rows list both architectures.
- **Schema:** the file's `type` value, or `participants` for a file that declares participants.
- **Execution:** `old` identifies the integration and correctness runners. A participant case is translated to an
  integration case and runs on the integration runner, so it is `old` too.
- **Steps:** assertions and actions, including assertions inside parallel blocks, or the correctness comparison mode.
- **Ignored:** number of key paths the loader drops without error. The probe loads each modified file through the
  case loader and, for a participant case, the translation. Replacing each value with either a map or a string must
  leave the recorded configuration unchanged. Paths are listed in the JSON.

The [JSON inventory](./panoramic-architecture-inventory.json) also records source paths, timeouts, images before CLI
image overrides, correctness CI mixins, and a `cases` map keyed by `<suite>/<name>`. Each case contains the effective
configuration, integration step counts by kind or correctness comparison options, and ignored key paths. Integration
OS rows share one case entry; matrix variants have separate entries. Totals count files once and steps per selected
OS row; disabled rows are separate from selected totals.

## Regenerating

```bash
PANORAMIC_WRITE_COVERAGE_INVENTORY=1 cargo nextest run -p panoramic coverage_inventory
```

Review the diff. Update the inventory whenever a case file, its platforms or its CI job changes.
Without the environment variable, the test compares the generated JSON and coverage tables with the checked-in
versions and reports added or removed row keys and the first differing line in each file. The other inventory tests
check file accounting, case placement by suite and status, case-name uniqueness across both suites, CI job and mixin
definitions, the correctness generator's fail-fast shell options, ignored-key detection, logging-case translation
equivalence, and migration-proof references.

## Coverage details

- There are 52 integration files and 42 correctness files, two of them under `test/correctness/disabled/`.
  Integration selects 45 Linux, 31 macOS and 34 Windows rows. Correctness selects 46 Linux rows: 39 Docker and seven
  Kubernetes-in-Docker variants. Two correctness cases are disabled (`otlp-traces` and `otlp-traces-ets`) and live in
  `test/correctness/disabled/`. The inventory has 158 rows.
- Correctness discovery ignores the integration runtime flag. CI runs all selected correctness cases on Linux.
  The seven variants come from `dsd-origin-detection-matrix`: `legacy`, `legacy-high-cardinality`,
  `legacy-optout-disabled`, `legacy-with-client-fields`, `unified`, `unified-high-cardinality` and
  `unified-with-client-fields`. File counts are not runnable counts.
- Integration has 25 cases on all three OSes, five on Linux/macOS, three on Linux/Windows, 12 Linux-only, one
  macOS-only and six Windows-only. Every integration `name` equals its directory name. The 52 files contain 218
  assertions, 23 actions and 42 parallel blocks.
- Every correctness baseline and comparison declares `saluki-images/datadog-agent:testing-release`. Every selected
  correctness job extends `.test-correctness-adp-baseline`; CI supplies the same `bundled-agent-adp` image to both sides.
- Integration uses `saluki-images/datadog-agent:testing-devel` on Linux and
  `saluki-images/agent-data-plane:testing-windows` on Windows (overridden in CI by `--image-override container=$ImageTag`).
  macOS uses host processes. The intake uses `saluki-images/correctness-tools:latest` and is enabled only in
  `dogstatsd-host-tag` and `dogstatsd-intake-metric`.
- Both `dsd-forwarding` and `dsd-forwarding-burst` require captured forwarded DogStatsD packets.
- `privileged-api-endpoints` declares five `output_contains` keys on `adp_cli` actions that the loader ignores:
  `procedure[6].output_contains`, `procedure[7].output_contains`, `procedure[8].output_contains`,
  `procedure[9].output_contains` and `procedure[10].output_contains`. These keys appear in all three OS rows.
- `datadog_intake.config_path` is ignored in `otlp-traces`, `otlp-traces-ets`, `otlp-traces-probabilistic` and
  `otlp-traces-probabilistic-full-trace-id`, including the two enabled probabilistic cases. The `apm-trace` cases set
  `millstone.config_path`, which the loader consumes.

## Effective configuration scope

The JSON records the loader's configuration with defaults and matrix environments applied, before CLI image
overrides. For a participant case, the generator translates the integration configuration for each subject OS,
requires the translations to be identical, and records one. Fact references in step paths are resolved; other paths
and template values remain as configured. Use product `config_dump` output from CI runs to compare effective product
configurations.

<!-- coverage-inventory:begin -->

## Coverage totals

| Suite | Files | Selected rows (subject OS / runtime) | Disabled rows | Assertions (subject OS) | Actions (subject OS) |
| --- | --- | --- | --- | --- | --- |
| integration | 52 | linux / linux: 45; macos / mac: 31; windows / windows: 34 | 0 | linux: 191; macos: 130; windows: 144 | linux: 20; macos: 14; windows: 16 |
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
| adp-ipc-no-dns | integration | linux | linux | selected | test-integration | integration | old | assertions: 4, actions: 0 | 0 |
| adp-logging-default-path | integration | linux | linux | selected | test-integration | participants | old | assertions: 2, actions: 0 | 0 |
| adp-logging-ignores-core-agent-log-file | integration | linux | linux | selected | test-integration | participants | old | assertions: 3, actions: 0 | 0 |
| adp-logging-mac-ignores-core-agent-log-file | integration | macos | mac | selected | test-integration-macos-arm64, test-integration-macos-amd64 | participants | old | assertions: 3, actions: 0 | 0 |
| adp-logging-respects-data-plane-log-file | integration | linux | linux | selected | test-integration | participants | old | assertions: 2, actions: 0 | 0 |
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
| adp-rc-core-disabled | integration | linux | linux | selected | test-integration | integration | old | assertions: 5, actions: 0 | 0 |
| adp-rc-no-products | integration | linux | linux | selected | test-integration | integration | old | assertions: 4, actions: 0 | 0 |
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
| dogstatsd-forwarding | integration | linux | linux | selected | test-integration | integration | old | assertions: 12, actions: 0 | 0 |
| dogstatsd-host-tag | integration | linux | linux | selected | test-integration | integration | old | assertions: 6, actions: 1 | 0 |
| dogstatsd-intake-metric | integration | linux | linux | selected | test-integration | integration | old | assertions: 4, actions: 2 | 0 |
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
| dogstatsd-top | integration | linux | linux | selected | test-integration | integration | old | assertions: 5, actions: 1 | 0 |
| dogstatsd-top-windows | integration | windows | windows | selected | test-integration-windows-amd64 | integration | old | assertions: 5, actions: 1 | 0 |
| liveness-container-tags | integration | linux | linux | selected | test-integration | integration | old | assertions: 4, actions: 1 | 0 |
| otlp-tls | integration | linux | linux | selected | test-integration | integration | old | assertions: 3, actions: 1 | 0 |
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

A participant case runs on the integration runner only if it has one `agent-adp` participant configured by
`config.env`, with `log_contains` and `log_not_contains` (`process` omitted), translated to `stream: both`, and
`file_contains`. File paths may reference the participant's facts, resolved per subject OS. Log steps search captured
output: the container's log stream (Agent and ADP under s6) on Linux, stdout and stderr of the spawned Agent and ADP
processes on macOS, and the container's console output only on Windows.

Participant-schema twins of the seven logging cases are built from a frozen copy of their inventory entries taken
before their schema changed (`bin/correctness/panoramic/src/config/coverage_inventory/logging_baseline.json`). All
seven logging cases use the participant schema. Both the twins and this inventory's entries must match that copy,
apart from `schema` and `container.exposed_ports: []`. Edit the copy by hand when the inventory format changes;
never regenerate it. In the integration schema the logging files declare `58125/udp`, which their participant-schema
versions drop; only `port_listening` reads a published port's mapping. On Linux an empty list also stops Docker
publishing the image's exposed ports; macOS host processes have no container, so there the list only feeds
`port_listening`. Windows containers never publish exposed ports, so there an empty list only stops the container
declaring the port. The translation uses default container settings, so changing these files' schema changes only
the inventory's `schema` column and `container.exposed_ports: []`. List `subject_os` in `runtimes` order to preserve
runtime order.

`adp-disabled-exit` and `adp-no-pipelines-exit` also fit the supported subset, but their log line stands in for a
process exit. They keep the old schema until exits are observed directly.

## Migration proofs

When changing a case's schema or runner, add CI evidence per (case, subject OS) and reference the inventory commit
used for comparison. Each proof names the tested commit, links the passing CI jobs and identifies the runner and
execution value.

<!-- migration-proofs:begin -->
| Case | Subject OS | Proof |
| --- | --- | --- |
| adp-logging-default-path | linux | `33e3e697c5`: [test-integration](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351927); passed on the integration runner (execution `old`). |
| adp-logging-ignores-core-agent-log-file | linux | `33e3e697c5`: [test-integration](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351927); passed on the integration runner (execution `old`). |
| adp-logging-respects-data-plane-log-file | linux | `33e3e697c5`: [test-integration](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351927); passed on the integration runner (execution `old`). |
| adp-logging-respects-data-plane-log-file | macos | `33e3e697c5`: [test-integration-macos-arm64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351928), [test-integration-macos-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351929); passed on the integration runner (execution `old`). |
| adp-logging-mac-ignores-core-agent-log-file | macos | `33e3e697c5`: [test-integration-macos-arm64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351928), [test-integration-macos-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351929); passed on the integration runner (execution `old`). |
| adp-logging-windows-default-path | windows | `33e3e697c5`: [test-integration-windows-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351930); passed on the integration runner (execution `old`). |
| adp-logging-windows-ignores-core-agent-log-file | windows | `33e3e697c5`: [test-integration-windows-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351930); passed on the integration runner (execution `old`). |
| adp-logging-windows-respects-data-plane-log-file | windows | `33e3e697c5`: [test-integration-windows-amd64](https://gitlab.ddbuild.io/DataDog/saluki/-/jobs/2199351930); passed on the integration runner (execution `old`). |
<!-- migration-proofs:end -->
