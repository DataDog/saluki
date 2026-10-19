# Component test driver migration plan

This document tracks the migration of component tests onto the component test driver. Each pull request in the stack
updates this file in the same commit as its code, and the last one deletes this file.

## Goal

[#2728] added `TestComponentDriver`, `ComponentControl`, `Outputs`, and `TestComponentSupervisor` in
`lib/saluki-core/src/components/test_util/`. About a dozen tests still run components through hand-built harnesses. A
few more copy parts of a run loop by hand, and several test setups use bare supervisors.

Some of these tests can move to the driver today. The others need helpers that the driver doesn't have yet. This stack
adds those helpers, converts the tests, and removes the hand-built harnesses. Nothing in it changes production
behavior.

[#2728]: https://github.com/DataDog/saluki/pull/2728

## Decisions

1. **Every helper lands in its own pull request before any test moves.** Each helper comes with unit tests in
   `test_util/tests.rs`, rustdoc in `test_util/mod.rs`, and an entry in `docs/development/testing-patterns.md` when it
   matters to people writing tests.
2. **Conversions are grouped by component type, one pull request per type.** A type with nothing to convert gets no
   pull request, so there is none for relays, decoders, forwarders, or destinations.
3. **The scope is conversions only.** Run loops that have no coverage today go on the [Follow-ups](#follow-ups) list.
4. **`retained_context_identity_follows_dogstatsd_post_processing` is deleted, not converted.**
5. **Two helpers are deferred.** No conversion needs `wait_until_not_ready` (F6) or piping one driver's output into
   another component's input (F7), so both are follow-ups.
6. **Every pull request is test-only.** Each one is a `chore(test): …` pull request with the
   `changelog/no-changelog` label.

## Stack

Each pull request checks off its own item.

- [x] **1. Plan.**
  - Adds this document.
- [x] **2. F1: `ComponentControl::wait_until`.**
  - Polls a condition within the control's wait timeout. The panic message names the component, and says if its `run`
    has already returned, as `wait_until_ready` does.
  - For updates that don't arrive on the input, such as a `Live<T>` change. Tests typically poll a `TestRecorder`
    counter.
- [x] **3. F2: pre-built source.**
  - `PrebuiltSource` is a `SourceBuilder` that wraps an already-built `Box<dyn Source + Send>` and its output
    definitions. Its `build` hands the box over once and returns an error on a second build. Its memory bounds are
    empty.
  - Works with every `DriverOptions` setting. Sources only for now.
- [x] **4. F3: `synchronous_transform` entry point.**
  - `TestComponentDriver::synchronous_transform` and `DriverOptions::synchronous_transform` run a
    `SynchronousTransformBuilder` through a small adapter `Transform`. The adapter receives a buffer, calls
    `transform_buffer`, dispatches the buffer to the default output, and stops when its input closes. A failed
    dispatch stops it with the dispatch error.
  - The adapter accepts every event type, as `ChainedConfiguration` does. No conversion needs it yet; its consumers
    are follow-ups.
- [x] **5. F4: shutdown separate from the supervisor.**
  - `DriverOptions::with_shutdown_withheld`, for sources and relays. The component then gets a shutdown handle that
    the control owns, in place of the one from its worker, so signalling shutdown stops only the supervisor, which
    aborts the component once the budget runs out. The handle fires once the control is dropped.
  - The substitution happens in the driver, so production code doesn't change.
- [x] **6. F5: contexts without running.**
  - One `DriverOptions` method per component type, such as `source_context`, which takes output definitions rather
    than a builder. Each returns `ContextParts`: the context that the driver would have passed to `run`, the input
    sender, and the outputs. The parts also keep the topology context and health registry alive. Nothing is spawned.
  - The context of a source or relay comes with a shutdown handle installed, which `ContextParts::signal_shutdown`
    fires.
  - For tests that drive a sub-piece of a component with a real component context.
- [x] **7. F8: `TestComponentSupervisor` with static workers.**
  - `TestComponentSupervisor::start_with(id, configure)` lets `configure` add static workers before the supervisor
    runs, and still waits until the supervisor is running. Its `dataspace()` replaces the hand-written
    `DataspaceCapture` workers.
- [x] **8. Sources conversions.** Needs F2, F4, and F5.
  - Converts the tests listed under [Sources](#sources), and removes `set_shutdown_handle_for_test` from
    `SourceContext` and `RelayContext`.
  - The decoder sub-piece tests no longer drain the `metrics` output in the background: the output holds 128 buffers,
    more than any of them dispatches.
- [ ] **9. Transforms conversions.** Needs F1.
  - Converts the tests listed under [Transforms](#transforms), and deletes the duplicated fake source and destination
    builders.
- [ ] **10. Encoders conversions.**
  - Converts the test listed under [Encoders](#encoders).
- [ ] **11. Misc conversions and cleanup.** Needs F5 and F8.
  - Converts the setups listed under [Misc](#misc), and adds `saluki-core` with the `test-util` feature to the
    dev-dependencies of `saluki-app` and `saluki-io`.
  - Moves the [Follow-ups](#follow-ups) to tracking issues, and deletes this file.

## Inventory

Tests are named rather than located by line, since line numbers drift as the stack lands.

### Sources

In `lib/saluki-components/src/sources/dogstatsd/mod.rs`:

- **The driver with `with_resource_registry`:** `no_created_listener_runs_idle_only_when_listeners_are_optional` and
  `listener_that_cannot_be_created_is_skipped_unless_all_are_required`, which landed already on the driver.
- **F2 with driver options,** all in the `supervision` module:
  - `background_work_runs_as_the_sources_own_children`
  - `source_does_not_finish_until_the_decoders_have_drained`, with `with_interconnect_capacity(1)` and a paused clock
  - `stream_handlers_are_the_listeners_own_children`
  - `the_registry_keeps_the_listener_bound_after_the_source_stops`

  `build_source_with` keeps building the `DogStatsD { .. }` literal, because the configuration can't ask for an
  ephemeral port: `build_addresses` treats port 0 as disabled. F2 wraps the literal, and `run_source` and the
  hand-wired context go away. Expectations on the supervision tree change from the node name `source` to
  `sources.<id>`.
- **F4:** `a_source_ignoring_shutdown_is_cut_off_with_its_children`.
- **F5,** for the decoder sub-piece tests, which use crafted peer identities that a real socket can't produce. F5
  replaces `test_source_context()`.
  - `connectionless_decoder_dispatches_full_and_flushed_buffers_and_forwards_frames`
  - `connected_decoder_waits_for_complete_outer_frame_and_stops_on_eof`
  - `uds_stream_capture_records_every_outer_frame_across_reads`
  - `uds_stream_capture_starting_mid_frame_captures_from_next_whole_frame_and_stops_cleanly`
  - `connectionless_decoder_counts_origin_detection_errors_by_reason`
  - `decoder_continues_connectionless_but_stops_connected_on_framing_error`

### Transforms

- **`transforms/apm_stats/mod.rs` `run_refreshes_peer_tag_keys_before_each_buffer`** hand-wires its contexts, and
  takes its health handle from a registry that is dropped straight away, which turns the run loop into a busy loop. It
  moves to the driver: `publisher.accept_registry`, then `send_event`, `shutdown`, and collect.
- **`transforms/aggregate/mod.rs` `production_owner_loop_serves_snapshots_and_stops_after_cancellation`** runs a
  blueprint with a fake source, the aggregate transform, and a fake destination. It moves to the driver, and
  `ControlledMetricSource*` and `DrainingMetricDestination*` are deleted.
- **`bin/agent-data-plane/src/components/dogstatsd_no_agg_split/mod.rs` `preaggregated_counters_to_rate`** drives
  `PassthroughBatcher` through `build_passthrough_dispatcher`. It folds into the existing driver test, with
  `start_paused` and a 1 ns idle timeout, and the helper is deleted.
- **`bin/agent-data-plane/src/cli/run.rs`:**
  - `retained_context_identity_follows_dogstatsd_post_processing` is deleted.
  - `no_agg_split_excludes_timestamped_metrics_from_tag_filterlist` is deleted, once the no_agg_split driver test
    asserts that timestamped metrics reach only the `passthrough` output.
  - The fakes that only those two tests use are deleted, along with the imports nothing uses anymore.
- **Live-update tests that copy the update branch of the run loop by hand,** which move to F1:
  - `tag_filterlist/mod.rs` `typed_live_updates_replace_whole_tag_rules_without_replacing_static_value_rules` polls
    `tag_filterlist_updates_total`. This also covers the cache rebuild, which nothing exercises today.
  - `dogstatsd_prefix_filter/mod.rs` `typed_live_updates_rebuild_the_matcher` polls
    `metric_filterlist_updates_total`. The test-only `FilterBuilder` also gets the trailing `.` that `build()` adds to
    the prefix. Without it, `prefix_matching_blocklist_applies_after_prefixing` matches `foobar` rather than the
    `foo.bar` its comment describes, and only passes because `fo` matches both.
  - `dogstatsd_post_aggregate_filter/mod.rs` `typed_live_updates_rebuild_the_effective_matcher` polls the output with
    probe metrics, since the component has no update counter.
- **Telemetry caveat:** `MetricsBuilder::from_component_context` adds `component_id` and `component_type` labels, so
  these tests set `with_component_id(..)` and include the labels in their `TestRecorder` keys. Counters that the run
  task registers lazily need a current-thread runtime, which is the `#[tokio::test]` default.

### Encoders

- **`encoders/datadog/metrics/mod.rs` `authoritative_v3_flushes_previous_point_limit_batch`** runs the real encoder
  through the driver, with `max_series_points_per_payload = 3`, a 250 ms `flush_timeout`, and authoritative V3
  series, and asserts on the HTTP payloads in the output.
- **`authoritative_v3_sketches_flush_previous_point_limit_batch` stays on `RequestBuilderScenario`.** It needs V3
  sketches with no V2 sketch builder. `build` always creates one, so production can't reach that state.

### Misc

- **The two racy OTLP server tests in `common/otlp/mod.rs`,** `build_succeeds_with_unix_grpc_endpoint` and
  `grpc_export_works_over_unix_socket`, use a bare `Supervisor` that they spawn and then yield to once. They move to
  `TestComponentSupervisor::start`.
- **Four supervisor setups move to F8,** and both `DataspaceCapture` copies are deleted:
  - `lib/saluki-app/src/api.rs`
  - `lib/saluki-app/src/logging/mod.rs`
  - `ServerTestHarness::start` in `lib/saluki-io/src/net/server/test_util.rs`
  - `bin/agent-data-plane/tests/config_cli.rs`
- **The hand-built context helpers of the double-take panic tests** in `lib/saluki-core/src/components/mod.rs` are
  replaced with F5.

### Kept as they are

- **Sub-pieces under `TestComponentSupervisor`, not whole components:** `sources/dogstatsd/forwarder.rs`; `io.rs`,
  `api_key.rs`, `retry.rs`, and `validation.rs` in `common/datadog/`; the other `common/otlp` server tests; and the
  `forwarders/datadog` `ApiKeyRefresher`.
- **The `spawn_component` stand-in in `common/datadog/io.rs`:** the real forwarder would start the API key validator,
  and its extra `/api/v1/validate` traffic would let two tests pass for the wrong reason.
- **The topology-level run tests in `saluki-core/src/topology/blueprint.rs`:** the driver re-implements this wiring
  itself, so moving them would stop testing the blueprint.
- **Tests of primitives and internals:** `topology/built.rs`, `graph.rs`, `interconnect/*`, and `health`; the
  dogstatsd_mapper cache tests; and the aggregate flush tests, which need an injectable clock rather than a driver
  feature.
- **Pure unit tests** of helpers, `transform_buffer`, and the encode functions.

## Follow-ups

These are out of scope for the stack. The last pull request moves them to tracking issues.

1. **Deferred helpers.**
   - F6: `wait_until_not_ready`. Only useful if `a_rotated_key_is_revalidated_before_the_next_interval` in
     `common/datadog/validation.rs` becomes a whole-forwarder test.
   - F7: piping one driver's `Output` into another `ComponentControl`.
2. **Run loops with no coverage that the driver can now reach.**
   - Transforms: `trace_sampler` (its `metrics` output), `metric_router` (matched and unmatched),
     `autoscaling_failover_gateway`, and `metric_filter`.
   - Synchronous transforms, through F3: dogstatsd_mapper, host_enrichment, trace_obfuscation, trace_tag_replacer, and
     the ottl_* transforms. apm_onboarding and host_tags are blocked by the components themselves.
   - Sources: `liveness`, `heartbeat`, and `otlp`.
   - Encoders: `buffered_incremental` with events, logs, and service checks, and the datadog `stats` and `traces`
     encoders.
   - Destinations: `prometheus`, `dogstatsd_client_telemetry`, and the `AlreadyRunning` branch of `dsd_stats`.
   - The buffer-full flush branch of `dogstatsd_no_agg_split`.
3. **Existing TODOs.** The `aggregate` `telemetry()` test (`component_events_dropped_total`), and the metrics
   encoder's end-to-end routing test, including payloads drained at shutdown.
