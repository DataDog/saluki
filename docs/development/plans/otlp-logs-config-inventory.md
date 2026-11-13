# OTLP logs configuration inventory plan

This plan covers the configuration parity work of the OTLP logs parity effort
([#2966](https://github.com/DataDog/saluki/issues/2966)), as listed in
[#2968](https://github.com/DataDog/saluki/issues/2968):

- `logs-enabled-config`: a typed `logs_enabled` setting
- `native-logs-pipeline`: a classifier pipeline for native OTLP logs
- `logs-config-inventory`: classify the keys that affect the OTLP logs path
- `logs-config-startup-gate`: decide which unsupported keys stop ADP from starting

It also states what it needs from `otlp-signal-gating` and `proxy-signal-flags`
([#2969](https://github.com/DataDog/saluki/issues/2969)). The background, including the list of keys that affect the
Agent's OTLP logs path, is on the Confluence [Analysis](https://datadoghq.atlassian.net/wiki/x/2oLeuAE) page.

Update this file as each pull request lands, and delete it in the last one, after moving the matrix rationale into the
overlay's `documentation` fields.

## Where things stand

As of `main` at `5aefcbd0ca` (2026-11-13):

- The overlay (`lib/datadog-agent/config/schema/schema_overlay.yaml`) has the pipeline tokens `dogstatsd`, `checks`,
  `otlp`, `traces`, and `cross_cutting`. Adding one touches the overlay model (`config-overlay-model/src/lib.rs`: the
  `Pipeline` enum and its token parser), the classifier code generation (`config/build/classifier_gen.rs`), and the
  runtime `Pipeline` enum (`config/src/classifier/mod.rs`). The generated documentation doesn't show pipelines.
- `DataPlaneConfiguration::active_pipelines` (`bin/agent-data-plane/src/config.rs`) feeds
  `ConfigurationSystem::check_compatibility` (`agent-data-plane-config-system/src/compatibility.rs`), which runs once,
  at startup, in `cli/run.rs`. It skips keys that no input set explicitly. Runtime updates aren't checked.
- Native OTLP logs are wired (`otlp_in.logs -> dd_logs_encode`, in `add_otlp_pipeline_to_blueprint`) whenever OTLP is
  enabled and `data_plane.otlp.proxy.enabled` is false. Neither `logs_enabled` nor `otlp_config.logs.enabled` gates
  this (`gap/signal-gating`). `logs_pipeline_required` repeats the same condition inline, next to the checks
  condition.
- All 125 `logs_config.*` leaves, `logs_enabled`, `log_enabled`, `otlp_config.logs.batch.*`, and
  `otlp_config.logs.infra_attributes.tags_as_ddtags` are `excluded`, most of them as "ignored in initial audit".
- Some inventoried keys are classified more favorably than ADP's logs behavior supports:
  - `otlp_config.logs.enabled` is `full`, but isn't honored per signal.
  - `data_plane.otlp.proxy.logs.enabled` and `data_plane.otlp.proxy.metrics.enabled` are `full`, but proxy mode
    ignores both and always relays logs and metrics (`gap/proxy-signal-flags`).
  - `additional_endpoints` and `dd_url` are `full` and `cross_cutting`, but ADP sends logs to the global additional
    endpoints and derives the logs host from `dd_url`, which the Agent never does for logs (`gap/logs-endpoints`).
- The gate can only classify what reaches the merged sources, and two gaps keep unsupported keys out of them:
  - **Environment variables.** The generated environment table covers only `full` and `partial` keys
    (`config/build/env_reader_gen.rs`), so `DD_LOGS_CONFIG_PROCESSING_RULES` or `DD_LOGS_CONFIG_LOGS_DD_URL` is never
    read. In connected mode the Agent streams these keys, so the gate sees them. In standalone mode it doesn't. This
    affects every unsupported key, not just logs keys, and is out of scope for this plan: in standalone mode, the
    gate catches unsupported logs keys only when they're set in `datadog.yaml`.
  - **Object-typed settings.** `SourceTree::flatten` (`agent-data-plane-config-system/src/source.rs`) emits an
    object's own key only for modeled leaves, and otherwise emits its children. An unmodeled object setting such as
    `logs_config.tag_filters` (`{exclude: [...]}`) reaches the classifier as `logs_config.tag_filters.exclude`, which
    matches no entry. Array settings such as `processing_rules` aren't affected.
- Several open pull requests change the logs wiring, for example the render step in
  [#3102](https://github.com/DataDog/saluki/pull/3102). Whichever lands later must use the shared predicate below.

### Upstream proxy key rename

Agent `main` renamed the proxy keys in DataDog/datadog-agent#59825 (merged 2026-11-10). `data_plane.otlp.proxy.*`
is gone, with no aliases. In its place are three flat flags, `data_plane.experimental.otlp.proxy.{logs,metrics,traces}`,
that default to `false`, and there's no master switch. The vendored schema (`a682da124f`) predates this. Once it's
updated, logs are proxied only when `data_plane.experimental.otlp.proxy.logs` is set.

This plan doesn't depend on that update. The shared predicate reads the typed model
(`config.domains.otlp.proxy.*`), not key names, so the update only changes the translation feeding it. The
reclassification of the old proxy keys is cheap, so do it anyway, and drop it if the schema update lands first. See
the open questions.

## Approach

The overlay holds one classification per key, and pipeline affinity decides when the startup gate checks the key.
Three rules follow:

1. **A logs-only key gets the `logs` pipeline, and no other.** Then the gate checks it only when ADP actually handles
   logs, never in proxy mode, where the core Agent processes logs.
2. **A key that gates the OTLP receiver keeps the `otlp` pipeline.** ADP applies OTLP gating itself in proxy mode too
   (see the proxy-mode gating decision in #2966), so `logs_enabled`, `log_enabled`, and `otlp_config.logs.enabled`
   matter in both modes.
3. **A key shared with other signals keeps its broad affinity.** Its logs difference goes into the documentation of a
   `partial` entry. One classification can't say "full for metrics, but unsupported for logs", and a warning or
   startup failure for every user of `additional_endpoints` would be wrong for metrics-only users. Leaving it at that
   would be a bypass, though: with native logs, a global `additional_endpoints` or `dd_url` sends logs somewhere the
   Agent wouldn't, which is exactly what PR 4 wants to catch. PR 4 therefore adds a logs-scoped severity for these
   keys (see there).

Severities are provisional until `logs-config-startup-gate` (PR 4). Until then, no logs key is `high`, so no logs
key can stop a deployment from starting. PR 1 has its own, narrower, startup effect, described there.

## Pull requests

### 1. `logs-enabled-config`: the `logs_enabled` setting

`otlp-signal-gating` copies the Agent's check that disables the whole OTLP receiver when OTLP logs are enabled
without the logs agent ("OTLP logs is enabled but logs agent is not enabled", `comp/otelcol/otlp/pipeline_validator.go`).
That needs a typed `logs_enabled`, so this PR comes first. Nothing reads the field yet, so the topology doesn't
change. Validation does, though (see the last bullet).

- Move `logs_enabled` and `log_enabled` from `excluded` to `inventory` as `partial`, `pipelines: [otlp]`, `warn:
  false`. The documentation says ADP reads them but doesn't gate on them yet (`gap/logs-enabled`). `otlp-signal-gating`
  moves them to `full`. They're `otlp` rather than `logs` because ADP applies the gating in proxy mode too (rule 2). Unlike
  `otlp_config.logs.enabled` in PR 3, they don't warn: many Agent users set `logs_enabled` for their own log
  collection, and ADP diverges only when it's false while OTLP logs are enabled.
- Translate both into one field, `control.logs_enabled`, next to the other pipeline switches in
  `agent-data-plane-config/src/control.rs`. Only the topology reads it, through `DataPlaneConfiguration`.
- `log_enabled` isn't a former name: the schema declares it as a separate setting, not under `renamed_from` of
  `logs_enabled`. The Agent ORs the two everywhere it reads them (`comp/logs/agent/impl/agent.go:159`,
  `comp/otelcol/logsagentpipeline/impl/agent.go:78`) and warns when `log_enabled` is true. So the witness ORs the two
  values: each generated `consume_` method ORs its value into the field (`|=`), which works because every translation
  starts from a fresh `DatadogTranslator`. Don't use the `renamed_from` mechanism, which makes the former name win
  when both are set. Replicate the warning as well.
- **Malformed values become fatal.** Once a key is modeled, a value that the Agent's casting can't convert is a hard
  error (a deliberate divergence, `datadog-agent/config/src/cast_de.rs`), where the Agent reads it as `false`. Today
  `logs_enabled: "yes"` passes through as an excluded key; after this PR, ADP refuses to start with it. The
  `logs-enabled-yes-yaml` recorder case already covers this value, so `corpus_replay` will flag it. `logs_enabled` is
  set in many Agent configurations, so this is more exposed than most newly modeled keys. The suggestion is to accept
  it, consistent with every other modeled key, update the replay expectation deliberately, and mention it in the
  release note. The alternative is a per-key exception to the casting rule, which this plan doesn't recommend.
- Tests: translator tests for each key alone, both set, either one false while the other is true, the defaults, and a
  malformed value, each through YAML and the environment (`DD_LOGS_ENABLED`, `DD_LOG_ENABLED`). Run `corpus_replay`,
  since both keys change support. No new recorder cases are needed: neither key has former names.

### 2. `native-logs-pipeline`: the `logs` pipeline

- Add a `logs` token to the overlay model, the classifier code generation, and the runtime `Pipeline` enum, with an
  overlay model test that parses it.
- Add `DataPlaneConfiguration::otlp_logs_native()`, the single predicate for whether ADP handles OTLP logs itself.
  Today it's `otlp_enabled() && !otlp_proxy_enabled()`, which is exactly what the topology wires now. Use it in:
  - `logs_pipeline_required` (`checks_enabled() || otlp_logs_native()`)
  - `add_otlp_pipeline_to_blueprint`, for the decision to connect `otlp_in.logs`
  - `active_pipelines`, which inserts `Pipeline::Logs`
- Document the token in the overlay header and `docs/development/configuration-system.md`.
- Tests: `active_pipelines` tests for native mode, proxy mode, OTLP disabled, and checks only (no `logs`). The
  per-signal proxy case is added by `proxy-signal-flags`. A test asserting that `Pipeline::Logs` is active exactly when
  `otlp_in.logs` is connected, over the same matrix, so wiring and classifier can't drift.

The issue gives the predicate as "OTLP enabled, logs enabled, and proxy mode off". That holds only once
`otlp-signal-gating` makes the wiring honor `logs_enabled` and `otlp_config.logs.enabled`. Adding those conditions to
the classifier before the wiring honors them would bring back the drift this predicate exists to prevent, so this PR
leaves them out even though `control.logs_enabled` exists by then. `otlp-signal-gating`
adds the two conditions to `otlp_logs_native()`, and `proxy-signal-flags` replaces `!otlp_proxy_enabled()` with the
per-signal check. Both update the parity test.

This PR adds no gate test. Every compatibility test uses real overlay keys, and before PR 3 no key has `logs`
affinity, so there's nothing to check. The gate test from the issue moves to PR 4, which adds the first `high` logs
keys.

### 3. `logs-config-inventory`: classify the keys

Deliverable: a key-by-key matrix in this file (section [Key matrix](#key-matrix)), and the overlay changes it
justifies.

Steps:

1. List every candidate key: the 125 `logs_config.*` leaves, `otlp_config.logs.*`,
   and the keys outside `logs_config` named in the Analysis page (`multi_region_failover.*`,
   `observability_pipelines_worker.logs.*`, `vector.logs.*`, the HTTP transport keys, secret refresh,
   `delegated_auth.*`, the hostname keys, `otlp_config.debug.verbosity`, `provider_kind`, the proxy flags). Start from
   the Analysis list. For each key, confirm against the Agent source whether it affects the OTLP logs path, and record
   the source location. A key that matters only by failing validation (for example `fingerprint_config.*`, which can
   stop the logs agent and with it the whole OTLP pipeline) counts as affecting the path.
2. Fill in, for each key: whether it affects the OTLP logs path, the target classification, the pipeline, the
   `planned` flag, the issue, and the gap ID.
3. Apply the matrix to the overlay:
   - **Logs path keys** move to `inventory`, mostly as `none` with `pipelines: [logs]`, a provisional `low` or
     `medium` severity, and `planned: true` with the sub-issue that implements them. The groups:

     | Group | Keys (all under `logs_config` unless stated) | Issue |
     |---|---|---|
     | Endpoints, auth, and compression | `logs_dd_url`, `logs_no_ssl`, `dev_mode_no_ssl`, `dd_url`, `api_key`, `additional_endpoints`, `use_v2_api`, `use_compression`, `compression_kind`, `compression_level`, `zstd_compression_level`, `delegated_auth.*` | #2973 |
     | Sender and batching | `batch_wait`, `batch_max_size`, `batch_max_content_size`, `batch_max_concurrent_send`, `disable_distributed_senders`, `http_timeout`, `http_protocol`, `connection_reset_interval`, `sender_backoff_*`, `sender_recovery_*`; also `otlp_config.logs.batch.*` | #2973, #2974 |
     | Transport selection | `use_http`, `force_use_http`, `use_tcp`, `force_use_tcp`, `socks5_proxy_address`, `http_connectivity_retry_interval_max`, and the TCP-only `dd_port`, `dd_url_443`, `use_port_443`, `dev_mode_use_proto` | #2976 |
     | Pipeline topology and shutdown | `pipelines`, `pipeline_failover.*`, `message_channel_size`, `payload_channel_size`, `stop_grace_period` | the matrix decides: `planned: false`, or #2974 |
     | Processing | `processing_rules`, `tag_filters` (the logs processor applies it to every message, OTLP included), `fingerprint_config.*` (validation only) | #2975 for `processing_rules`; the matrix decides the others |
     | Enrichment | `otlp_config.logs.infra_attributes.tags_as_ddtags` | #2972 |
     | MRF and OPW | `multi_region_failover.failover_logs`, `multi_region_failover.logs_service_allowlist`, `observability_pipelines_worker.logs.*`, `vector.logs.*` | #2973 |

     The transport group is `planned: true` because `tcp-transport-investigation` investigates support, which the
     configuration guide counts as planned. Some of these keys get a typed field in later tasks, and those tasks move
     them to `full` or `partial` with their witness methods.
   - **Gating keys**: `otlp_config.logs.enabled` becomes `partial` with `pipelines: [otlp]`, `warn: true`, and
     documentation of `gap/signal-gating`, until `otlp-signal-gating` lands. PR 1 has already classified `logs_enabled`
     and `log_enabled`.
   - **Proxy flags**: `data_plane.otlp.proxy.logs.enabled` and `data_plane.otlp.proxy.metrics.enabled` become `partial`
     with `warn: true`, documenting that proxy mode always relays both signals, until `proxy-signal-flags`. Skip this
     if the schema update has already removed them.
   - **Shared keys**: `additional_endpoints` and `dd_url` become `partial`, `warn: false`, keeping `cross_cutting`, and
     document the logs routing difference (`gap/logs-endpoints`). Review the other shared keys already in the
     inventory for logs affinity, and change them only where the matrix shows a difference: `api_key`, `site`,
     `min_tls_version`, `skip_ssl_validation`, `proxy.*`, `no_proxy_nonexact_match`, `sslkeylogfile`,
     `tls_handshake_timeout`, `multi_region_failover.enabled`, `hostname`, and `provider_kind` (`[dogstatsd, otlp]`
     today).
   - **Other logs keys** stay `excluded`, with a dated reason that names the feature, for example
     `"2026-11-20: ADP does not currently support container log collection."`. Group them by feature: file tailing,
     container and Kubernetes log collection, the auditor and registry, multi-line detection, the decoder (including
     `max_message_size_bytes`, `tag_truncated_logs`, adaptive sampling, and noisy log detection), integration logs, and
     log streaming. Check each logs processor setting against `comp/logs-library/processor/processor.go` before
     excluding it, since the processor runs for OTLP logs too. Excluded `hostname_*`, `secret_refresh_*`, `delegated_auth.*`, and `http_dial_fallback_delay` that
     aren't logs-specific keep their current status. If the matrix shows that they affect logs differently from the
     other signals, record that as a follow-up instead of widening this PR.
4. Make `SourceTree::flatten` emit an object setting's own key whenever the schema marks it as a leaf, not only when
   it's modeled, so that `tag_filters` reaches the classifier as one key (see
   [Where things stand](#where-things-stand)). This is the only code change in this PR. It also applies to the one
   object-typed key already classified, `use_v3_api.distributions.endpoints` (`medium`), which starts to warn when
   set.
5. Run `make build-schema-overlay` and review the generated compatibility documentation, which gains about 50 entries
   under "Being Worked On".

Tests:

- Overlay validation and code generation (`make build-schema-overlay`, `make check-all`).
- Classifier tests for one representative key of each kind: `none` with `logs` affinity, the `partial` gating key with
  `otlp` affinity, a shared `partial` key with `warn: false`, which yields no entry, and an excluded key, which yields
  none either.
- A `flattened_keys` test for an unmodeled object setting, and a compatibility test that classifies `tag_filters`
  set through the file and through the configuration stream.
- `cargo nextest run --lib -p agent-data-plane-config-system corpus_replay`, because support changes for
  `otlp_config.logs.enabled` and the proxy flags affect replay coverage.

### 4. `logs-config-startup-gate`: severities

This PR depends on PR 3 and on `otlp-signal-gating`. Before signal gating, ADP handles logs natively whenever OTLP is
native, so the `logs` pipeline is active for OTLP-metrics-only users. A `high` logs key, for example a
`processing_rules` written for the Agent's file tailing, would then stop ADP for users who send it no logs.

Steps:

1. **What happens to OTLP traffic when ADP refuses to start.** From reading the Agent code (not yet tested): with
   `data_plane.enabled` and `data_plane.otlp.enabled`, the Agent moves its own OTLP listeners off
   `otlp_config.receiver` (`relocateReceiverListeners` in `comp/otelcol/otlp/config.go`). So if ADP exits, nothing
   listens on the configured OTLP endpoints, and metrics and traces are lost along with logs. Preflight mode
   disables OTLP (`comp/dataplane/preflightmode/impl/config.go`), so it can't catch an OTLP logs incompatibility first.
   Confirm with integration cases in connected and standalone mode, recording what a client sees (connection refused
   or a hang) and whether the supervisor restarts ADP.
2. **Extend the matrix** with, for each unsupported key: the user-visible consequence when ADP ignores it, the pipeline
   affinity, whether it applies in native and in proxy mode, and the behavior in connected and in standalone mode.
3. **Derive the severities.** The tradeoff: `high` turns silent divergence into a total OTLP outage (step 1), and
   `medium` only warns. A suggested rule, for the matrix to confirm:
   - `high` where ignoring the key sends logs somewhere the user didn't intend, or bypasses a data-protection control:
     `processing_rules` (masking), `tag_filters`, `logs_dd_url`, `logs_config.api_key`,
     `logs_config.additional_endpoints`, MRF for logs, and OPW for logs.
   - `medium` where ignoring the key changes delivery or performance but not destination or content: batching,
     compression, sender tuning, and transport selection.
   - `low` for keys that only affect internal sizing.
4. **Shared keys with a logs difference** (`additional_endpoints`, `dd_url`, and any others the matrix finds). Their
   overlay entries stay `partial` for all pipelines, so the overlay needs a way to attach a severity that applies only
   when the `logs` pipeline is active. A suggested shape is an optional `pipeline_severity: { logs: high }` on
   `partial` entries, emitted by the classifier code generation and checked by `check_compatibility` against the
   active pipelines. The alternative is to accept the bypass until `logs-routing-isolation` lands and document it.
   This is a judgment call for the matrix: the rule in step 3 argues for `high`.
5. Record the severities in the overlay, with each key's documentation stating the consequence, and the matrix as the
   rationale.

Tests:

- Compatibility tests in `compatibility.rs`: a `high` logs key fails the check with `Pipeline::Logs` active, and passes
  with only `Pipeline::Otlp` active (proxy mode).
- If step 4 adds a logs-scoped severity: `additional_endpoints` fails the check with `Pipeline::Logs` active and passes
  with only `Pipeline::DogStatsD`.
- An end-to-end test in `bin/agent-data-plane` that builds `active_pipelines` from native and proxy configurations
  and runs the gate on the same sources.
- Integration cases modeled on `adp-config-check-exit` and `adp-config-check-warn`: a `high` logs key with native OTLP
  logs stops ADP with the error; a `medium` key starts with a warning; and the same `high` key in proxy mode doesn't
  block startup. Run the `high` case in connected mode with the key in the environment, and in
  standalone mode with the key in `datadog.yaml`, since standalone mode doesn't read unsupported keys from the
  environment. Because the cases name specific keys, they need the same "replace this key when it becomes
  supported" note as the existing ones.

## Ordering

```text
PR 1 ──> otlp-signal-gating (#2969) ──┐
  └────> PR 2 ──> PR 3 ───────────────┴──> PR 4
```

- **PR 1 comes before `otlp-signal-gating`**, which needs the typed `logs_enabled`.
- **PR 2 and `otlp-signal-gating` can land in either order.** Landing signal gating first lets PR 2 write the final
  predicate at once, and shortens the time `otlp_config.logs.enabled` warns as `partial`. It isn't needed for
  correctness, so PR 2 shouldn't wait for it.
- **PR 3 needs PR 2** for the `logs` token.
- **PR 4 waits for PR 3 and `otlp-signal-gating`.**

PR 3 is the largest: a matrix of about 130 keys and about 50 new overlay entries. Apart from the small `flatten` change, it changes
only the overlay and the generated files, so it stays reviewable as one PR. If it isn't, split off the corrections to keys already in the
inventory (`otlp_config.logs.enabled`, the proxy flags, `additional_endpoints`, and `dd_url`). They don't need the
`logs` token, so they could land before PR 2.

## Key matrix

Filled in by PR 3 and extended by PR 4. One row per key outside the "Other logs keys" groups, which get one row per
group.

| Key | Affects OTLP logs | Agent source | Classification | Pipelines | Planned, issue | Gap | Consequence if ignored | Native / proxy | Connected / standalone | Severity |
|---|---|---|---|---|---|---|---|---|---|---|

## Open questions

- **Upstream proxy rename.** Agent #59825 removed `data_plane.otlp.proxy.*` and made the per-signal flags default to
  `false`. When the vendored schema follows, the meaning of "proxy mode stays the default" for logs changes. Does that
  update belong to `proxy-signal-flags`, or to a separate schema update before it?
- **Check logs.** In the Agent, check logs also go through the logs agent, so `logs_config` endpoints and
  `processing_rules` apply to them. In ADP they share `dd_logs_encode`. This plan gives logs keys only the `logs`
  pipeline, active for native OTLP logs, keeping check logs out of scope as #2966 decides. Should the matrix still
  record a check logs column for the separate check logs effort?
- **Runtime updates.** The gate runs only at startup. A `high` logs key added later through the configuration stream
  is never reported. Is that acceptable for this effort, or should `logs-config-startup-gate` at least warn on
  updates?
