# Conventions

This page collects the project-specific rules that aren't mechanical enough for the
[style guide](./style-guide.md) or a lint: design preferences, scope decisions, and matters of taste. Reviewers call
these out in PR reviews when they're missed. A pull request number at the end of a rule points to the fix for a bug
that the rule guards against.

When a review or a design discussion settles a question that's likely to come up again, add it here.

## Code comments

Keep code comments, including rustdoc on private items, short. A comment should do one of two things:

- flag a caveat that a reader could easily miss
- annotate a large block of logic at a high level, so that it's easier to navigate

Don't write comments that restate what the code already says, and don't re-explain the design at every call site.
Explain a design once, in the module documentation, and keep comments at call sites to a sentence or two. Before
keeping a comment, ask whether deleting it would make the reader miss anything.

Comments describe the code as it is now. Don't write comments that describe a change, such as "previously," "now
uses," or "replaces X": Git keeps the history. A regression test can name the failure that it guards against and link
its issue. Outside tests, don't add banner comments such as `// ---- Section ----`. To annotate a large block, write a
full sentence instead. Don't commit proof-of-concept notes or design write-ups as Markdown files, next to the code or
under `docs/`. Move the useful details into code comments, or into the docs page that owns the topic. Plans for work
that spans several pull requests follow [Plans](./agents.md#plans).

Word comments in the [ASD-STE100 style](./style-guide.md#writing-style).

## Generated and vendored files

<!-- review:generated-files -->
Never hand-edit files marked `@generated` or `DO NOT EDIT`. Change their source of truth, and regenerate them.
<!-- /review -->

The vendored Datadog configuration schema under `lib/datadog-agent/config/schema/core/` is a pristine upstream copy,
so never hand-edit it either. [Configuration system](./configuration-system.md) explains how to change what the schema
feeds into.

## Matching the Datadog Agent

<!-- review:agent-parity -->
`agent-data-plane` (ADP) matches what users observe from the Datadog Agent: output, defaults, accepted input, and the
result of bad input. ADP can differ when the Agent's reason for a behavior doesn't apply and the pull request
description or a code comment says so. Don't flag that difference. ADP doesn't copy Agent internals, or Go patterns that
don't fit Tokio, unless the pull request says why. An example is a rendezvous channel on every loop iteration.

- Some changes claim parity, or change a default, a limit, or the accepted input for a setting or protocol that the
  Agent also has. Such a change links to Agent source pinned to a commit, or tests against values from the Agent. If
  it does neither, ask what the Agent does.
- When a change fixes one path, check its sibling paths. Sibling paths include aggregated and timestamped metrics;
  series and sketches; each encoder version; each transport, including Windows named pipes; and each signal. Flag a
  sibling that has the same defect and stays unfixed, or that inherits behavior meant for another path. (V2 series
  ignored a point limit that V3 obeyed: #2055. V3 sketch payloads inherited the series limits: #3082.)
- When the Agent logs a bad value and continues, ADP continues too. Flag a new startup error for such a value, and a
  startup step that a short outage can make fatal. A startup error becomes a crash loop. (One invalid mapper entry
  stopped DogStatsD from starting: #3114.)
<!-- /review -->

## Reuse what exists

Before you add a mechanism, look for an existing one that does the job.

<!-- review:reuse -->
Flag a new type, handle, or task that does the work of an existing mechanism, and ask what it adds. These come up
most often:

- Event batching and dispatch use `BufferedDispatcher` from `lib/saluki-core`.
- Retries and backoff use `ExponentialBackoff` and the retry policies in `lib/saluki-io/src/net/util/retry/`.
- Datadog sites, endpoints, and request building live in `lib/saluki-components/src/common/datadog/`.
- A pooled object goes back to its pool when it drops, so it doesn't need a hand-written return path.
- Flag logic that the standard library or a workspace crate already provides, and a constant that two crates each
  define.
<!-- /review -->

## API routes and control plane workers

<!-- review:api-routes -->
Publish internal API routes and gRPC services by asserting a `DynamicRoute` from a worker. Flag a new static handler
added with `APIBuilder::with_handler` or `APIBuilder::with_grpc_service`. Shared crates provide workers, and each
binary assembles its own control plane supervisor from them. Flag a library builder that assembles a binary's control
plane from several workers. A server type's `into_supervisor` method is fine.
<!-- /review -->

Dynamic routes let any worker in the supervision tree contribute to an endpoint it doesn't own, which keeps workers
composable.

## Hot paths and memory

<!-- review:hot-paths -->
Much of Saluki runs once for each metric, log, or span. In that per-event code, flag:

- a clone, an owned `String`, or a `format!` call where a borrow, a `&'static str`, or a value that the code already
  owns works
- a method that clones large data and doesn't say so in its name
- a fixed string in an event, tag, or context that doesn't use `MetaString::from_static` or string interning
- a builder, resolver, buffer, or metric handle that the code makes for each event, when one instance works
- a temporary `Vec` or map that the code fills only to iterate over once
- a `SystemTime::now` call where one-second precision is enough: use `saluki_common::time::get_coarse_unix_timestamp`

Regressions that only affect tail latency (p95, p99) also count. In production code, outside `bin/correctness/` and
`test/`, flag:

- an unbounded channel or queue, or a very large bound with no reason. A new channel usually holds 64 to 128 items. A
  queue that something else bounds, such as a supervisor's queue of children to spawn, is fine.
- a collection that grows for each connection, key, or context with no size limit, expiration, or removal on every
  exit path. Exit paths include disconnect, flush, capture stop, and delete. Also check an expiry setting of `0`,
  which turns expiry off. (A UDS capture buffer grew without limit and stayed after capture stopped: #2823.)
- a per-connection or per-request value in a task name, metric tag, or component ID. Each value makes a new metric
  series. (A stream index in task names leaked memory and CPU: #1899.)
<!-- /review -->

## Concurrency and lifecycle

<!-- review:concurrency -->
Components run concurrently on Tokio, under supervisors, with explicit startup and shutdown. Flag:

- a blocking call in async code, such as `std::fs` I/O
- a spawned task that nothing joins or cancels
- a component loop that isn't a `loop { select! { ... } }` with a `health.live()` arm, such as a `while let` loop
  over input
- a `select!` arm that awaits work whose duration grows with buffered state, such as a whole flush. (A slow flush in
  one arm made `/live` fail on CPU-starved pods: #2726.)
- a final flush or shutdown path that reuses a steady-state condition, such as an idle timeout, and so leaves buffered
  data unsent. A lifecycle change has a test that stops the component right after input and checks the output. (A
  final flush that reused the idle check dropped recent metrics: #2820.)
- a shutdown path that can hang, or that differs between `select!` arms
- a notification sent before readers can see the new state, and a ready signal sent before the listener binds.
  (Configuration update events went out before the configuration held the new values: #1914.)
- `abort()` as the only way to stop `spawn_blocking` work or a thread. That work checks a
  `saluki_common::sync::shutdown::ShutdownHandle` instead. (The cgroups poller kept running after its collector
  dropped: #1848.)
- two tasks that can each block on a full bounded channel that the other one drains
- a silent drop where backpressure works, such as a `broadcast` channel whose receivers can lag. An `mpsc` channel
  that replaces a `broadcast` channel has one receiver, so check that no second subscriber exists. (A lagging
  `broadcast` receiver lost autodiscovery events: #1671.)
- a `broadcast` receiver that handles `RecvError::Lagged` like `RecvError::Closed`. A receiver that lags can still
  receive.
- a `Notify` used as if `notify_waiters()` stores a permit, as `notify_one()` does. It wakes only the `Notified`
  futures that exist when it runs.

Each supervised child declares its own shutdown strategy, and its owner enforces exactly that strategy. Flag an owner
that keeps back part of a child's grace period as a margin, passes its own deadline down to its children, or treats a
child supervisor differently from a child worker. A child that overruns its grace period has a bug: fix the child and
its tests, not the deadlines.
<!-- /review -->

The owner is a supervisor or a scope. If something outside has a shorter limit, it preempts by aborting the subtree.
That's by design.

## Numbers and identity

<!-- review:numbers -->
- Use checked or saturating math on sizes, bounds, and durations from configuration. Tagged releases build with
  `overflow-checks = false`, so integer overflow wraps silently. `Duration` arithmetic panics in every build. (A large
  memory bound wrapped to a tiny one: #2668.)
- Flag plain `-` between two snapshots of an unsigned counter. Use `saturating_sub` or `checked_sub`, because a reset
  or a key collision can make the later value smaller. (Two groups with the same name made one subtraction use two
  different counters: #1852.)
- Flag division or scaling by an input or configured value that can be `0`, such as a sample rate. (A sample rate of
  `@0` counted each point about 260 million times: #3073.)
- Flag a feature, a report, or a limit that a setting turns off with its default or with a valid `0`, when the Agent
  doesn't. For example, a rate limit keyed on an interval that defaults to `0` can send no reports at all.
- Flag a metric value from external input with no check for non-finite values. Each metrics encoder drops non-finite
  points, and a new encoder does too. Histogram bucket bounds can be infinite. (Non-finite points reached V3
  payloads: #1970.)
- Flag hashing, serialization, or IDs that depend on `HashMap` iteration order. Sort first, or use a `BTreeMap`.
  (Check ID hashes changed between runs: #1213.)
<!-- /review -->

## Internal telemetry

<!-- review:telemetry -->
- `agent-data-plane` adds the `adp.` prefix to internal metric names at bootstrap, through
  `AppBootstrapper::with_metrics_prefix`. Flag a metric that code registers with the prefix in its name. Remap rules
  match the prefixed name, which is correct.
- A metric measures what its name says, in that unit, and its type fits. A counter of events doesn't count payloads.
- Components emit the standard `component_*_total` metrics with a tag for the cause, such as
  `component_events_dropped_total` with `drop_reason`. Flag a component-specific name for the same thing.
- Each path that drops or changes input increments a counter. Count an outcome after it happens, such as bytes sent
  after a successful response. (Bytes counted at the socket write included failed requests: #2582.)
- When the Datadog Agent expects another name, add a remap rule in `bin/agent-data-plane/src/state/metrics/rules/`.
  The target doesn't start with `datadog.agent.`, which the Core Agent adds, and it carries the Agent's help text. A
  metric or tag rename updates the rules that match it. (A target with the prefix got it twice: #1814.)
<!-- /review -->

## Security

<!-- review:security -->
- Each `unsafe` block and `unsafe impl` has a `// SAFETY:` comment that says why its invariants hold.
- A route that changes runtime state, or that returns configuration, workload data, or captured traffic, uses
  `EndpointType::Privileged`. That endpoint requires a client certificate. (The privileged API once accepted clients
  without a certificate: #2237.)
- Outside tests, code that builds a TLS config directly with `rustls`, and not with the `saluki-tls` builders, calls
  `ensure_client_config_fips_compliant` or `ensure_server_config_fips_compliant`. (Server TLS configs skipped the FIPS
  check: #1954.)
- Flag `danger_accept_invalid_certs` outside tests, unless a setting turns it on and the code logs a warning.
- Flag logs, errors, and `Debug` output that can contain API keys, tokens, or passwords. Flare files that ADP returns
  to the Core Agent are fine, because the Core Agent scrubs each file that it adds to a flare.
- Scrubbing and obfuscation fail closed. Flag an error branch that returns or forwards the input unchanged.
  (Malformed JSON queries skipped obfuscation: #2601.)
<!-- /review -->

## Build and release plumbing

These rules cover the build and release files at the root of the repository, and code that depends on the platform.
Examples are the `Makefile`, `ci/`, `docker/`, `.gitlab-ci.yml`, and `Cargo.lock`.

<!-- review:build-plumbing -->
- A renamed or new build variable has the same name in every producer and consumer. This covers Make variables,
  Docker build `ARG` values, CI variables, and the `APP_*` variables that `lib/saluki-metadata/build.rs` reads. (CI
  passed `APP_DEV_BUILD`, but `docker/Dockerfile.agent-data-plane` didn't declare it, so release images reported a
  development version: #2345.)
- Developers build on macOS. Flag `Makefile` defaults from the host, such as `rustc -vV` or `uname`, that reach
  Linux container builds. Flag scripts that fail on an unknown architecture. (A host-triple default broke image
  builds on macOS: #1067.)
- In `Cargo.lock`, flag a downgrade of a crate that the pull request doesn't target. (An `h2` bump downgraded
  `windows-sys` for unrelated crates: #2367.)
- ADP builds for Linux, macOS, Windows, and AIX, which is big-endian. Flag code that assumes little-endian byte order
  at runtime, such as `from_ne_bytes` or `transmute` on bytes from outside the process, a union layout, or a tag at a
  fixed byte index. `make check-aix-cross` only type-checks. (`MetaString` kept its tag at a byte index that assumed
  little-endian layout: #2032.)
<!-- /review -->

## Core crate

These rules cover the data model, the interconnect, the object pools, and the internal metrics registry in
`lib/saluki-core`, which every other crate uses.

<!-- review:saluki-core -->
Apply these rules to files under `lib/saluki-core`: the data model, the interconnect, the object pools, and the
internal metrics registry.

- The event types under `src/data_model/event/` are Saluki's own model. Flag a field or method on them that takes or
  returns a protocol type, such as an OTLP or protobuf type. Sources and decoders translate input into the model.
  Encoders translate the model into output, so one encoder works for events from every source. `DDSketch` and the
  `serde_json::Value` in log properties are part of the model. Payload types under `src/data_model/payload/` are
  transport-level and can hold types such as `http::Request`.
- When behavior depends on the input protocol, the event holds it in a typed field that the source sets at
  construction, as `TraceIngestInfo` does. Flag a new event method that gets the protocol from tags or attributes,
  because transforms can change or remove them. (The trace sampler treated a trace as OTLP when its root span had an
  `otel.trace_id` attribute. It sampled native traces with that attribute as OTLP: #2854.)
- `Event` is as large as its largest variant, and it moves by value through each component. Each new field costs
  memory, and a field on the largest variant makes every event larger. Flag a new event field that nothing reads,
  even when the Datadog Agent has that field. A field is fine when the description names the stacked pull request
  that reads it. Flag a larger budget in `core_event_types_stay_within_size_budget` that the description doesn't
  explain.
- A change can keep a public signature and still change the result, as when `EventD::hostname()` started to return
  `Some("")`. Search the repository for each caller. Flag a caller that the new result breaks, such as one that
  expects `Some` to hold a hostname that isn't empty. Put the comment on the changed item, and give the caller's path.
  If the description doesn't mention the new result, ask whether the author intends it.
- `MetricValues::merge` combines two points of the same type only when their timestamps are equal. A rate whose
  interval changes, and a value of another type, replace all points on purpose. Flag other merge code that replaces a
  point that has a different timestamp. (A gauge merge dropped the earlier point, so a gauge sent every 10 seconds
  went out every 30 seconds: #444.)
- Sum weighted `f64` samples with compensated summation, as `neumaier_add` in
  `src/data_model/event/metric/value/histogram.rs` does. (A plain sum of `{1, 1e100, 1, -1e100}` gave `0` instead of
  `2`: #1666.)
- `Context` equality and hashing use only the context key. Code that changes the name, host, tags, or origin tags of
  a context calculates the key again. Tag order and repeated tags don't change the key. (Context hashing combined tag
  hashes with XOR and didn't skip repeated tags, so a tag that appeared twice removed itself from the key: #209.)
- In an object pool, each event that frees capacity, such as a release or a shrink, wakes the waiting tasks. (A
  shrink made room but added no permit, so waiters didn't wake: #1925.) Flag code that removes a pooled item before it
  holds the item's permit. (The shrinker took an item promised to a waiter, which then panicked: #371.)
- `Dispatcher` and `Consumer` count items with `Dispatchable::item_count`. `events_sent_total` counts each item once
  for each active sender, and `events_elided_total` counts it once for each sender whose gate is inactive.
  `events_discarded_total` counts each item once when the output has no sender. `DispatchTarget::send` and
  `BufferedDispatcher::flush` both add to `events_sent_total`, so check both on the buffered path. When a change
  touches a send or receive path, check each counter on every path. (A refactor of `Dispatcher` and `Consumer` broke
  their event counts: #822.)
- At each flush, the internal metrics registry evicts a metric that has no handle left and no update since the last
  flush. The registry keeps a metric that something registered in the last `IDLE_EVICT_INTERVALS` flushes.
  `delete_counter` removes a counter at once. Flag a new handle, boolean, or task that tracks whether a metric is in
  use, and ask what it adds.
<!-- /review -->

## Project scope

- The `standalone` mode of `agent-data-plane` is left over from earlier development and testing. It isn't for
  production use, and supporting it shouldn't block feature development.
- `panoramic` under `bin/correctness/` is our black-box correctness and integration tool. `stele` is a library
  of simplified telemetry data types shared by the correctness tools. Neither replaces unit tests for the branches
  inside a crate, and neither is the Antithesis platform, which lives separately under `test/antithesis/`. See
  [Testing](./testing.md).
