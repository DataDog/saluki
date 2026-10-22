# Remote agent runtime extraction plan

This document tracks the extraction of the remote agent infrastructure in `bin/agent-data-plane/src/internal/` into
`datadog-agent-runtime`. It lives next to the code so that the remaining work stays visible after the extraction
stack merges. Each pull request in the stack updates this file in the same commit as its code.

Once the stack merges, this file is the backlog of [follow-ups](#follow-ups). Each follow-up pull request removes its
item, and once the list is empty, the file is deleted or replaced with design notes that are still useful.

## Goal

Agent Check Runner (ACR) runs as a remote agent of the Datadog Agent, just like Agent Data Plane (ADP). To do that, ACR
copied large parts of ADP's `internal/` module: the remote agent workload provider, the dynamic log level worker, the
remote agent services, the control plane, the host and autodiscovery providers, the internal telemetry worker, the vsock
mapping, and most of the `run` command. The copies were taken from Saluki 1.5.0, 1.6.1, and 1.6.2, and they have
already drifted.

This crate replaces those copies. A remote agent uses it to:

- register with the Datadog Agent over the Remote Agent Registry (RAR)
- receive its configuration over the configuration stream
- get a workload provider backed by the Datadog Agent's tagger and workloadmeta
- serve the status, flare, and telemetry services
- run the workers of a standard control plane

A binary only supplies what is truly its own: its configuration system, its topology, its control plane supervisor, its
status sections, its telemetry remapping rules, and any extra API handlers.

## Decisions

1. **The configuration scope stops at the stream.** The runtime yields `mpsc::Receiver<ConfigUpdate>`. Each binary
   keeps its own configuration system and plugs it in through traits. Making `agent-data-plane-config-system` generic
   gets its own plan.
2. **The runtime provides a full facade.** A staged facade type drives the whole startup sequence. It's built on
   public building blocks that a binary can also use one at a time.
3. **Each extraction is a pure move.** Every pull request in the stack preserves ADP's behavior and migrates ADP onto
   the extracted code in the same pull request. Known bugs are fixed in follow-up pull requests, listed under
   [Follow-ups](#follow-ups). The one exception is the status, flare, and telemetry services: they're published as a
   dynamic gRPC route, which is how routes are meant to be published internally, rather than mounted on the privileged
   API endpoint up front.
4. **Each binary builds its own control plane supervisor.** The runtime shares the workers that go into `ctrl-pln`,
   not the supervisor itself, so each binary chooses which of them it runs.

## Stack

Each pull request keeps ADP's behavior unchanged. The status of each one is either not started, in review (with a link
to the pull request), or merged.

- [x] **1. Skeleton and remote agent core.** Status: merged ([#2803](https://github.com/DataDog/saluki/pull/2803)).
  - Creates the crate, its workspace entries, its `taskdump` feature, and this document.
  - Moves `internal/remote_agent.rs` to `remote_agent/mod.rs`, adds the `StatusSectionProvider` hook, and takes the
    telemetry remapping rules as a parameter.
  - Adds `RemoteAgentClientConfiguration::from_parts` to `datadog-agent-commons` for the vsock name mapping.
  - Moves ADP's DogStatsD status fields into `DogStatsDStatusSection` and adds `datadog_agent_runtime` to ADP's
    first-party log targets.
- [x] **2. Logging.** Status: merged ([#2806](https://github.com/DataDog/saluki/pull/2806)).
  - Moves the logging translator and the log level parsing into the runtime, and adds `LoggingSettings` and
    `LoggingTranslator`. The base first-party target list moves into the runtime, and it shrinks to the `datadog_agent`
    and `saluki` target prefixes. None of the crates that this adds or drops both logs and is linked into ADP, so this
    doesn't change which logs a plain `log_level` shows.
  - Moves `DynamicLogLevelWorker` into `saluki-app`, where it applies a stream of already translated `LogLevel`s.
    `LoggingTranslator::translate_log_levels` produces that stream, and ADP feeds it from `Live::into_stream`, which
    this pull request adds to `agent-data-plane-config`.
- [x] **3. Environment and workload.** Status: merged ([#2836](https://github.com/DataDog/saluki/pull/2836)).
  - Moves `internal/env/**` and the tagger prefix fixture, and adds `WorkloadSettings` and
    `RemoteAgentEnvironmentProvider`.
  - Updates `.github/workflows/update-agent-tagger-prefixes.yml` for the new paths.
- [ ] **4. Control plane workers.** Status: in review (pull request not opened yet).
  - Moves the runtime configuration and configuration updates workers, and moves the internal telemetry worker into
    `saluki-core`, next to the internal metrics types. ADP still builds `ctrl-pln` itself, from these workers.
  - Adds `RemoteAgentServicesWorker`, which publishes the status, flare, and telemetry services as a dynamic gRPC route
    on the privileged API endpoint.
- [ ] **5. Facade.** Status: in review (pull request not opened yet).
  - Adds the `facade` module and rewrites ADP's `run` command on top of it. ADP still builds `ctrl-pln`, and adds to it
    the `remote-agent` supervisor from `RemoteAgent::create_remote_agent_supervisor`, which holds the supervision tree route
    and the remote agent workers.
- [ ] **6. Documentation and example.** Status: in review (pull request not opened yet).
  - Adds the crate documentation, `examples/minimal_remote_agent.rs`, and the `AGENTS.md` entry.

## Invariants

Every pull request in the stack checks its changes against these.

### Names

- The root supervisor is `adp-root`, built as `{identifier}-root`. The `privileged-api-endpoints` integration test and
  the tests in `bin/agent-data-plane/src/cli/debug/runtime.rs` assert this name.
- The supervisors `internal-sup`, `ctrl-pln`, `env-provider`, `workload`, and `autodiscovery` keep their names, as does
  every worker. The `privileged-api-endpoints` integration test asserts `ctrl-pln`.
- The facade's supervisor under `ctrl-pln` is `remote-agent`.
- The registration task is `adp-remote-agent-task`, built as `{identifier}-remote-agent-task`.

### Routes

- `/workload/remote_agent/tags/dump` and `/workload/remote_agent/external_data/dump`, which the ADP `debug workload`
  command calls.
- `/metrics`, `/compat/metrics`, `/config`, and `/config/runtime`.
- The status, flare, and telemetry gRPC services on the privileged API endpoint.

### Unchanged behavior

- Metric names.
- Health registry and memory bounds identifiers, such as `env_provider.*`.
- Flare artifact names and size limits.
- Refresh and retry intervals.
- The order of children under `adp-root` and `internal-sup`.

### Log targets

Code moved into this crate logs under the `datadog_agent_runtime` target, and `DynamicLogLevelWorker` logs under
`saluki_app`. A plain `log_level` only applies to first-party targets, so both targets must stay covered by
`FIRST_PARTY_LOG_TARGETS` in `lib/datadog-agent/runtime/src/logging.rs`, which every remote agent's
`LoggingTranslator` starts from. Its entries are target prefixes, so the `datadog_agent` and `saluki` entries cover
them. The `adp-rar-registration`, `adp-rar-disabled`, and `adp-cmd-port` integration tests look for registration log
lines, so they catch a missing `datadog_agent_runtime` target.

## Follow-ups

These are deliberately out of scope for the stack, because each one changes behavior. A follow-up pull request removes
its item when it merges.

1. **Supervise the registration and configuration stream loops.** `RemoteAgentBootstrap::new` spawns the registration
   loop with `spawn_traced_named`, and `RemoteAgentBootstrap::create_config_stream` spawns the configuration stream
   loop with `tokio::spawn` (both in `remote_agent/mod.rs`). Neither task is supervised, and neither stops on shutdown.
   The registration loop also keeps running when `RemoteAgentBootstrap::new` returns an error, so a caller that retries
   leaves one extra registration loop behind per failed attempt. Turn them into shutdown-aware workers with a
   `(Handle, Worker)` shape, and then remove the exception from the conventions in `src/lib.rs` and the background task
   notes on both methods. Deferred because registration has to finish before the supervision tree exists, so this
   changes the startup sequence.
2. **Use one resolved secure API address.** `RemoteAgent::start` advertises the address from
   `LocalConfiguration::advertised_secure_api_address` when it registers, which ADP takes from its local configuration.
   ADP binds the privileged API to the address from its authoritative configuration
   (`bin/agent-data-plane/src/internal/control_plane.rs`). If the Datadog Agent supplies a different
   `data_plane.secure_api_listen_address`, the two disagree. Deferred because fixing it changes which value wins.
3. **Gate autodiscovery on its first subscriber.** The autodiscovery broadcaster in
   `lib/datadog-agent/runtime/src/env/autodiscovery.rs` starts streaming as soon as its worker starts, and drops
   events while nothing has subscribed, so a late subscriber can miss the initial snapshot. ACR found this. Deferred
   because it changes when events are delivered.
4. **Make the default log file per binary.** `PlatformSettings::get_default_log_file_path` in
   `lib/datadog-agent/commons/src/platform/mod.rs` hard-codes `agent-data-plane.log`. Until then,
   `LoggingTranslator::new` takes the default log file from the binary, and ADP passes that function's result. Deferred
   because it changes a public API that other crates use.
5. **Add the hooks ACR needs.** None of these exist yet, and ADP doesn't need them:
   - host tags
   - a `LocalAutodiscoveryProvider` in standalone mode
   - workload provider toggles for a tagger-only setup, without the containerd and cgroups collectors
   - configurable tag store and external data store entity limits, which are hard-coded to 2000 in
     `lib/datadog-agent/runtime/src/env/workload/mod.rs`
   - a process start time for the status `Started` field, which today records when the services worker was created
6. **Make the configuration system generic.** This gets its own plan.
7. **Split `remote_agent/mod.rs` into submodules.** Registration, the configuration stream, the services, flare
   collection, and event reporting can each have their own module. Deferred so that the first pull request keeps rename
   detection in Git for the moved file.
8. **Write an ACR migration guide.** ACR consumes Saluki 1.6.2, so moving to this crate also means adapting to these
   changes since then:
   - `saluki-context` was folded into `saluki-core`.
   - `AppBootstrapper::from_configuration` was replaced by `AppBootstrapper::new`, and the bootstrap supervisor it
     returns has to be added to the supervision tree.
   - `DynamicAPIBuilder` was replaced by `APIBuilder`.
   - The guide also lists the code that ACR deletes in exchange.

## Decided defaults

- **Runtime log level changes arrive as a `Stream`.** `DynamicLogLevelWorker` takes a stream of translated `LogLevel`s,
  so it doesn't depend on any configuration system. Each binary turns its own change notifications into that stream:
  ADP uses `Live::into_stream` and `LoggingTranslator::translate_log_levels`.
- **Names derived from the app identifier are byte-identical for ADP.** For example, `{identifier}-root` is
  still `adp-root`.
- **`StatusSectionProvider` is synchronous.** That's enough for the health section of ACR.
- **`AppBootstrapper` stays in each binary.** ADP's other subcommands share it, so the facade starts from a
  `BootstrapGuard` and the bootstrap supervisor rather than bootstrapping the process itself.
