# Programmable metric processing: status

**Status:** the state of the work at `b01049372e` (October 10, 2026), on branch `luke/programmable-metrics`. This
document records where the implementation of [the specification](programmable-metrics.md) stands, how to run it, what
it changed outside its own crates, what is still open, and what the next stages inherit. It summarizes and links to
[the implementation design](programmable-metrics-implementation.md) rather than repeating it, and it holds the open
items that no other document in the repository records. Where a document disagrees with the code, the code is
authoritative.

## Contents

1. [Where things stand](#where-things-stand)
2. [Documents](#documents)
3. [Running it](#running-it)
4. [Code map](#code-map)
5. [Changes users can see](#changes-users-can-see)
6. [Changes outside the new code](#changes-outside-the-new-code)
7. [Decisions awaiting the owner](#decisions-awaiting-the-owner)
8. [Spec amendments index](#spec-amendments-index)
9. [Known limitations and risks](#known-limitations-and-risks)
10. [Deferred work by stage](#deferred-work-by-stage)
11. [Environment notes](#environment-notes)
12. [How the work was done](#how-the-work-was-done)

## Where things stand

Stages 1 to 7 of the specification are implemented, with a local demo (committed after stage 2, connected to ADP's
artifact channel and given its headless check in stage 5, and extended in stages 6 and 7):

- ADP's metrics path always runs from the plan. Lowering turns the native configuration into the default program, the
  frame lays it out once at boot, and region hosts run its regions. The legacy metrics path exists only under
  `cfg(test)`, as a differential reference, so there is no production switch back to it.
- Configuration changes, policy artifacts, and aggregation changes install live as generations: stateless ones with a
  prepare barrier and one schedule store, aggregation changes whose bucket width changes at a cut.
- DogStatsD aggregation runs as a stateful kernel in the `dsd_agg` isolation slot, bucketing by admission time and
  closing buckets by watermark.
- Policy artifacts arrive through a local development stub of the artifact channel, for two scopes: `post_enrich` (the
  policy region) and `dogstatsd_pre_aggregation` (DogStatsD processing up to and including aggregation). The privileged
  API serves the status, export, plan, explain, and default program.
- Outputs and recipients are resources of hosts fixed at boot (stage 7): one encode host and one delivery host per
  fixed delivery host, `dd_out`'s, the failover pair (idle until failover is enabled), and the Cluster Agent pair when
  its endpoint resolves at boot. Each output runs as a lease of its encode host and each recipient as an I/O loop of
  its delivery host; the installer prepares them, publishes each host's native set and settings, withdraws what the
  configuration no longer declares, and drains and releases what no generation uses. So endpoints, routing groups,
  failover, keys, proxies, and delivery settings change live, logs, events, and traces with them, and a resources
  artifact (the stub channel's `resources` product) can declare recipients and outputs that policy artifacts route
  to. The privileged API serves the resources and every recipient incarnation. See
  [the stages 7 and 9 plan](programmable-metrics-stages-7-and-9.md), its "Stage 7 as built," and the
  [implementation design's summary](programmable-metrics-implementation.md#stage-7-resource-generations-as-built).
- Stage 9 is designed; its measurement harness (P9.0: probes behind ADP's `measurement` feature, the source-fed
  measurements, the `dogstatsd_codec` bench, the container scaling curves in `test/scaling/`) and its foundations
  (P9.F, each neutral at one lane: the bounded node acquire, `last_seen` as a maximum in the aggregation kernel,
  accumulated kernel and cache telemetry, sampled CPU time (every entry measured by default), the tap gate, the
  epoch-switch observation, shared kernel parts; see the plan's [P9.F as
  built](programmable-metrics-stages-7-and-9.md#84-stage-9-foundations-interfaces-step-and-parts)) are in place, and
  the rest is not built. Stages 8 and 10 are not built. Their
  extension-point types exist, only native defaults are accepted, and each unsupported case is a capability error that
  names a stage (see [Deferred work by stage](#deferred-work-by-stage)).

**Branch.** Eleven commits on top of `main` (`b940bf1f8b`, the merge base) before the stage 7 commit. They are local
only: the branch has no upstream and nothing is pushed. The spec commit is signed; the implementation and document
commits are unsigned (made with `--no-gpg-sign`, since the repository sets `commit.gpgsign=true`). No CI pipeline has
run on the branch.

| Commit | Stage | What it delivered |
|---|---|---|
| `e435160474` | spec | The specification. Unchanged since. |
| `f11cffe4a1` | 1 | `saluki-core` execution types (batch header, generation types, kernel trait). The plan crate: semantic model, IR with schema and versioning, canonical hash, predicates, operator registry with contracts, validator with duplicate analysis and catalog, reference interpreter, equivalence, explain, limits, extension-slot types, and hand-written default-program fixtures. |
| `3e058a556a` | 2 | Batch headers on buffers, admission clocks with reset epochs, admission points that stamp `t_a` and the generation in the dispatcher, heartbeats, blueprint snapshots and graph checks. `saluki-metric-kernels`: the native cores extracted, with frozen pre-extraction references and kernel-against-reference property tests. Lowering of native configuration (host facts, bindings, outputs, telemetry profiles), checked against the legacy blueprint. |
| `a4c72a4f2a` | demo | `saluki-metric-plan-syntax` (parser, printer, spans) and `programmable-metrics-demo` (launcher, stub, editor, recording intakes, telemetry viewer). Hardens `stele`'s v3 decoder. |
| `d991c4b923` | demo | Keeps the syntax crate's property-test input generation within a small stack. |
| `f6ef9cddcb` | 3 | The region host and driver, the frame with R1 and its cost model, placement, region programs, plan diff and kernel reuse, the I/O-free installer core, and the plan-driven ADP metrics path with a supervised installer. Fixes the blueprint's memory bounds omitting the last component added. |
| `4c64b4c522` | 4 | Endpoint routing, Multi-Region Failover, and autoscaling failover gateways lowered to routing operators, run as one memoized decision in `metrics_enrich`. Decisions, R2 to R4, R5a, R6, R7, and capacity semantics in the interpreter. |
| `26af093620` | 5 | Channel types, the stub, the export bundle, composition with correspondence and provenance, the artifact lifecycle (revision high-water mark, redelivery, revocation, stale base, last-known-good, panic budgets with rollback), the control surface, and region input capacities that keep native burst slack. The demo's integration with ADP (the `default-program` route, preview composition, plan diff) and its headless check. The `saluki-io` proxy TLS change. |
| `b01049372e` | 6 | Aggregation as a kernel in `dsd_agg`: admission-time bucketing, watermarks, heartbeats, progress, the tick gate, clock resets, lineages and transitions, the cut protocol, shared budgets, resumable drains, R5b as emission suppression, the `dogstatsd_pre_aggregation` scope, and allowed bucket widths. |
| `2729a33271`, `d7316c474f` | docs | This document's status after stage 6, and the stages 7 and 9 design with its second review. |
| (the stage 7 commit) | 7 | Resource generations: the encode host and the delivery host, recipient loops with drains, reopens, and live settings, the resource table and resource transactions, credentials, the resource budget, fingerprints version 2, pending and degraded artifacts, the `resources` product, the status and resources routes; stage 9's measurement harness (P9.0); the demo's scenarios S8 to S11. |
| (the P9.F commit) | 9 | Stage 9's foundations, neutral at one lane: the bounded node acquire and the budget recording, `last_seen` as a maximum in the aggregation kernel, accumulated aggregation, cache, resolver, mapper, and tag projection telemetry with the telemetry audit, sampled CPU-time accounting, the tap gate, `Observation::EpochSwitched` without the tracker clone, and `KernelSpec::with_shared`. |

**Next.** Stage 9's parts, designed in
[programmable-metrics-stages-7-and-9.md](programmable-metrics-stages-7-and-9.md) (section 8.4); stage 7's deferred
items are [below](#stage-7-resource-generations). Stage 8, the real artifact channel,
is not part of it beyond what stage 7 needs to settle who may declare endpoints. The owner's direction for this phase
(October 10, 2026):

- Stage 7: no full stage 8. Endpoint privilege may be settled by a configuration-driven change, or by declaring
  endpoints apart from ordinary program definitions. Security is not a priority yet, as long as nothing is
  unrecoverable. Other stage 7 questions take the path that least surprises operators and contributors, to revisit
  later if needed.
- Stage 9: the specification's stage 9 list is not binding. Partitioning is to be designed as a whole, from the state
  of the art and the constraints of this problem, to restore the parallelism that fusion gave up while adding as little
  coordination and communication as possible. If first-class parallelism makes R1's cost model unnecessary, it should
  go (see [what it couples to](#stage-9-partitioned-execution)).

## Documents

| Document | What it holds |
|---|---|
| [programmable-metrics.md](programmable-metrics.md) | The specification, authoritative for semantics. It is never edited without its author; refinements go to the proposed amendments. |
| [programmable-metrics-implementation.md](programmable-metrics-implementation.md) | The implementation design, stages 1 to 6 as built: [code layout](programmable-metrics-implementation.md#code-layout), [execution contracts](programmable-metrics-implementation.md#execution-contracts-in-saluki-core), [frame and region programs](programmable-metrics-implementation.md#physical-frame-and-region-programs) (R1 to R7), [the plan crate](programmable-metrics-implementation.md#the-plan-crate), [installation](programmable-metrics-implementation.md#installation), [ADP integration](programmable-metrics-implementation.md#adp-integration), [demo](programmable-metrics-implementation.md#demo), [stage plan](programmable-metrics-implementation.md#stage-plan) with stage 3 to 5 exit evidence, [stage 6](programmable-metrics-implementation.md#stage-6-stateful-kernels) with its [exit evidence](programmable-metrics-implementation.md#exit-evidence), [87 proposed spec amendments](programmable-metrics-implementation.md#proposed-spec-amendments), and [risks](programmable-metrics-implementation.md#risks). |
| [programmable-metrics-stub-protocol.md](programmable-metrics-stub-protocol.md) | The contract between ADP and the demo server: the poll, the envelope, the control surface routes, and the demo server's side. |
| This document | Status, how to run, changes outside the new code, open decisions, the amendments index, deferred work, and environment notes. |
| [programmable-metrics-stages-7-and-9.md](programmable-metrics-stages-7-and-9.md) | The next phase's design (being written; the link works once it is committed). |

Release notes, one per stage that changed what operators see:

- Stage 3: [`plan-driven-metrics-path-c5ebc95607a402c8.yaml`](../../releasenotes/notes/plan-driven-metrics-path-c5ebc95607a402c8.yaml)
  (its buffering item was rewritten in stage 5).
- Stage 4: [`plan-driven-metrics-gateways-8f5f8bebdc56b2e3.yaml`](../../releasenotes/notes/plan-driven-metrics-gateways-8f5f8bebdc56b2e3.yaml).
- Stage 5: [`programmable-metrics-artifact-channel-stub-f17b83af422ad285.yaml`](../../releasenotes/notes/programmable-metrics-artifact-channel-stub-f17b83af422ad285.yaml).
  Its sentence "Artifacts with stateful operators, such as aggregation, are rejected" stopped being true for the
  `dogstatsd_pre_aggregation` scope in stage 6; it still holds for `post_enrich`. Reword it if the notes ship in one
  release.
- Stage 6: [`plan-driven-dogstatsd-aggregation-b30cf0ea8aa14b5e.yaml`](../../releasenotes/notes/plan-driven-dogstatsd-aggregation-b30cf0ea8aa14b5e.yaml).

## Running it

### Prerequisites

- Rust as `rust-toolchain.toml` pins it (1.96.0), `jq`, and `protoc`; `make` checks all three
  (`check-rust-build-tools`).
- For tests and the gate: `cargo-nextest`, the nightly Rust the `Makefile` names (`RUST_NIGHTLY_VERSION`,
  `nightly-2026-01-18`, for `check-fmt`, `fmt`, and `generate-api-docs`), `cargo-sort`, `cargo-machete`, `cargo-hack`,
  `cargo-deny`, and `dd-rust-license-tool` (the `Makefile` installs these as needed), and `vale`.
- The demo needs no Datadog Agent. ADP runs in standalone mode, every listener is on 127.0.0.1, and nothing leaves the
  machine. TCP ports 17080, 17100 to 17105, 17110, and 17111 and UDP port 17125 must be free (each plus 10000 for the
  check).

### The demo

`make demo-programmable-metrics` builds ADP (`make build-adp`, profile `devel`) and the demo, then runs
`target/devel/programmable-metrics-demo run --adp-bin target/devel/agent-data-plane $(DEMO_ARGS)`. Open
`http://127.0.0.1:17080`. Ctrl-C, `SIGTERM`, or `SIGHUP` stops the demo and ADP (ADP gets `SIGTERM`, then `SIGKILL`
after 10 s or on a second Ctrl-C). A `SIGKILL` of the demo leaves ADP running in its own process group; before starting
ADP, the launcher names any port that is taken, which is usually such a leftover ADP.

- **Run directory**: `target/programmable-metrics-demo/run-<unix seconds>` by default. `DEMO_ARGS="--run-dir <dir>"`
  reuses one, which keeps the published artifact across a restart (scenario S7). It holds `ipc_cert.pem` (self-signed,
  owner-only), `datadog.yaml` (the whole ADP configuration: inherited `DD_*` variables are removed from ADP's
  environment), `adp.log`, `adp.console.log`, and `channel/` (the stub's store, `store.json` and `history.jsonl`).
- **Ports**, each with a flag (for example `DEMO_ARGS="--ui-port 18080"`):

  | Listener | Port | Flag |
  |---|---|---|
  | UI and JSON API | 17080 | `--ui-port` |
  | Artifact channel stub | 17100 | `--stub-port` |
  | Primary intake (`dd_url`) | 17101 | `--primary-port` |
  | Team B intake (additional endpoint) | 17102 | `--team-b-port` |
  | Team C intake (additional endpoint) | 17103 | `--team-c-port` |
  | Multi-Region Failover intake | 17104 | `--mrf-port` |
  | Team D intake (declared only by a resources artifact) | 17105 | `--team-d-port` |
  | ADP unprivileged API | 17110 | `--api-port` |
  | ADP privileged API (mutual TLS) | 17111 | `--secure-api-port` |
  | ADP DogStatsD (UDP) | 17125 | `--dogstatsd-port` |

- **Other flags**: `--poll-interval-ms` (1000), `--aggregate-window-secs` (1, so changes show within seconds; it must be
  an allowed width), `--counter-expiry-secs` (0, so a paused or rerouted counter leaves the columns at once; ADP's own
  default is 300), `--traffic-rate` (datagrams per second, 20), `--traffic-paused`, and `--no-launch` (serves the demo
  for an ADP started separately and prints its command line).
- **Configuration and traffic**: the [Demo](programmable-metrics-implementation.md#demo) section lists the generated
  ADP configuration. Traffic carries `demo.checkout.requests` (a counter) and `demo.checkout.latency` (a distribution)
  tagged `team:checkout`, `demo.catalog.views` (`team:catalog`), `demo.platform.queue_depth` (a gauge,
  `team:platform`), the 20 counters `demo.debug.0` to `demo.debug.19`, and the canary counter `demo.canary`, once per
  datagram. By default the primary column receives everything, team B `demo.checkout.*`, team C `demo.catalog.*`, and
  the failover column nothing.

**A visible change in about 30 seconds:**

1. The editor loads the default program once ADP serves its export (**Load default program** reloads it). It is the
   `post_enrich` scope: `input.metrics -> <policy node>`, then the policy `tee`, the routing `split`s, and the
   Multi-Region Failover gateway's `split`.
2. Replace the line `input.metrics -> <node>` with the following, keeping the original target in the `false` branch:

   ```text
   input.metrics -> drop_debug

   node drop_debug = split(name starts_with "demo.debug.") {
     true  -> drop
     false -> <node>
   }
   ```

3. Press **Compile** (Ctrl+Enter). The status reads that the artifact was composed and validated with no findings, and
   the plan diff lists the added nodes. Press **Publish** and approve.
4. Within about 3 s, `demo.debug.*` fades out of the primary column, the outcome under Publish reads that ADP runs it
   (with the generation), the acknowledgment timeline shows `active`, the per-node rates show `drop_debug`, and the
   canary check stays balanced.

Other edits: put `or tag("team") == "platform"` on its own line before the `) {` of the split that feeds team B (S2);
replace the failover split's predicate with `true and name in ["demo.checkout.requests"]` (S4); add a second branch
to the primary output in the `tee`, emit to `metrics_routing_encode_99`, or add an `aggregate` node to see ADP reject
each one while the previous generation keeps running (S5); **Revoke** restores the default routing (S6). Each scenario
edits the default program, so each one undoes the one before; to narrate one change at a time, revoke between steps or
keep editing the same program.

**The pre-aggregation scope** has no picker in the UI. Fetch its source with
`curl -s 'http://127.0.0.1:17080/api/default-program?scope=dogstatsd_pre_aggregation' | jq -r .source` and paste it
into the editor; Compile picks the scope from the source. To change the window, set both `dsd_no_agg_split`'s and
`dsd_agg`'s `window` to the same literal, such as `10s` (a `timestamp_split`'s window must equal its aggregation's,
amendment 83). The change installs at a cut, the first shared boundary at least 1 s after arming; watch
`plan_cuts_total{outcome}`. `POST /api/revoke` with `{"scope": "dogstatsd_pre_aggregation"}` revokes only that scope;
the **Revoke** button sends no scope and empties the whole snapshot. The headless check does not cover this scope; ADP's
lifecycle and cut tests do.

**Headless check.** `make demo-programmable-metrics-check` builds the same binaries and runs
`target/devel/programmable-metrics-demo check --adp-bin target/devel/agent-data-plane $(DEMO_CHECK_ARGS)`. Flags:
`--run-dir` (default `target/programmable-metrics-demo/check-<unix seconds>`), `--port-offset` (10000: the UI on
27080 and every other port moved up the same way, so it runs beside a demo), and `--traffic-rate` (20). When other
ports are taken, pass `DEMO_CHECK_ARGS="--port-offset <n>"`. It exits 1 on any failure,
naming the run directory, and writes `check-report.json` there with compile, publish-to-acknowledgment, and
publish-to-visible timings. Each step compiles, publishes, waits up to 20 s for the acknowledgment on the status route
and in the next stub poll, checks that ADP's active plan hash equals the plan the editor composed, then waits up to
30 s for every intake to receive exactly the expected families and hold them for 3 s. Throughout, the rolling canary
range must balance, and at the end the whole run's canaries are checked (both within an allowance of 6 at 20 per
second: they catch an aggregated canary point lost or doubled, not a few datagrams ahead of aggregation, which ADP's
continuous-input tests check exactly).

| Step | Edit | What it proves |
|---|---|---|
| S0 | The unmodified export. | Acknowledged `active` with no new generation and an empty diff; `/export` and `/default-program` hash as expected. |
| S1 | `drop_debug` ahead of the policy `tee`. | `demo.debug.*` leaves the primary intake only; `/plan` and `/explain` describe it. |
| S2 | Team B's route widened with `or tag("team") == "platform"`. | Team B also receives the platform family. |
| S3 | `split(tag("team") == "checkout")` ahead of the `tee`, `true` to team B only. | Exclusive routing: checkout families reach team B and not the primary intake. |
| S4 | Failover predicate `true and name in ["demo.checkout.requests"]`. | Mirroring: the failover intake receives only that family. |
| S5a to S5d | A duplicate branch to the primary output; an emit to an undeclared output; an `aggregate` without parameters; a full `aggregate` in the policy scope. | ADP rejects each one naming the problem (`duplicate_contribution`, the undeclared output, `valid_parameters`, and placement's "runs only in an isolation slot" for S5d, which the editor cannot flag because it does not place), and S4 keeps running. |
| S6 | Revoke (an empty snapshot). | The boot plan hash is active again and the default routing is back. |
| S8a | Publish team D's resources (one recipient, `resources_team_d`). | The resources config is `active`, and ADP declares the output. |
| S8b | Route `team:platform` to `resources_team_d`. | Team D's intake receives the platform family with team D's credential. |
| S9a | Revoke the resources. | The policy stays `active`, degraded: its emits to team D are dropped and counted as policy loss at the route's node; team D's recipient retires with a drain report. |
| S9b | Publish the resources again. | The emit returns, without a new policy revision. |
| S10a to S10c | Revoke both; publish the route before the resources; then the resources. | The route waits, `pending` ("waiting for output `resources_team_d`"), and installs once the output is declared. |
| S11 | Pause team D's intake, then revoke. | The recipient drains until its 30 s deadline and drops what it holds, and `dd_out`'s drop telemetry counts as many. |

A run took about 70 to 75 s through stage 6; with S8 to S11 it takes about 150 s (147.9 to 149.9 s in the four stage 7
runs, S11's deadline included). About 18 s go to waiting at the start for a whole canary range behind ADP's start, and
about 10 s at the end for the rolling range to cover the last change. The check was added in stage 5 and passed in every
recorded run of stages 5 to 7, deliberate mutation runs aside; stage 7 ran it twice on one run directory, the second
time with the first's `--run-dir`, before and again after its last code change.

### The stage gate

Each stage gate ran the commands below with the packages each stage touched (stage 6's list shown). This is not
`make check-all` plus `make test-all`, which the [stage plan](programmable-metrics-implementation.md#stage-plan) states
inaccurately; see [Known limitations and risks](#known-limitations-and-risks) for what it leaves to CI.

```sh
cargo check --workspace --tests
make check-fmt
make check-clippy
PKGS="saluki-core saluki-io saluki-metric-kernels saluki-metric-plan saluki-components agent-data-plane \
  agent-data-plane-config agent-data-plane-config-system saluki-metric-plan-syntax programmable-metrics-demo"
for p in $PKGS; do
  # On macOS in a sandbox, add --test-threads 4 for saluki-components and saluki-io (see Environment notes).
  cargo nextest run -p "$p" --no-fail-fast -E 'not test(/property_test_/)'
done
for p in $PKGS; do
  cargo nextest run -p "$p" --release --no-fail-fast -E 'test(/property_test_/)' --no-tests=pass
done
make check-unused-deps
make check-features    # stage 6 also ran: cargo hack --feature-powerset --package <p> check --tests, per package
make check-licenses
make check-deny
make generate-api-docs
vale --minAlertLevel=error docs/design/programmable-metrics-implementation.md lib/saluki-metric-plan \
  lib/saluki-metric-kernels lib/saluki-core/src/execution
```

The last gate before the stage 6 commit passed every step but `nextest-saluki-io`, whose two failures are proxy
tests that fail in a sandbox (see [flaky tests](#known-limitations-and-risks)); `--test-threads 4` for `saluki-io`
was added to the script only afterwards. Stage 6 also ran `make demo-programmable-metrics-check` twice, the
release-note check on its note (`ci/tooling/release_notes.py check`; `make check-release-notes` passes at
`b01049372e`), `RUSTDOCFLAGS='-D warnings' cargo doc -p saluki-metric-plan -p saluki-metric-kernels -p saluki-core
--no-deps --features saluki-metric-plan/test-util`, `cargo nextest run -p saluki-metric-plan` with and without
`--features test-util` (622 tests each at integration; 630 with `test-util` after the review fixes), and the
measurements and benches below.

### Focused tests and golden files

```sh
cargo nextest run -p saluki-metric-plan -E 'test(/compile::|install::|runtime::|interpret::/)'
cargo nextest run -p saluki-metric-plan --features test-util     # and without: both must pass
cargo nextest run -p saluki-core -E 'test(/execution::/)'
cargo nextest run -p saluki-metric-kernels
cargo nextest run -p agent-data-plane -E 'test(/programmable_metrics::/)'      # every ADP plan test (slow)
cargo nextest run -p agent-data-plane -E 'test(/programmable_metrics::(installer|channel_stub|api)/)'
cargo nextest run -p agent-data-plane -E 'test(/burst_tests/)'
cargo nextest run -p programmable-metrics-demo -p stele -p saluki-metric-plan-syntax

# Stage 7: real encode and delivery hosts against recording intakes (live changes, faults, differential, reclamation).
cargo nextest run -p agent-data-plane -E 'test(/delivery_tests|telemetry::tests::the_hosts_report/)'
cargo nextest run -p saluki-components --test-threads 4 \
  -E 'test(/common::datadog::(delivery|io)|encode_host|host::tests/)'
# The 1,000-cycle reclamation test (about 11.5 minutes):
cargo nextest run -p agent-data-plane --run-ignored only -E 'test(/a_thousand_cycles_of_adding/)'

# Rewrite golden files, then review the diff line by line:
SALUKI_METRIC_PLAN_UPDATE_EXPLAIN=1 cargo nextest run -p saluki-metric-plan -E 'test(/explain/)'
SALUKI_METRIC_PLAN_UPDATE_SCHEMA=1 cargo nextest run -p saluki-metric-plan -E 'test(/checked_in_schemas_are_current/)'
```

Golden files are change detectors only; each one is backed by separate hand-derived checks.

### Property tests, benches, and measurements

```sh
# Property tests, release only (stage 6: 58 tests in these four crates). CI runs them through make test-property.
cargo nextest run -p saluki-metric-plan -p saluki-metric-plan-syntax -p saluki-metric-kernels -p saluki-core \
  --release -E 'test(/property_test_/)'

# Criterion benches. The plan crate's declare required-features = ["test-util"]: without it, cargo skips them.
cargo bench -p saluki-metric-plan --features test-util --bench region_driver
cargo bench -p saluki-metric-plan --features test-util --bench selection   # selection_default_program: stage 4 gate
cargo bench -p saluki-core --bench region_host

# Ignored measurement tests: release, an idle machine, one at a time.
cargo nextest run -p agent-data-plane --release --run-ignored only -E 'test(/measurement_tests/)' \
  --no-capture --test-threads 1
```

Saved property-test regressions are committed in the new crates' `proptest-regressions/` directories. The five
measurement tests compare plan against legacy on one and four workers, with and without `GATEWAYS`, and legacy
against itself as the noise check; the tolerances are 7% throughput, 7% CPU and allocated bytes, and 10% p99, as
medians of five rounds. Machine load above about 4 makes legacy fail its own tolerances. To run only the gateway
configuration and the noise check, filter with `-E
'test(/measurement_tests::(with_gateways|the_legacy_topology_against_itself)/)'`.

Stage 7's gate ran them with P9.0's changes (process CPU, real encoders) on an 18-core Mac at load about 2.2 to 3.7.
The noise check passed (a first run at load 2.5 to 3.9 failed on paced p99, 1.176, which is noise), and all four
plan measurements passed: saturated throughput 1.11 to 1.21 of legacy's, process CPU per metric at most 1.011,
allocated bytes 0.56 to 0.89, paced p99 0.27 to 0.65. `cargo bench -p saluki-components --bench encode_host`: one
lease against native `DatadogMetrics` per metric, V2 series 0.4% and 2.0% slower in two reruns (a first, noisy run
4.9%), V3 series 0.5%, sketches 1.2%; per buffer (`per_buffer`) 3.67 µs against 3.64 µs, 29 ns more.

## Code map

The [code layout](programmable-metrics-implementation.md#code-layout) maps every module with the stage that added it,
and gives the dependency direction. This section is the orientation that layout leaves out.

| Crate or module | Role | Start reading at |
|---|---|---|
| [`lib/saluki-core/src/execution/`](../../lib/saluki-core/src/execution) | Generic execution contracts with no metric knowledge: batch header, admission clocks, points, schedules and registry, generations and tokens, the `Kernel` trait, `StateBudget`, and the generic region host. | `region/host.rs` (`RegionHostBuilder` and the transform it builds), `region/driver.rs` (`RegionDriver`), `admission.rs`, `kernel.rs`, `budget.rs` |
| [`lib/saluki-metric-kernels/`](../../lib/saluki-metric-kernels) | Native metric semantics, extracted once and shared by the legacy components and the plan. | `aggregate/` (`AggregateCore`, `AggregateKernel`), `matcher.rs`, `params/` |
| [`lib/saluki-metric-plan/`](../../lib/saluki-metric-plan) | IR, operators and contracts, validator, interpreter, compiler (frame, placement, rewrites, decisions), region driver, installer core, channel, composition, export. No I/O. | `compile/frame.rs` (`Frame::compute`), `runtime/driver.rs` (`MetricRegionDriver`), `install/core.rs` (`InstallerCore`, `Action`), `interpret/mod.rs` (`Interpreter`) |
| [`lib/saluki-metric-plan-syntax/`](../../lib/saluki-metric-plan-syntax) | Surface syntax for tools only; nothing that runs plans may depend on it. | `parse`, `print` |
| [`bin/agent-data-plane/src/programmable_metrics/`](../../bin/agent-data-plane/src/programmable_metrics) | ADP integration: host facts, bindings, catalog, outputs, lowering, the plan topology, the installer worker, the stub worker, the control surface, the snapshot bridge, and the test topology. | `installer.rs` (`ProgrammableMetrics::boot`, `InstallerWorker`), `lowering/mod.rs` (`Lowering`), `topology.rs` (`PlanTopology`) |
| [`lib/saluki-components/src/common/datadog/delivery/`](../../lib/saluki-components/src/common/datadog/delivery), [`encoders/datadog/metrics/host.rs`](../../lib/saluki-components/src/encoders/datadog/metrics/host.rs) | Stage 7's hosts: the delivery host (routing loop, recipient table, input barrier, withdrawal, reopen, client switch, credential followers, validation) and the multiplexed encode host (leases, shards, drain markers); the recipient loops are in `common/datadog/io.rs`. | `delivery/router.rs` (`Router`), `delivery/host.rs` (`DeliveryHostConfiguration`), `host.rs` (`MetricsEncodeHost`), `io.rs` (`RecipientLoop`) |
| [`bin/agent-data-plane/src/cli/run.rs`](../../bin/agent-data-plane/src/cli/run.rs) | Topology assembly: `MetricsWiring` hooks, the workers on the internal supervisor; the legacy path in `run/legacy.rs` (tests only). | `handle_run_command`, `create_topology` |
| [`bin/programmable-metrics-demo/`](../../bin/programmable-metrics-demo) | Demo server, embedded UI, recording intakes, traffic, preview, launcher, and the headless check. | `app.rs` (routes), `scenario/mod.rs` (S0 to S6) |

**At runtime:**

- **Boot.** `handle_run_command` calls `ProgrammableMetrics::boot`, which captures `HostFacts` (host tags are fetched
  here, once), lowers with `Lowering::new` and `lower`, validates (a default program that fails validation runs
  degraded), computes the frame with `Frame::compute`, and creates `InstallerCore::new` with the boot generation. Then
  `create_topology` builds the metrics path from the frame through the `MetricsWiring` hooks, and `run.rs` adds the
  control surface (`ControlSurfaceWorker`), the stub worker (`StubChannelWorker`, only with a stub URL), the snapshot
  bridge (when aggregation runs in a region), and the installer worker (`into_installer`) to the internal supervisor.
- **Per batch.** A source's dispatcher stamps each buffer's header (`t_a`, epoch, generation from the shared schedule)
  under its admission point's lock, the region host (the transform `RegionHostBuilder` builds) reads it, and
  `MetricRegionDriver::process` runs the program of the
  generation the header carries, writing to exits that the host dispatches in order.
- **Configuration change.** The installer worker sees `ConfigChanged`, lowers again, and passes the result to the core
  (`on_default_program`). The core's actions prepare every region (`RegionControl::Prepare`), wait for every
  acknowledgment, commit with one store of the schedule, and release programs after the old generation retires. When
  the lowering changes resources (stage 7), the core first runs a resource transaction (`PrepareResources` to each
  delivery host whose publication differs, then `PublishResources` with the native set, settings, and withdrawn set),
  prepares the generation's leases and recipients with its regions (`PrepareSite::Host`), and, after the old
  generation retires, drains its leases (`ReleaseLeases`) and then releases the recipients no generation uses
  (`ReleaseRecipients`), whose exits the delivery hosts report on their event channels.
- **Cut** (a state owner's transition that cannot share its instance). `Action::ArmCut` arms it in one transaction
  under the affected points' locks, `WatchCut` polls until a point stamps the new generation (then the cut is
  irrevocable), and `FinalizeCut` completes it once every affected point has crossed.
- **Artifact.** The stub worker polls, publishes snapshots on the `ArtifactFeed`, and the installer passes each to the
  core (`on_snapshot`), which checks receipt, composes into the default program, and proposes it like a configuration
  change; acknowledgments go back with the next poll (`apply_states_with_bases`).

**Invariants to keep** (each is stated where it is built, in the implementation design):

1. Generations are assigned only at admission points. Regions never choose one; a batch runs whole under the
   generation its header carries.
2. A commit is one store of the schedule; every point switches at its next stamp.
3. A cut's instant `T` is on the new generation's token before publication. A cut can be cancelled only until some
   point stamps the new generation.
4. State owners run only in isolation slots, each fed by exactly one admission point (until stage 9).
5. The physical frame is fixed at boot. Candidates place into it; anything needing a new slot, edge, output, or host
   component is a capability error (stage 7).
6. Native component IDs and series survive (slot names, `component_id` tags, accounting paths under the slot); only the
   fused components' own component series go.
7. Panic budgets cover artifact nodes only. A panic under the default program fails the component, as natively.
8. Rewrites never increase capacity loss under the interpreter's delivery order, which is the oracle (one pass per
   trace event, in topological order). Native filters move only whole (R5a, R6); R5b is emission suppression.
9. Artifacts emit only to outputs the base declares, with matching fingerprints; host components never come from
   artifacts; only composition writes provenance; revisions strictly increase across scopes.
10. A region's input holds as many buffers as the queues it replaces and their in-flight buffers did.
11. Dead legacy code goes behind `cfg(test)`, never `#[allow(dead_code)]`.

**Names worth knowing.** Slots keep native IDs: `dsd_pre_aggregation`, `dsd_agg` (the isolation slot), and
`metrics_enrich`; a chain R1 cuts gets `<first node>_region` slots such as `dsd_prefix_filter_region`,
`dsd_no_agg_split_region`, and `dsd_post_agg_filter_region`. Region input capacities with defaults: 515
(`dsd_pre_aggregation`), 257 (`metrics_enrich`), 386 (`metrics_enrich` with Multi-Region Failover), 128 (`dsd_agg`).
Limits and their decision-log notes are in [`limits.rs`](../../lib/saluki-metric-plan/src/limits.rs). In method
syntax, `FixedSizeEventBuffer`'s inherent `header_mut` (returning `&mut BatchHeader`) shadows the trait method
`Dispatchable::header_mut` (returning an `Option`).

**Fixtures and tests.** The hand-written default programs are in `lib/saluki-metric-plan/fixtures/`, with the exports
in `fixtures/export/` and the explain golden files in `fixtures/explain/` (`default-program-stage3*` keep the stage 3
form, whose gateways are opaque hosts); the checked-in JSON schemas are in `lib/saluki-metric-plan/schema/`. ADP's
lowering matrix is `programmable_metrics/lowering/matrix.rs` (`MATRIX`), with `lowering/fixtures/every-branch.json`;
the demo's export fixture is `bin/programmable-metrics-demo/fixtures/export-post-enrich.json`. The ADP test topology
(`test_topology.rs`) replaces I/O with test doubles in place through `TopologyBlueprint::replace_*` and runs the
differential, live-change, fault, lifecycle, telemetry, burst, and measurement tests. The frame harness
(`runtime::harness::FrameHarness`, `test-util`) runs whole frames of region drivers in process.

## Changes users can see

The release notes are the operator-facing record. This list adds what they leave out, marked **not release-noted**.

**Configurations that now fail at startup:**

- `aggregate_window_duration_seconds` other than 1, 2, 3, 4, 5, 6, 10, 12, 15, 20, 30, 60, 120, 180, or 300, including
  0. The error names the nearest width. This applies with or without artifacts (stage 6).
- `aggregate_context_limit` or `dogstatsd_mapper_cache_size` above 2^53−1, and some durations above 2^53−1 seconds
  (stage 3, amendment 14).
- A `stub_url` that is not `http://` on a loopback address, has a query, or a zero `poll_interval` (stage 5; only when
  the experimental keys are set).
- The reverse, **not release-noted**: a host with no platform CA certificates and no proxy used to fail at startup
  ("no native root CA certificates found") and now starts; HTTPS endpoints then fail at the handshake. See
  [the root-certificate change](#the-root-certificate-change).

**Behavior:**

- Live changes to the metric filterlist, the tag filterlist, and Multi-Region Failover install as generations (stages 3
  and 4).
- Timestamped passthrough metrics go out with their batch; `aggregate_passthrough_idle_flush_timeout` has no effect,
  and the native loss at shutdown is gone (stage 3, amendment 12).
- The host-tags deadline counts from process start, including startup waits for the Agent (stage 3, amendment 9).
- Region inputs hold 515 and 257 buffers where native components held 128 each, so a burst backs up to the source
  exactly as natively (stages 3 and 5).
- Aggregation buckets by receive time, flushes at the tick once the watermark arrives (waiting up to 2 s), absorbs
  backward clock jumps up to 300 s and resets on larger ones, emits a repeated histogram statistic once, and changes
  window live through artifacts at a cut (stage 6).
- **Not release-noted:** order within a batch is now preserved where native filters reordered survivors
  (`swap_remove_back`), so a gauge context sampled twice in one batch after a filter dropped a metric can report a
  different last sample (amendment 2).
- **Not release-noted:** reordering a set-typed filterlist no longer counts in `metric_filterlist_updates_total` or
  `tag_filterlist_updates_total`, which now compare resolved values.
- **Not release-noted:** with several configuration errors, metrics-path errors are reported first, and host tags are
  fetched before topology assembly (amendment 38). With one error, the error is the native one.
- **Not release-noted:** the memory bounds of every topology now count the last component added (a `saluki-core` fix,
  named only in the stage 3 commit message), and `EventsBuffer` grew from 40 to 80 bytes, adding 40 bytes per channel
  slot to every pipeline's interconnect bounds, logs, traces, and events included.

**Stage 7** (release-noted unless marked): endpoints, routing groups, failover, keys, proxies, and delivery settings
change live, and logs, events, service checks, and traces follow `additional_endpoints`; a removed endpoint's payloads
still on their way are dropped and counted from the transaction that withdraws it; restart-only settings are the ones
other encoders share, plus the storage path and whether persistence is on; the idle failover hosts; the shared input
of an encode host's outputs; the 4.15 task and series changes; the `resources` product; validation of endpoints added
live; two static-mode telemetry fixes; the primary endpoint and the TLS and connection settings are live, the Cluster
Agent endpoint restart-only (autoscaling failover and the Cluster Agent settings apply live only when one resolved at
startup), and a queue bound raised live is not checked against the memory limit until a restart;
`plan_recipient_retirements_total{outcome:persisted}`. **Not release-noted:** `/programmable-metrics/status` gains a
`recipients` member and `/programmable-metrics/resources` is new (the experimental stub protocol documents both); API
keys are validated once per change of the native endpoints, in the native order, where a forwarder's startup
validation ran once; a recipient's `delivery.queue_bytes` above the host's bound is capped rather than refused; a
configuration change whose preparation keeps failing shows in `last_error` and the status summary's degraded reason;
a payload no recipient of a host's native set takes is counted in `component_errors_total{error_type:http_send}`; a
primary endpoint's recipient identity spells its endpoint resolved (`dd_out/primary/0/http://h:p`, without a trailing
slash), so a respelling is the same recipient.

**Telemetry** (all release-noted): the fused components' and gateways' component, health, and task series are gone;
`metrics_enrich` and `dsd_agg` report sent counts per output instead of `_default`; accounting paths nest under the
slot; `dsd_agg` no longer counts sent metrics twice, no longer emits what R5b suppresses, and counts one extra flush
per cut and per reset; `aggregate_passthrough_flushes_total` counts incoming batches, and the passthrough batcher's
`aggregate_passthrough_batch_duration_secs` and `buffer_full` drops are gone. New series: `plan_active_generation`,
`plan_live_generations`, `plan_installs_total`, `plan_node_events_total`, `plan_loss_total`, `plan_rewrites_applied`,
`plan_active_revision`, `plan_cuts_total`, `plan_cut_cancellations_refused_total`, `plan_state_watermark_lag_seconds`,
`plan_state_lineages`, `plan_clock_resets_total`, and `plan_state_tick_grace_expired_total`.

**New surfaces** (experimental, release-noted): the `experimental.programmable_metrics.artifact_channel.{stub_url,
poll_interval}` keys, the privileged routes `/programmable-metrics/{status,export,plan,explain,default-program}`, and,
in stage 7, `experimental.programmable_metrics.resources.credentials`, the stub's `resources` product, and
`/programmable-metrics/resources`.

Native quirks are preserved on purpose and pinned by fixtures; see
[native behavior: preserved and changed](programmable-metrics-implementation.md#native-behavior-preserved-and-changed)
and the issues never filed under [Follow-ups](#follow-ups-not-tied-to-a-stage).

## Changes outside the new code

New code is `lib/saluki-metric-kernels`, `lib/saluki-metric-plan`, `lib/saluki-metric-plan-syntax`,
`bin/programmable-metrics-demo`, `bin/agent-data-plane/src/programmable_metrics`, and `lib/saluki-core/src/execution`.
Outside them the branch touches 71 paths (`git diff --name-status -M main..HEAD`): three files renamed into the
kernels crate, the three design documents and four release notes under [Documents](#documents), and the code and
configuration below. It adds no third-party crate: `Cargo.lock` gains the four workspace packages and edges between
existing ones, so `LICENSE-3rdparty.csv`, `deny.toml`, and CI configuration are untouched.

Classes: **permanent** (an improvement that stands on its own), **integration** (required by the new code, permanent),
**workaround** (temporary), **test seam** (`#[cfg(test)]` or a `test-util` feature).

| Where | What changed | Why | Class |
|---|---|---|---|
| `Cargo.toml`, `Cargo.lock` | Four workspace members. `saluki-components` drops `float-cmp`, `foldhash`, `hashbrown`, `smallvec` (moved with the cores); `saluki-core` gains `arc-swap`; ADP gains `rustls` and dev-dependency `axum`. | New crates. | integration |
| `Makefile` | `demo-programmable-metrics` and `demo-programmable-metrics-check`. | Demo entry points. | permanent while the demo exists |
| `.vale/.../accept.txt` | About 28 terms. | Keeps Vale clean on the new prose and doc comments. | integration |
| ADP `main.rs` | Passes `started_at: SystemTime` to `handle_run_command`. | Time parameters from wall-clock process start (amendment 9). | integration, behavior change |
| ADP `cli/run.rs`, `cli/mod.rs` | The metrics path is always plan-driven, through `MetricsWiring` hooks; new workers; metric builders made `pub(crate)` and return `MetricsBranch`. | Stages 3, 5, 6. | integration |
| ADP `cli/run/legacy.rs` (new) | The legacy metrics path, `#[cfg(test)]`. | Differential reference. | test seam, removable once differential and measurement tests retire |
| ADP `components/` | `dogstatsd_filterlist` deleted and `metric_name.rs` moved into the kernels crate; `dogstatsd_no_agg_split`, `dogstatsd_post_aggregate_filter`, `dogstatsd_prefix_filter`, `metric_filter`, and the `tag_filterlist` and `host_tags` transforms compile only for tests; `fetch_host_tags` and the tag rule conversions stay. | One implementation of each native rule. | integration and test seam |
| ADP `dogstatsd_contexts/api.rs`, `internal/control_surfaces.rs` | `#[cfg(test)]` accessors. | Context-dump tests through the snapshot bridge. | test seam |
| `agent-data-plane-config` `control.rs`, `defaults.rs` | `programmable_metrics.artifact_channel.{stub_url, poll_interval}` (1 s default). | The stub channel. | integration; the keys are temporary until a real channel replaces the stub |
| `agent-data-plane-config` `domains/dogstatsd.rs` | Tag-value allowlist validation removed. | Moved to `saluki_metric_kernels::params`. | integration |
| `agent-data-plane-config-system` `saluki_only.rs` | Validation delegates to the kernels crate; window widths at deserialization; the stub keys. | Stages 2, 5, 6. | integration, behavior change (widths) |
| `config-overlay-model` `saluki_keys.rs`; generated `configuration.md` | The two keys and the window's documentation. | Key inventory. | integration |
| `saluki-components` aggregation, mapper, autoscaling gateway, endpoint routing | Cores moved to the kernels crate; transforms became adapters; frozen references under `aggregate/reference/`; aggregation's snapshot request API; `AggregateConfiguration::with_clock` (`test-util`). | Extraction; the `dsd_agg` snapshot bridge; clocked legacy reference. | integration and test seam |
| `saluki-components` forwarders, `common/datadog/` | `metrics_recipients()` on the Datadog and Cluster Agent forwarder configurations; `should_route_to_endpoint` shared. | Recipient identities and output fingerprints come from the forwarder itself, so they cannot drift (amendments 13, 41). | integration |
| `saluki-core` interconnect (`event_buffer.rs`, `dispatcher.rs`, `mod.rs`) | Inline `BatchHeader`; `with_capacity`, order-preserving `retain`, `move_into`, `reserve`; stamping under one mutex per admission point. `EventsBuffer` grows from 40 to 80 bytes. | Headers travel with batches. | integration (a cost on every buffer) |
| `saluki-core` `blueprint.rs`, `graph.rs`, `built.rs`, `snapshot.rs`, `ids.rs` | Admission points, per-component input capacity, snapshots; `GraphError::DuplicateEdge`; cycle check from every component; `replace_*` (`test-util`). | Introspection and burst parity. | integration; the cycle check and the bounds fix (below) are permanent improvements; `replace_*` is a test seam |
| `saluki-core` `blueprint.rs` bounds | Bounds recalculated after inserting a component. | The last component added was never counted. | permanent (bug fix) |
| `saluki-core` data model, `health/mod.rs`, `benches/region_host.rs` | `ContextKey::as_u64`, `Context::key`, `SetPoints::members`; health test helpers; a bench. | Memo keys and set comparison; region health tests. | integration; test seam (health) |
| `saluki-metrics` `test.rs` | `TestRecorder` accessors. | Telemetry parity tests. | test seam |
| `stele` `metrics.rs` | v3 decoder hardening: capacities capped, wrapping delta decode, checked ranges. | A 20-byte malformed payload aborted the demo. Could ship on its own. | permanent (bug fix) |
| `saluki-io` `client.rs` | The proxy connector's TLS is built only when proxies are configured. | See below. | **permanent** |

### The root-certificate change

**What changed.** `HttpClientBuilder::build`
([`client.rs`](../../lib/saluki-io/src/net/client/http/client.rs), stage 5) used to build
`hyper_http_proxy::ProxyConnector::new(connector)` for every client. It now builds it only when the proxy list is
non-empty, and `ProxyConnector::unsecured(connector)` otherwise. `ProxyConnector::new` (with `rustls-tls-native-roots`)
reads the platform's root certificates on every client build and fails when it finds none; `unsecured` reads nothing.

**Why.** In stage 5, tests that build an HTTP client (the stub client, the control surface, the demo, the lifecycle
suite's stub test, and 22 existing `saluki-components` tests) failed under full-workspace parallel nextest on macOS in
a sandbox: the macOS trust-settings reads failed (OS error -36) after about 24 to 30 s. The reads were for a TLS
configuration that nothing uses without proxies: `hyper-http-proxy` reads it only after a request matches a proxy, and
the Datadog forwarder passes an empty proxy list when none is configured.

**Verdict: a permanent improvement, not a workaround.** It removes a redundant per-client platform read without
changing any connection: clients with proxies, HTTPS destinations through a proxy, FIPS builds, and custom CA settings
behave as before. The one observable change is the zero-certificate host above, which is arguably closer to the
Agent's behavior (Go fails at the handshake). It could ship as its own pull request. What remains a workaround is only
the local `--test-threads 4` in the gate (see [Environment notes](#environment-notes)).

**Two corrections and an option.**

- The new code comment says the proxy connector's TLS "is used only to reach a proxy over HTTPS": in fact it is used
  for the TLS session to an HTTPS destination inside a CONNECT tunnel, through any proxy; reaching an `https://` proxy
  goes through the inner connector with saluki's TLS. The existing `TODO(fips)` comment has the same confusion. The
  decision to skip it is still correct.
- [Risks](programmable-metrics-implementation.md#risks) ("Stub tests under load") repeats the inaccurate wording, and
  the stub worker paragraph in [ADP integration](programmable-metrics-implementation.md#topology-construction-installer-stub-control-surface)
  still says that building any `HttpClient` loads the platform's certificates.
- Optional, not done: build the connector `unsecured` and give it the client's own rustls configuration with
  `ProxyConnector::set_tls`. Tunnelled TLS would then honor saluki's root store, minimum version, and FIPS checks
  (resolving the `TODO(fips)`), and proxied clients would stop reading platform certificates. That changes behavior for
  proxied clients and needs its own pull request and release note.

## Decisions awaiting the owner

| Decision | State | What is needed |
|---|---|---|
| **Clock-reset threshold** | 300 s, the specification's minimum (was 600 s), in `limits::values::CLOCK_RESET_THRESHOLD`, amendment 64, and the stage 6 release note. It halves the longest freeze of an absorbed backward jump. It was chosen pending the owner's confirmation, which never came. | Confirm 300 s, or revert: one constant plus the release note and amendment text. |
| **Amendments needing a decision** | 15 of the 87: 9, 12, 14, 20, 47, 49, 51, 52, 62, 64, 65, 66, 71, 76, 83 (see the [index](#spec-amendments-index)). | A value, a decision-log entry, wording to adopt, statements to withdraw, or an operator-visible break to accept. |
| **Decision-log entries** | `limits.rs` has "Decision log" notes for `HEARTBEAT_PERIOD` (1 s, was 5 s), `CLOCK_RESET_THRESHOLD`, `PROGRESS_PERIOD` (100 ms), `TICK_GRACE` (2 s), `CUT_ARM_ATTEMPTS` (10), and `MAX_TRANSITION_LCM` (300 s). The drain deadline (30 s) and the overlap budget (4), which amendment 20 says need entries, have none, nor does `ARTIFACT_MEMORY` (64 MiB). | Accept the noted entries into the specification's decision log, and decide entries for the three without notes. |
| **Allowed bucket widths** | Other `aggregate_window_duration_seconds` values now fail startup (amendment 83, upgrade release note). | Accept the break. |
| **Intentional changes against native** | Release-noted and recorded as decided: 9 (host-tags deadline), 12 (passthrough timeout without effect), 14 (values above 2^53−1 fail boot), 26 (statistics once), 82 (kind conflicts counted as loss). | Confirm. |
| **Antithesis panic hook** | `initialize_antithesis` in ADP's `main.rs` reports every panic as `unreachable!`, including artifact panics a region survives within its budget (3 per 60 s, 5 in all), so an Antithesis run reports a failed property for designed behavior. Stage 3 deferred this to stage 5, which did not decide it; the [execution contracts](programmable-metrics-implementation.md#region-host) text still says stage 5 must. | Keep it, or report contained panics with `sometimes!` (or a flag on the panic). |
| **Strong `GenerationRef` in `Prepare`** | A region that stops reading its control mailbox keeps every preparation sent to it alive; that is what bounds the unbounded mailbox (retries stop at the overlap budget), pinned by a fault test. | Keep it, or carry a `Weak` in the message and bound the mailbox. |
| **Release-note gaps** | The items marked not release-noted under [Changes users can see](#changes-users-can-see), and the stale stage 5 sentence on stateful artifacts. | Decide which to note before the notes ship. |
| **Capability stage numbers** | Placement maps `MissingCapability::State` to stage 6, the frame names stage 10 for a state owner without an isolation fence, and the interpreter names stage 7 for a cut or reset with state in a host component or without a fence. | Settle the numbering in the stage 7, 9, and 10 plans. |
| **Stage 7 changes against native** | Amendments 135 (a burst reaching several outputs of one encode host backs up sooner: 451 batches where native held 515 with one routing group) and 137 (a removed endpoint loses what its old generation's leases still build after the transaction that withdraws it), both release-noted. | Accept, or size the encode host's input by its boot leases (135), or withdraw after the generation without the recipient commits (137). |
| **Stage 7 deferrals** | Static mode stays the native forwarder (not folded into the delivery host); platform roots load per client build; failures stay text-matched to declared resources; a declared recipient's directory does not include its name; growth in place is not admitted; 4.13's validation errors are caps at run time. See [Stage 7](#stage-7-resource-generations). | Decide which belong in a stage 7 follow-up. |
| **The Core Agent and the credentials key (A36)** | ADP follows `experimental.programmable_metrics.resources.credentials` on the configuration stream over the boot base (pinned by `resources_credentials_follow_the_agent_stream_over_the_boot_base`); whether the Core Agent streams a Saluki-only map key is outside ADP. | Confirm with the Agent, or document the key as restart-only in connected mode. |
| **The specification's Vale errors** | The specification, which only its author edits, adds 12 errors to `make check-docs`, a CI job (see [Environment notes](#environment-notes)). | Before CI runs: add the 11 flagged words to the Vale vocabulary, and edit or exempt the hyphenated `Exactly-once`. |

## Spec amendments index

The 87 proposals are in [proposed spec amendments](programmable-metrics-implementation.md#proposed-spec-amendments),
numbered in order: 1 to 41 from stages 1 and 2, 42 to 48 from stage 3, 49 to 52 from stage 4, 53 to 61 from stage 5,
and 62 to 87 from stage 6; the next is 88. Nothing in the specification is edited without its author. The theme and
kind columns are this document's classification. [Native behavior](programmable-metrics-implementation.md#native-behavior-preserved-and-changed)
lists four more corrections to the specification's compatibility matrix from the stage 6 research: the histogram row,
the shutdown row, the expiry row, and the timestamped row.

- **Themes:** EXE execution (admission, generations, time); FRM frame, fusion, rewrites; NAT native parity; IR IR,
  predicates, validation; INS installation and lifecycle; ART artifacts and channel; TEL telemetry; STA state (stage
  6); VER verification.
- **Kinds:** C clarifies the specification; N restates it to match native behavior; D changes the design or a rule; L
  defers to a later stage.
- **Owner:** "yes" means a decision beyond approving the text.

| # | Short title | Theme | Kind | Owner |
|---|---|---|---|---|
| 1 | Native components never restart; only artifact regions do | INS | N | no |
| 2 | Native filters reorder within a batch; regions keep order | NAT | N | no |
| 3 | Binding liveness follows native | NAT | N | no |
| 4 | Node and component IDs keep native names | TEL | C | no |
| 5 | Physical frame fixed at boot; candidates place into it | FRM | L | no |
| 6 | Correspondence by reserved IDs | ART | C | no |
| 7 | Admission per component output; opaque hosts readmit | EXE | C | no |
| 8 | Starved branches carry header-only progress | EXE | C | no |
| 9 | Host-tags deadline is process start plus duration | NAT | D | yes: accept that startup waits consume `expected_tags_duration` |
| 10 | Client telemetry output taps the post-aggregation filter | NAT | N | no |
| 11 | Native aggregation quirks pinned by fixtures | NAT | N | no (the quirks were never filed; see follow-ups) |
| 12 | Passthrough emitted per input batch | NAT | D | yes: `aggregate_passthrough_idle_flush_timeout` has no effect; deprecate it? |
| 13 | Recipient identity is native; duplicate delivery kept | NAT | N | no |
| 14 | Invalid default program runs degraded; huge integers fail boot | INS | D | yes: accept boot failure above 2^53−1 |
| 15 | Gateways only where native wiring builds them | NAT | N | no |
| 16 | Kernels may own native cache-expiry tasks | EXE | C | no |
| 17 | No configuration version; changes numbered per process | INS | C | no |
| 18 | Predicates add `const`, `flag`, `is_empty`, `matches` | IR | D | no |
| 19 | Fused kernels' accounting nests under their slot | TEL | C | no |
| 20 | Behavior-changing limits need decision-log entries | INS | C | yes: drain deadline and overlap budget entries |
| 21 | Canonical numbers: integers within ±(2^53−1) | IR | C | no |
| 22 | Contract vocabulary | IR | C | no |
| 23 | Authority covers every loss class | ART | C | no |
| 24 | A host source enters the plan once | IR | C | no |
| 25 | Intervening writers include the divergence node | IR | C | no |
| 26 | Repeated histogram statistics listed once (emitted once from stage 6) | NAT | D | no |
| 27 | Split ports narrow kinds by predicate | IR | C | no |
| 28 | Equivalence compares contexts by parts | VER | C | no |
| 29 | Interpreter trace time | VER | C | no |
| 30 | Trace events before their stage are explicit errors | VER | L | no (stale since stage 6) |
| 31 | Stage 9 sum check is pairwise per bucket | VER | C | no (revisit in stage 9) |
| 32 | Aggregation's contract as an opaque host | IR | C | no (historical) |
| 33 | Provenance is consistent | ART | C | no |
| 34 | Approval binds to the document as received | ART | C | no |
| 35 | Scopes are closed and can end at exits | ART | C | no |
| 36 | Traces are well typed | VER | C | no |
| 37 | Binding names are typed-model paths; credentials never bindable | IR | C | no |
| 38 | Lowering fails where native construction fails | NAT | N | no (stale on aggregation since stage 6) |
| 39 | Host facts include the Cluster Agent endpoint | NAT | C | no |
| 40 | Before stage 4, gateway emits are named `<gateway>_out` | ART | C | no (historical) |
| 41 | Output fingerprints cover where an output delivers | ART | C | no |
| 42 | A clock reset admits to the cut's new generation | STA | D | no |
| 43 | Fusion stops at cycles through a fence | FRM | C | no |
| 44 | A superseded generation is `draining` until it retires | INS | C | no |
| 45 | Configuration changes queue behind a preparation | INS | D | no |
| 46 | Last-known-good only below an artifact generation | INS | D | no |
| 47 | R1 pipelines by cost | FRM | D | yes: a design choice; the owner favors removing it if stage 9's parallelism makes it unnecessary |
| 48 | Fusion removes queues | FRM | D | no (resolved in stage 5; drop it) |
| 49 | Capacity rule judged under one interleaving | VER | C | yes: adopt the Equivalence wording |
| 50 | Native filters move only whole | FRM | D | no |
| 51 | Decisions group only directly joined splits and tees | FRM | D | yes: adopt the wording |
| 52 | R7 writes its operand order into the plan | FRM | D | yes: adopt the wording |
| 53 | Idempotency against the policy in effect, redeliveries only | ART | C | no |
| 54 | A revision counts as seen once authentic | ART | C | no |
| 55 | Acknowledgment unchanged until its generation retires | ART | C | no |
| 56 | Configuration changes reach artifacts by re-composition | ART | C | no |
| 57 | Artifact equal to the running plan creates no generation | INS | C | no |
| 58 | Revocation final for artifacts accepted before it | ART | C | no |
| 59 | A supervision failure belongs to the document | INS | C | no |
| 60 | Per-config outcomes survive a refused redelivery | ART | C | no |
| 61 | Stale base relative to the policy region and outputs | ART | C | no |
| 62 | Bucketing by substitution, with a 2 s tick grace | STA | D | yes: `TICK_GRACE` entry |
| 63 | Shutdown discards the bucket open at the latest time | STA | N | no |
| 64 | Clock reset flushes everything; threshold 300 s | STA | D | **yes: confirm 300 s and its entry** |
| 65 | Heartbeats carry their scheduled generation; 1 s period | STA | D | yes: `HEARTBEAT_PERIOD` and `PROGRESS_PERIOD` entries |
| 66 | Cut arming is one transaction | STA | D | yes: `CUT_ARM_ATTEMPTS` entry |
| 67 | Cut phases | STA | C | no |
| 68 | The cut boundary travels with the generation | STA | C | no |
| 69 | Time-driven output generation | STA | C | no |
| 70 | Retirement at the crossing | STA | D | no |
| 71 | Transition width bound: `lcm` at most 300 s | STA | D | yes: `MAX_TRANSITION_LCM` |
| 72 | Cut installations keep no last-known-good generation | STA | D | no |
| 73 | Drain deadline of a cut's replaced generation starts at finalization | STA | C | no |
| 74 | One global context budget | STA | D | no |
| 75 | Lineage key is node plus state contract | STA | C | no |
| 76 | R5b is emission suppression inside the state owner | STA | D | yes: withdraw two decision-log statements |
| 77 | The aggregation slot `dsd_agg` | STA | C | no |
| 78 | The pre-aggregation scope | ART | D | no |
| 79 | Proposals during a cut | STA | C | no |
| 80 | Context snapshots cover every lineage | TEL | C | no |
| 81 | Interpreter time | VER | C | no |
| 82 | Kind and rate-interval conflicts counted as policy loss | TEL | D | no |
| 83 | Bucket widths validated at configuration load | NAT | D | yes: accept the startup failure |
| 84 | One admission point per state owner through stage 8 | STA | L | no (input to stage 9) |
| 85 | Supervision of state | STA | D | no |
| 86 | Verification of bucket assignment | VER | C | no |
| 87 | Same-revision configs in one snapshot all rejected | ART | C | no |

Totals: kinds C 49, D 26, N 9, L 3; themes STA 20, ART 16, NAT 12, INS 9, IR 8, VER 8, FRM 7, TEL 4, EXE 3.

Amendments 88 to 140 are in [the stages 7 and 9 plan's section
10](programmable-metrics-stages-7-and-9.md#10-proposed-spec-amendments): 88 to 107 from its stage 7 design, 108 to 128
from its stage 9 design, 129 to 138 from stage 7's parts and integration, and 139 and 140 from stage 9's foundations
(P9.F: `last_seen` in the aggregation kernel, refining 117; the statistics tap observes what is dispatched, refining
126); the next is 141. Stage 7's:

| # | Short title | Theme | Kind | Owner |
|---|---|---|---|---|
| 88 | Fixed frame for resources | FRM | D | no |
| 89 | Leases | INS | D | no |
| 90 | Output fingerprints cover the selector | ART | D | no |
| 91 | Removal by the resource authority wins, and fails closed | ART | D | yes: accept degraded over fallback |
| 92 | Pending and degraded | ART | C | no |
| 93 | Resources artifacts | ART | D | yes: endpoint privilege |
| 94 | Declared recipient identity | NAT | C | no |
| 95 | Credentials | IR | C | no |
| 96 | Retirement closure for recipients | INS | C | no |
| 97 | The delivery-policy slot, first opening | EXE | D | no |
| 98 | Retry queue identity stays native | NAT | N | no |
| 99 | Declared output IDs | IR | C | no |
| 100 | Last-known-good and resources | INS | D | no |
| 101 | Payload classes and resource transactions | NAT | C | no |
| 102 | Budgets for resources | EXE | C | no |
| 103 | Encoder streams per binding | EXE | C | no |
| 104 | Encoder and forwarder settings are plan parameters | IR | D | no |
| 105 | Standalone mode | NAT | N | no |
| 106 | `dd_out` without the plan | NAT | N | no |
| 107 | Capability numbering | FRM | C | no |
| 129 | Renumbered outputs reject new artifacts | ART | C | no |
| 130 | Who asks for missing structure | FRM | C | no |
| 131 | Bound outputs carry their own fingerprint rule | IR | C | no |
| 132 | Version 1 bases upgrade when they compose | ART | C | no |
| 133 | A waiting artifact is the newest of its scope | ART | C | no |
| 134 | Native recipients by configured position | NAT | C | no |
| 135 | Burst slack per encode host | EXE | D | yes: accept the smaller slack |
| 136 | Plan-level telemetry has owners | TEL | C | no |
| 137 | Withdrawal precedes the generation that drops a recipient | INS | C | yes: accept the loss window |
| 138 | Retirements are releases | INS | C | no |

Wording proposed for adoption: amendment 49 ("under the reference interpreter's delivery order"), 51 ("a maximal
group of `split` and `tee` operators joined directly by edges within one region"), and 52 ("operator parameters,
other than the order of a pure `all` or `any`'s operands"). Amendment 76 proposes withdrawing two statements. The
first is "removing keys can only reduce capacity loss." The second is "the optimized plan's occupied keys are always
a subset of the original's." Both are in the R5b row of the specification's Rewrites table (under Compiler), not in
its decision log, as the amendment and the R5b section of the implementation design call it.

## Known limitations and risks

[Risks](programmable-metrics-implementation.md#risks) records the performance evidence and what it does not cover
(SMP on Linux, queue bytes, slow sinks, key skew, decoder-pool contention at the admission mutex, the encoder-side cost
of per-batch passthrough, the per-kernel resource-group cost on Linux, burst loss at a real UDP socket), R1's coarse
estimates, memo costs, and the stage 6 risks. The gaps below are recorded there only in part, if at all.

**Verification the gate did not do:**

- The gate is not CI. It never ran `make check-docs` (whole-tree Vale, a CI job: the specification adds 12 errors
  there, see [Environment notes](#environment-notes); since the stage 7 review the gate runs Vale on every file the
  stage changes or adds, which has none), `check-fips-module`, `check-aix-cross` (which type-checks ADP,
  and so the new crates, for AIX), or `test-docs` (parts ran `cargo test --doc` on the crates they touched, the gate
  never did). Nor did it run the CI jobs outside `check-all`, such as `check-schema-overlay`, which fails if the
  generated configuration reference the branch changed is out of date. `test-miri` and `test-loom` never ran either,
  but their targets (`stringtheory`, and `saluki-core`'s `observability::metrics`) are untouched. The measurement
  probes' tests (`make test-measurement`) and every crate's integration test targets (`make test-integration-targets`,
  among them `saluki-core`'s `worker_resource_group`, which pins a worker's resource-group accounting) ran in no CI job
  before the stage 7 review: the gate runs both since, and CI has a job for each.
- Through stage 6, no Panoramic correctness or integration run on the plan-driven path. Stage 7 ran the correctness
  suite once, on an image built from the stage 7 tree (`make build-datadog-agent-image-release`), each case against
  the same image's Agent without ADP as its baseline (`DOCKER_HOST` set to the local daemon's socket): 35 of 42 passed
  and none failed; the 7 `dsd-origin-detection-matrix` cases need `kind`, which the machine lacks. `otlp-traces` is
  disabled on `main` (#2723), so `otlp-traces-probabilistic` stood in for the gate's traces case. Not run: the
  integration tests, a baseline built from `main` (`--image-override baseline=<main image>`), and any correctness case
  that exercises endpoint allowlists, Multi-Region Failover, or autoscaling failover (none exists). A parity check
  against upstream ADP: run the `dsd-*` cases with `panoramic --image-override baseline=<upstream image>`.
- The runtime differential tests run standalone only. Connected mode's `liveness_in` and host tags are covered only by
  the structural comparison with the legacy blueprint.
- The frame property test never generates candidates whose nodes differ from the boot plan's; one compile test covers
  placing new nodes.
- ADP-level stage 6 tests not built (covered at unit or harness level): a snapshot-bridge panic in a running
  topology, revocation while a cut is irrevocable, two-scope composition, trace 13, a failed cut generation's loss
  window, and arming while `dsd_in` or `otlp_in` is blocked under load. Trace 14's irrevocable phase runs only in the
  interpreter.
- `property_test_r5b_exact` draws artifact-shaped plans, not the default program, and does not run
  `compare_under_capacity`.
- No ADP test with the real core of a re-composition that the core rejects.
- The `GATEWAYS` measurement has never run on an idle machine; it passed in stage 6 at load about 7.

**Behavior worth knowing:**

- When artifacts for both scopes arrive in one snapshot and the combined candidate fails the transition-class or
  cut-width check, every new artifact in the snapshot is rejected, not only the one at fault: `accept` checks
  acceptances together and then alone through `InstallerCore::check`, but those two checks run later, in `start`.
- The stage 4 selection numbers in Risks came from a temporary alternating-rounds harness that is not checked in;
  `benches/selection.rs` measures the same shapes with criterion, more noisily.
- The demo's exit evidence included a manual run with screenshots; none are checked in.

**Flaky or environment-dependent tests:** six proxy tests that exist on `main` (four in `saluki-components`
`common::datadog::proxy`, two `saluki-io` proxy tunnel timeout tests) fail intermittently on macOS in a sandbox under
parallel load; the `channel_stub` and `api` network tests can time out (about 30 s) on the first run of a freshly built
binary and pass on rerun; nextest reports one to four "leaky" tests per full run that never reproduce; and
`a_transaction_takes_its_locks_one_at_a_time_in_the_order_given` occasionally reports a leak under load.

## Deferred work by stage

The branch adds no `FIXME`, `todo!`, or `unimplemented!`; its five `TODO`s were carried over from `main` by the
extraction (the mapper and aggregation cores), and its five `#[ignore]`s are the measurement tests. Deferred work is
marked by capability errors that name a stage, and by the notes below.

### Stage 7: resource generations

Built (see [Where things stand](#where-things-stand)). What stage 6 left for it is resolved: the capability errors are
renumbered (amendments 107 and 130: "needs a restart" or "requires stage 10 frame changes"); outputs and recipients
change live; the drain-on-close mechanism was ported from the spike (P7.1a), so a retiring recipient sends what it
holds; leases' and recipients' registrations are released at retirement, and the reclamation test shows tasks,
resource groups' bounds and the hosts' live bytes, health registrations, the resource table, the resource budget
(counted against a memory limit), and the metric handles the process holds back at their baseline after 1,000 cycles,
each of which adds and removes an additional endpoint and declares and revokes a recipient and an output, on a domain
of its own; later
generations' resources are admitted against the resource budget; oversized V2 payloads are counted. Deferred, each with
its reason in the plan's [Stage 7 as built](programmable-metrics-stages-7-and-9.md#stage-7-as-built):

- Static mode folded into the delivery host (it stays the native forwarder, pinned against the host).
- Platform root certificates loaded once per process across client rebuilds.
- A structured prepare failure naming the recipient, in place of text matching; a declared recipient's directory
  including its name. (Since the stage 7 review, a prepare whose directory a dropped loop owns waits for that loop's
  exit, so the text matching no longer meets that race.)
- Growth in place of a live recipient's bound (its credit keeps its creation's bytes).
- 4.13's validation errors for a declared queue bound above the native one and for spilling without storage (capped at
  run time instead).
- The follow-up of the plan's A18 and A19: credentials at rest, version-stable directories, upgrade adoption, and a
  janitor for orphaned directories.
- Tests the plan's 6.11 lists that stage 7 did not build in ADP, each covered at a lower level: a recipient loop that
  fails replaced without a configuration change (delivery host and core tests), a host's prepare timeout and worker
  restarts during transactions (the installer's tests with fake hosts), disk full during a retirement's persist (the
  recipient loop's flush tests), generation failures with and without a kept last-known-good across two scopes, and a
  cancelled cut whose generation adds an endpoint (the core's property test); the resources product end to end (the
  demo's S8 to S11 against the real binary); and autoscaling failover's delivery, whose Cluster Agent host needs an
  `https` endpoint the recording intakes do not serve (the delivery host's Cluster Agent tests). Since the stage 7
  review, the failed-prepare test checks events, logs, and traces too; not built: a routing-group add and remove in
  the reclamation cycle, and the non-metrics delivery of an `additional_endpoints` change while a cut is in flight.

### Stage 8: production control plane

- The stub, its `experimental.programmable_metrics.artifact_channel.*` keys, and ADP's `rustls` dependency (the stub
  client's empty root store) exist only until a real channel replaces them.
- Approved-revisions authority in validation is deferred by decision: composition is the only writer of provenance, so
  it is needed only once provenance can come from elsewhere.
- Facts about Remote Config from the codebase survey: an Agent cache expiry arrives as an empty snapshot, which the
  specification treats as a revocation; apply states are synchronous decoder verdicts, so asynchronous install
  outcomes need an extension of the client crate; and versions are invisible to the subscriber, so revisions must
  travel in the payload.
- Through stage 8, regions replace only chains of synchronous work (`compile/mod.rs`).

### Stage 9: partitioned execution

- **Foundations in place (P9.F)**, each neutral at one lane, and what they leave to later parts (the plan's [P9.F as
  built](programmable-metrics-stages-7-and-9.md#84-stage-9-foundations-interfaces-step-and-parts) gives the reasons):
  - P9.3b: move the budget's `loom` models from `saluki-core` (`execution::budget::loom_tests`, which transcribe
    `try_admit`) into the kernels crate once it has a `loom` feature, running `try_admit` itself; call
    `enter_measured` at drain steps and `finish`; build every lane-side core with `.with_last_seen(LastSeen::Latest)`
    unless `AggregateKernel::new` builds it, or a delayed earlier batch moves a counter's expiry back.
  - P9.4: read `StateBudget::recorded`'s operations in `check_capacity` (each carries the `used` value it observed;
    their order may differ from the compare-and-swaps'); oracle cores and the arrival replay set `LastSeen::Latest`.
  - P9.5: set `set_cpu_time_sampling` at startup (16 on Linux, or `SALUKI_CPU_TIME_SAMPLING`); call `enter_measured`
    at installer builds; decide how the gauges a background task sets (`cache_current_items`, `cache_current_weight`,
    `cache_weight_limit`, `context_resolver_interner_*`) sum across lanes, since today the last lane's value wins.
  - P9.1a: carry the tap gate's wiring (`blueprint.rs` collects gates at build, `built.rs` attaches gated senders,
    `DispatchTarget::sender_gates`, `Consumer::received_metrics`) over to lane edges.
  - M7, the tap gate's saving, is not measured yet.
  - `StateBudget`'s atomics are `loom`'s under `cfg(all(test, feature = "loom"))`, so `saluki-core`'s `loom` build
    runs only with the `loom_tests` filter, as `make test-loom` does.
- **Frame restrictions.** `FrameError::StateOwnerPoints` (a state owner is fed by one admission point until stage 9
  adds repartition edges) and `StateOwnerPaths` (one physical path per point); amendment 84. The interpreter drains
  everything on a reset at any of several points; the multi-point reset rule arrives with repartition edges. The
  watermark tracker already keeps the min-over-points rule, so only the reset rule changes; since P9.F, it reports an
  epoch change as `Observation::EpochSwitched` with what it knew before the header (`in_publication_epoch_with`), and an
  older-epoch header after a newer one from the same point as a fault.
- **The oracle is partly built.** `compare_reordered` decides counters and rates (pairwise within 1e-9 relative), sets
  (by union), and count mismatches; gauges, histograms, and distributions are `Undecided`, since the gauge check needs
  the admitted inputs (amendment 31).
- **Where the parallelism went.** Legacy ran the five DogStatsD transforms as five tasks, pipelining consecutive
  batches. Fusing them into one region (about 250 ns per metric, serial) ran one saturated stream at 0.68 to 0.72 of
  legacy on four workers. R1's cost model (`OperatorDef::cost`, `REGION_COST_FLOOR` 64 ns, `FusionVerdict::Pipelined`)
  cuts chains back into pipelined regions, restoring 1.04 (four workers) and 1.27 (one worker) at 0.77 to 0.94 of the
  worker time. In the stage 3 benches (Risks) the region driver cost 1 to 3% over hand-written kernel calls (the
  post-aggregation case ran faster than its baseline), and the host about 70 ns per buffer; the stage 6 benches, at
  load about 7, show 6 to 13%, with the stage 5 tree at 10% on post-aggregation in the same conditions.
- **What the cost model couples to**, if first-class parallelism removes it: `compile/frame.rs` (`REGION_COST_FLOOR`,
  `Slot::cost`, `Frame::cost_budget`); `operators/routing.rs` (`decision_cost`); R4 memoization, which decides by the
  same costs (`MEMO_COST_FLOOR` 25 in `compile/decision.rs`), so removing R1 still needs a memo rule;
  `explain/physical.rs` and `explain/text.rs` and the cost lines of the explain golden files; `testing/fixtures.rs`
  (slot naming after cuts); ADP's `region_input_capacity` and the burst pins (515, 257, 386), which follow frame paths;
  `GATEWAY_REGIONS` in `telemetry::tests` and the frame and edge pins in `topology::tests`; `selection_default_program`
  (it asserts the fused policy decision); amendment 47. Slot names change too: the stage 3 and 4 release notes
  advertise `dsd_prefix_filter_region`, `dsd_no_agg_split_region`, and `dsd_post_agg_filter_region` as component IDs.
  Isolation slots are already unpriced.
- **Order and admission.** One `tokio::sync::Mutex` per admission point is held across the stamp and the sequential
  fan-out; FIFO order by `t_a` per channel rests on it, including for the DogStatsD decoder pool, which shares one
  dispatcher. Native fan-out awaits each sender in turn, so one full output stalls the others, and regions reproduce
  that. Contention between decoders is unmeasured.
- **Context hashes are process-local.** `ContextKey` comes from a `foldhash` state seeded once per process
  (`ContextKey::as_u64` documents this): a partition key derived from it is stable within a process only, so fixtures
  cannot pin it and nothing may persist it.
- **Registrations** (see stage 7): partitions must be fixed at boot or reuse their health and resource-group
  registrations.
- **State.** One global `StateBudget` is shared by lineages: an `Arc` of an atomic counter that every new context
  acquires from with a compare-and-swap (`execution/budget.rs`), so partitions of one owner would contend on it unless
  credits are split (the specification's stage 9 lists credit splitting); since P9.F a new context takes its node's
  unit first, in one bounded acquisition (`try_acquire_below`), so the node limit holds at any lane count, and M4
  decides whether contention needs O1. `CutBoundary::epochs` is per point. The
  interpreter shares one output latch per set of upstream points where the frame gives each isolated owner of a boot
  plan its own slot, so comparisons must avoid two isolated owners over one point.
- **Placement gaps.** A state owner fed straight by a point whose route reaches the isolation slot only through
  pipelined slots has no placement (a pre-aggregation artifact's aggregation under mapper profiles); a multi-hop
  pass-through, or stage 7's new edges, would give it one.
  `AdmissionPointSnapshot` carries no heartbeat recipients (optional). R1 prices a memoized decision as a hit, and with
  DogStatsD defaults and every gateway estimates `metrics_enrich` at 75 ns against a 64 ns budget.
- **Stage 6 choices and the reasons behind them.** The
  [decisions](programmable-metrics-implementation.md#decisions) of stage 6 list which alternatives it took; a redesign
  needs why:

  | Choice | Rejected alternative | Reason |
  |---|---|---|
  | Arm under the affected points' locks only | Lock every point | A point's lock is held across its fan-out, so locking an unaffected source under backpressure (five points from `connect_metrics_source`) would stall DogStatsD. |
  | Heartbeats carry generations | Generation-free heartbeats | A slot could not tell a post-publication reset at an idle point from a pre-publication one. |
  | Heartbeats only toward state owners | To every consumer | `dsd_in.metrics` also feeds host taps (`dsd_stats_out`, `dsd_debug_log_out`). |
  | Retirement at the crossing | At the next tick | The old lineage releases every budget credit before the new one admits, so a cut near the context limit loses nothing. |
  | Reset by `control(ClockReset)`, instance kept | Rebuild the instance | No rebuild on the data path; credits and gauges stay continuous. |
  | One upstream point per slot | Several | A second point's reset would emit the first point's open buckets of the new timeline early; deferred with the restriction. |
  | `lcm` at most 300 s | 900 s | Keeps the specification's cut-wait bound and a bounded overlap. |
  | A failed cut generation rolls back by reinstalling the last-known-good plan as a new installation with its own cut | A one-store `Steady(last-known-good)` commit | After the cut, that would run two lineages of one series without a boundary and feed a retiring lineage. |
  | A revocation or newer artifact against an armed cut cancels it while cancellable, else installs after finalization | Re-arm a replacement at the same `T` | That needs a cancel transaction per native change and a second token with an equal `T`; installing after finalization is simpler and never moves `T`. |
  | Progress throttled to 100 ms of `t_a` | The heartbeat period (1 s) | Closure lag under fully filtered traffic is ten times smaller, at about 10 header-only buffers per second. |
  | Output epoch is the slot's reset count | The upstream point's epoch | Well defined, and independent of the one-point restriction. |
  | Lineage key from the state contract only, other parameters passed per call | Every parameter, any change a `cut` | The specification classifies transitions by contract. |
  | One global context budget plus per-node caps, credits released on `Drop` | Per-node credits with a sum-of-limits placement rule | With one lineage it is exactly native `len >= limit`; the global cap keeps the firm state bound true during a cut. |
  | R5b as emission suppression | The specification's removal form; ghost entries at insertion | Removal breaks the capacity rule for first-come admission with expiry (amendment 76); ghost entries diverge when an installation changes the filter. |
  | One product-wide revision mark | One per scope | One mark satisfies "revisions strictly increase" across scopes. |
  | Ticks gated by in-band headers | Driver-requested heartbeats | A driver cannot await or reach the registry; in-band headers have the same effect. |

### Stage 10: measured extensions

The specification's stage 10 is a list of candidates, each needing its own measured benefit; the code names it for
the ones it meets. The frame names stage 10 for an isolation fence without keyed state, state in a region without an
isolation fence, operators that read host signals, and a state owner that no admission point feeds (such as one fed
by another state owner's time-driven output) (`compile/frame.rs`). Placement has `MissingCapability::Signals`; the
interpreter rejects signal readers (`interpret/program.rs`).

### Follow-ups not tied to a stage

**Native issues preserved on purpose and never filed** (the design says "tracked separately"; nothing tracks them):

- `flush_open_windows` (`dogstatsd_flush_incomplete_buckets`) has no observable effect, since a final flush emits only
  closed buckets either way, but three places promise "flush open buckets on shutdown": the aggregate transform's
  configuration docs in `saluki-components`, `agent-data-plane-config`'s `domains/dogstatsd.rs`, and
  `schema_overlay.yaml`, which also marks the key `support: full`. The kernels crate documents it correctly.
- New counters get zeros for buckets before they were first seen, and an empty tick leaves `last_flush` stale, so the
  first counter after an idle gap gets a zero for every bucket of the gap (99 zeros after 1,000 s). Fixing it changes
  customer output and needs a release note.
- `aggregate_active_contexts_by_type` and `aggregate_active_contexts_bytes_by_type` are incremented with a context's
  first kind and decremented with its current one (`aggregate/state.rs` in the kernels crate), so after a counter to
  gauge conflict `{metric_type:counter}` stays at +1 and `{metric_type:gauge}` goes to -1. They feed the Agent's
  `aggregator.dogstatsd_contexts_by_mtype`.
- `component_events_sent_total` double counts for `BufferedDispatcher` users: `DispatchTarget::send` adds senders
  times items and `BufferedDispatcher::flush` adds the flushed length again. Regions do not use it, so `dsd_agg` no
  longer double counts; native buffering components still do.
- The encoder's uncounted V2 payload drops (see stage 7).

**Plan crate and runtime:** live updates allocate 1.3 to 2.1 times legacy per metric (each change lowers, compiles,
and prepares every region; regions could reuse an unchanged program's code and telemetry, which
`SlotKernels::unchanged` already hands them); kernel construction is not fallible, so an allocation failure in a kernel
such as the mapper's interner aborts past any panic budget (`try_reserve` would fix it; `artifact_memory` keeps
artifacts away from it); nothing compiles with an `OperandProfile`; the two random-plan generators could merge;
`testing::with_opaque_gateways` could retire; explain shows no per-node memory bounds; admission points have no
telemetry for stamps, heartbeats, or lock contention (resets have `plan_clock_resets_total`); the interpreter cannot
preview an unresolved plan whose kernel parameters are bound (the demo composes into `/default-program` instead);
unmemoized selection is dominated by `CompiledPredicate::evaluate`, and a fused evaluation of `any` over
`intake_name` leaves would close more of the gap to the hand-written selection; R3 leaves dead subgraphs below dead
ports that are not a `split`'s (such as an untimestamped input's passthrough), since removing them would need a `drop`
node; the `live_binding_on_host` validation warning (`Rule::LiveBindingOnHost`) has had no native producer since
aggregation became a kernel, and is kept for later hosts.

**Installation and artifacts:** see [Risks](programmable-metrics-implementation.md#risks) for receipt limits counting
unmodified nodes, panic charging residue, and re-approval provenance; the joint two-scope attribution is under
[Known limitations and risks](#known-limitations-and-risks).

**ADP:** the configuration reference has no sentence on region input capacities (it is generated from
`schema_overlay.yaml`; a sentence under `aggregator_buffer_size` was suggested); encoders' event interconnects are not
counted in the blueprint's bounds (existing on `main`); region input sizing follows the longest boot path.

**Demo:** an exact whole-run canary check (pauses at both range edges, about 30 s more per run); a check step for the
pre-aggregation scope; compiling the preview into the frame (capability errors show only at ADP); shorter
duplicate-contribution messages; a printer break inside `config("...")` for editors below about 81 columns (46 test
strings depend on today's layout); a liveness check of ADP after `/ready`; the plan graph; S7 is manual.

**Syntax crate:** pretty printing is O(depth times size) for deeply nested predicates (a 58-level, 220 KB case prints
in 0.58 s in release; memoized flat widths would make it linear); one syntax error per parse; no `->` chains; enums and
output references are quoted strings; comments are not preserved; spans inside predicates fall back to the predicate's.
The crate documentation lists the round-trip holes, all in invalid artifacts.

**Stale text in the implementation design and code:**

- The stage plan says every gate ran `make check-all` and `make test-all` (see [the stage gate](#the-stage-gate)).
- The region host's panic-hook paragraph says stage 5 must decide; it did not.
- The stub worker paragraph, the Risks entry on stub tests, and the comment in `client.rs` describe the proxy TLS
  inaccurately (see [the root-certificate change](#the-root-certificate-change)).
- The stage 6 exit evidence says `make check-features` "was not run as is"; the gate script did run it, and it passed
  (see [Environment notes](#environment-notes)).
- The Demo section's S5 list ("an `aggregate` node") and check expectations ("keeps state") describe stage 5's
  guardrails; the stage 6 [scopes section](programmable-metrics-implementation.md#scopes-ir-composition-channel) has
  the current S5c and S5d.
- Amendment 30 says `cancel` and clock resets are interpreter errors; stage 6 implemented both. Amendment 38 and the
  default-program paragraph say native aggregation still emits repeated statistics once per listing; since stage 6 it
  emits them once. Amendment 26's title says "rejected" where its text says listed once.
- Amendment 76 and the R5b section call the two statements it withdraws the decision log's; they are in the
  specification's Rewrites table.
- Amendment 11 says the aggregation quirks are "tracked separately," and Risks says so of
  `component_events_sent_total`; nothing tracks either.
- `limits::BUCKET_WIDTHS`'s `# Missing` section says validation does not enforce widths; it does: parameter checks
  call `check_bucket_width` (`operators/mod.rs`) from `operators/aggregate.rs` and `operators/dogstatsd.rs`, and
  `validate/state.rs` adds the `widths_above_sixty` and `split_window` rules.

**Outside this work:** `agent-data-plane-config-system`'s `system.rs` comment says there is no timeout on the initial
snapshot wait, but the code waits 15 s; `docs/development/testing.md` says airlock talks to containerd over gRPC, but
it uses Docker through bollard.

## Environment notes

- **macOS test concurrency.** In a sandbox on macOS, tests that configure proxies still load platform certificates per
  client build, and under parallel load the trust-settings reads fail (OS error -36, about 24 s). Run
  `saluki-components` and `saluki-io` with `--test-threads 4` (stage 5 verified `--test-threads 2`). This is a local
  workaround: the flag was added for `saluki-io` after the stage 6 gate, whose only failure was those two `saluki-io`
  tests, so no recorded gate ran it that way. The script's comment blames slow root-certificate loading; the failures
  are the trust-settings reads. Linux CI is unaffected (`rustls-native-certs` reads PEM files there), and the macOS CI
  jobs run outside a sandbox and ran these tests on `main`. Running outside the sandbox, or the optional `set_tls`
  change, removes the need.
- **Cold network tests.** `channel_stub` and `api` tests can time out on the first run of a freshly built binary under
  full parallel load; rerun.
- **`PROPTEST_CASES` is ignored** by tests that set `ProptestConfig::with_cases`, which most plan crate property tests
  do. To run more cases, raise the count in the source temporarily.
- **Measurements need an idle machine.** Above a load of about 4, legacy against itself fails its own tolerances.
- **Disk.** Parallel work trees each build their own `target/` and fill the disk quickly. Remove a work tree's `target/`
  when it finishes (`cargo clean --release` or `rm -rf target`), and build with `CARGO_INCREMENTAL=0`. When the disk
  is full, the step of `make fmt` that installs or updates the nightly Rust (`ensure-rust-nightly`) fails; its
  other steps can run directly (`cargo +nightly-2026-01-18 fmt`, `cargo autoinherit`, `cargo sort --workspace .`).
- **`make check-features`** walks every `Cargo.toml` under the repository, including other sessions' work trees under
  `.claude/worktrees/` (empty now) and `.git/pi-main-archive` (whose virtual manifest prints a harmless error). It then
  checks each package once per tree that has it, and would fail on a package only another tree has. The stage 6 gate's
  run passed; the stage 6 integration also ran `cargo hack --feature-powerset --package <p> check --tests` once per
  package of this workspace.
- **Vale.** `make check-docs` (a CI job) reports 51 errors at `b01049372e` with Vale 3.21.0: 12 in the specification,
  which this branch adds, and 39 already on `main` (38 in `lib/datadog-agent/remote-config`, 1 in `panoramic`). The
  gate therefore ran Vale on the changed paths only. Eleven of the specification's errors are spelling (such as
  `injective`, `unflushed`, and `syntaxes`) that entries in the Vale vocabulary
  (`.vale/styles/config/vocabularies/technical/accept.txt`) would clear without editing it; the twelfth,
  `Google.LyHyphens` on `Exactly-once`, needs the author's edit or a rule exception.
- **Release notes.** `reno` is not installed. Fragment suffixes came from `openssl rand -hex 8`, and fragments were
  checked with `make check-release-notes` or `.venv/bin/python3 -I ci/tooling/release_notes.py check <note>` (the
  system Python lacks PyYAML).
- **Local ADP on macOS** (the demo launcher handles all of this): ADP needs a configuration file that exists, a
  non-empty `api_key` and an explicit `hostname` in standalone mode, a readable IPC certificate
  (`make create-dummy-ipc-cert`), and a writable log path (set `DD_DATA_PLANE_LOG_FILE` or
  `DD_DISABLE_FILE_LOGGING=true`). DogStatsD binds 127.0.0.1, not `::1`; standalone mode never applies configuration
  updates; `datadog-intake` binds `0.0.0.0:2049`.
- **Agent tooling.** File writes by agents sometimes rewrote string literals starting with `"/` into a proxy URL;
  `git grep gh.072103` finds any. None are in the tree at `b01049372e`.
- **Session leftovers.** The working session registered a git work tree in its temporary directory
  (`scratchpad/demo-wt`, detached at `b01049372e`) and wrote a `.claude/launch.json`, not tracked by git, that points
  at that work tree's binaries and a scratch run directory. After the directory is deleted, run `git worktree prune`,
  and delete or rewrite `.claude/launch.json`. Scratch-only material is lost with it: the stage 4 selection harness
  (`selection_rounds.rs`), the demo screenshots and the headless-browser drivers that took them, a Python stand-in for
  ADP's stage 5 side, and the review probes (every finding they reproduced has a regression test in the tree).

## How the work was done

- **Per stage:** the design first, as a section of the implementation design (stage 6 merged two drafts and reviewed
  them before any code); then an interfaces step that fixes the types parts share; then three to six parts in parallel
  work trees, each implemented, reviewed through several lenses, and fixed; then one integration step that merges,
  regenerates golden files, runs [the stage gate](#the-stage-gate) and the demo check, and updates the design and
  release notes; then one local commit. Stage 3, built as five serial parts, took about six hours; the
  interfaces-first fan-out roughly halved later stages.
- **Conventions:** semantic fixtures are hand-written expected results, independent of the code under test; golden
  files only detect change. A restructured native component keeps a frozen `#[cfg(test)]` copy of its pre-extraction
  code (`reference.rs`), with kernel-against-reference property tests. Every fix was mutation-checked by hand:
  introduce the bug, see a test fail, revert. Property tests are named `property_test_*` and run in release. Test seams
  live behind `#[cfg(test)]` or `test-util`. `saluki-core` stays free of metric and Datadog knowledge. The
  specification is never edited; refinements become proposed amendments.
- **Practice:** keep a short orientation brief so parts need not read every earlier note; start the next stage's
  independent parts on the last committed base; run focused tests while iterating and the full gate only at
  integration; commit locally and unsigned; leave `architecture-experiment/`, which git does not track, alone.
