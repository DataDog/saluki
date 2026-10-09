# Subagent runtime extraction plan

This document tracks the extraction of the subagent infrastructure in `bin/agent-data-plane/src/internal/` into
`datadog-agent-runtime`. It lives next to the code so that the remaining work stays visible after the extraction
stack merges. Each pull request in the stack updates this file in the same commit as its code.

## Goal

Agent Check Runner (ACR) runs as a subagent of the Datadog Agent, just like Agent Data Plane (ADP). To do that, ACR
copied large parts of ADP's `internal/` module: the remote agent workload provider, the dynamic log level worker, the
remote agent services, the control plane, the host and autodiscovery providers, the internal telemetry worker, the vsock
mapping, and most of the `run` command. The copies were taken from Saluki 1.5.0, 1.6.1, and 1.6.2, and they have
already drifted.

This crate replaces those copies. A subagent uses it to:

- register with the Datadog Agent over the Remote Agent Registry (RAR)
- receive its configuration over the configuration stream
- get a workload provider backed by the Datadog Agent's tagger and workloadmeta
- serve the status, flare, and telemetry services
- run the standard control plane

A binary only supplies what is truly its own: its configuration system, its topology, its status sections, its
telemetry remapping rules, and any extra API handlers.

## Decisions

1. **The configuration scope stops at the stream.** The runtime yields `mpsc::Receiver<ConfigUpdate>`. Each binary
   keeps its own configuration system and plugs it in through traits. Making `agent-data-plane-config-system` generic
   gets its own plan.
2. **The runtime provides a full facade.** A staged `Subagent` type drives the whole startup sequence. It's built on
   public building blocks that a binary can also use one at a time.
3. **Each extraction is a pure move.** Every pull request in the stack preserves ADP's behavior and migrates ADP onto
   the extracted code in the same pull request. Known bugs are fixed in follow-up pull requests, listed under
   [Follow-ups](#follow-ups).

## Stack

Each pull request keeps ADP's behavior unchanged. The status of each one is either not started, in review (with a link
to the pull request), or merged.

- [ ] **1. Skeleton and remote agent core.** Status: in review (pull request not opened yet).
  - Creates the crate, its workspace entries, its `taskdump` feature, and this document.
  - Moves `internal/remote_agent.rs` to `remote_agent/mod.rs`, adds the `StatusSectionProvider` hook, and takes the
    telemetry remapping rules as a parameter.
  - Adds `RemoteAgentClientConfiguration::from_parts` to `datadog-agent-commons` for the vsock name mapping.
  - Moves ADP's DogStatsD status fields into `DogStatsDStatusSection` and adds `datadog_agent_runtime` to ADP's
    first-party log targets.
- [ ] **2. Logging.** Status: not started.
  - Moves the logging translator, the log level parsing, and `DynamicLogLevelWorker`.
  - Adds `LoggingSettings`, `LoggingTranslator`, and `LiveValue`. The base first-party target list moves into the
    runtime.
- [ ] **3. Environment and workload.** Status: not started.
  - Moves `internal/env/**` and the tagger prefix fixture, and adds `WorkloadSettings` and
    `SubagentEnvironmentProvider`.
  - Updates `.github/workflows/update-agent-tagger-prefixes.yml` for the new paths.
- [ ] **4. Control plane.** Status: not started.
  - Moves the telemetry, runtime configuration, configuration updates, and control plane modules, and adds
    `ControlPlaneBuilder`.
- [ ] **5. Facade.** Status: not started.
  - Adds the `subagent` module and rewrites ADP's `run` command on top of it.
- [ ] **6. Documentation and example.** Status: not started.
  - Adds the crate documentation, `examples/minimal_subagent.rs`, and the `AGENTS.md` entry.

## Invariants

Every pull request in the stack checks its changes against these.

### Names

- The root supervisor is `adp-root`, built as `{identifier}-root`. The `privileged-api-endpoints` integration test and
  the tests in `bin/agent-data-plane/src/cli/debug/runtime.rs` assert this name.
- The supervisors `internal-sup`, `ctrl-pln`, `env-provider`, `workload`, and `autodiscovery` keep their names, as does
  every worker. The `privileged-api-endpoints` integration test asserts `ctrl-pln`.
- The registration task is `adp-remote-agent-task`, built as `{identifier}-remote-agent-task`.

### Routes

- `/workload/remote_agent/tags/dump` and `/workload/remote_agent/external_data/dump`, which the ADP `debug workload`
  command calls.
- `/metrics`, `/compat/metrics`, `/config`, and `/config/runtime`.

### Unchanged behavior

- Metric names.
- Health registry and memory bounds identifiers, such as `env_provider.*`.
- Flare artifact names and size limits.
- Refresh and retry intervals.
- The order of children under `adp-root` and `internal-sup`.

### Log targets

Moved code logs under the `datadog_agent_runtime` target. A plain `log_level` only applies to first-party targets, so
that target must stay in ADP's first-party list. The `adp-rar-registration`, `adp-rar-disabled`, and `adp-cmd-port`
integration tests look for registration log lines, so they catch a missing target.

## Follow-ups

These are deliberately out of scope for the stack, because each one changes behavior. A follow-up pull request removes
its item when it merges.

1. **Supervise the registration and configuration stream loops.** `RemoteAgentBootstrap::new` spawns the registration
   loop with `spawn_traced_named`, and `RemoteAgentBootstrap::create_config_stream` spawns the configuration stream
   loop with `tokio::spawn` (both in `remote_agent/mod.rs`). Neither task is supervised, and neither stops on shutdown.
   Turn them into shutdown-aware workers with a `(Handle, Worker)` shape. Deferred because registration has to finish
   before the supervision tree exists, so this changes the startup sequence.
2. **Use one resolved secure API address.** ADP advertises the secure API address from its local configuration when it
   registers (`bin/agent-data-plane/src/cli/run.rs`), but binds the privileged API to the address from the
   authoritative configuration (`bin/agent-data-plane/src/internal/control_plane.rs`). If the Datadog Agent supplies a
   different `data_plane.secure_api_listen_address`, the two disagree. Deferred because fixing it changes which value
   wins.
3. **Add `datadog_agent_remote_config` to the base first-party log targets.** The list is `FIRST_PARTY_LOG_TARGETS` in
   `bin/agent-data-plane/src/internal/logging.rs`. Deferred because it changes which logs a plain `log_level` shows.
4. **Gate autodiscovery on its first subscriber.** The autodiscovery broadcaster in
   `bin/agent-data-plane/src/internal/env/autodiscovery.rs` starts streaming as soon as its worker starts, and drops
   events while nothing has subscribed, so a late subscriber can miss the initial snapshot. ACR found this. Deferred
   because it changes when events are delivered.
5. **Make the default log file per binary.** `PlatformSettings::get_default_log_file_path` in
   `lib/datadog-agent/commons/src/platform/mod.rs` hard-codes `agent-data-plane.log`. Deferred because it changes a
   public API that other crates use.
6. **Add the hooks ACR needs.** None of these exist yet, and ADP doesn't need them:
   - host tags
   - a `LocalAutodiscoveryProvider` in standalone mode
   - workload provider toggles for a tagger-only setup, without the containerd and cgroups collectors
   - configurable tag store and external data store entity limits, which are hard-coded to 2000 in
     `bin/agent-data-plane/src/internal/env/workload/mod.rs`
   - a process start time for the status `Started` field, which today records when the status service was created
   - extra workers in the `ctrl-pln` supervisor
7. **Make the configuration system generic.** This gets its own plan. Once it lands, `Live<T>` implements `LiveValue`
   directly and ADP's `LiveLogLevel` newtype goes away.
8. **Split `remote_agent/mod.rs` into submodules.** Registration, the configuration stream, the services, flare
   collection, and event reporting can each have their own module. Deferred so that the first pull request keeps rename
   detection in Git for the moved file.
9. **Write an ACR migration guide.** ACR consumes Saluki 1.6.2, so moving to this crate also means adapting to these
   changes since then:
   - `saluki-context` was folded into `saluki-core`.
   - `AppBootstrapper::from_configuration` was replaced by `AppBootstrapper::new`, and the bootstrap supervisor it
     returns has to be added to the supervision tree.
   - `DynamicAPIBuilder` was replaced by `APIBuilder`.
   - The guide also lists the code that ACR deletes in exchange.

## Decided defaults

- **`LiveValue` lives in this crate.** Moving it to `saluki-config` later would be a breaking change for ACR, so where it
  lives gets settled before ACR adopts the crate.
- **Names derived from the app identifier are byte-identical for ADP.** For example, `{identifier}-root` is
  still `adp-root`.
- **`StatusSectionProvider` is synchronous.** That's enough for the health section of ACR.
- **`AppBootstrapper` stays in each binary.** ADP's other subcommands share it, so the facade starts from a
  `BootstrapGuard` and the bootstrap supervisor rather than bootstrapping the process itself.
