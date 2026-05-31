---
sut_path: /home/ssm-user/src/saluki
commit: fc4bb29728814ddf9321572b954ec28f58faeb53
updated: 2026-05-30
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: ADP Confluence — correctness-test topology (diff-testing Agent vs ADP via fakeintake) informs reuse.
  - path: https://datadoghq.atlassian.net/wiki/spaces/AMCC/pages/6640602441/Tag+Filter+RC+Relay+Stress+Test+agent+ADP
    why: Documents the Core Agent → AgentSecure config-stream → ADP relay topology (informs the config-stream add-on).
---

# Deployment Topology: Agent Data Plane (ADP)

## Guiding principle

The existing `bin/correctness` harness (`millstone` + `datadog-intake` + `panoramic` + `airlock`) is
a near-perfect base — but it is built for **deterministic diff-testing with a healthy intake**.
Antithesis's value is the opposite: **fault the links the harness keeps healthy**. The single most
important design move is to put the **ADP → intake HTTP forwarding link across a container boundary**
so Antithesis can partition, delay, drop, and black-hole it. Everything else stays minimal.

Antithesis injects faults **per container**: anything in one container shares its fate, and links
*within* a container cannot be faulted. So component placement is dictated by which links we need
faultable.

> **Post-evaluation routing (catalog now 35 properties).** The primary topology + listener variant
> cover ~24 of 35. The remaining ~11 route to the two add-ons: **Add-on 1 (config stream)** covers
> the config/reload cluster — `config-stall-no-deadlock`, `config-incompatible-refuses-start`,
> `config-runtime-update-not-revalidated`, **`filter-config-reload-correct`**, and the reload facets
> of `tag-filterlist-applied-consistently`; **Add-on 2 (diff-test)** covers the differential
> correctness properties — `aggregate-matches-agent`, **`mapper-output-matches-agent`**,
> **`prefix-filter-ordering-matches-agent`**, and the differential facet of
> `tag-filterlist-applied-consistently`. The events/service-checks trio
> (`events-sc-no-silent-loss`, `malformed-event-sc-no-crash`, `events-sc-pipeline-reachable`) runs in
> the **primary** topology (the workload must emit events + service-checks, not only metrics).
> `mapper-interner-bounded` runs in the primary topology (high-cardinality mappable names + small
> mapper interner).

## Primary topology (covers ~24 of 35 properties)

Standalone ADP, fed deterministic load, forwarding to a mock intake — all three on separate
containers so every link is faultable.

```text
+------------------------+        DogStatsD          +------------------------+      HTTP (Datadog      +------------------------+
| workload-client        |  (UDP/TCP, faultable)     | adp                    |       intake API,       | mock-intake            |
| - dogstatsd driver     | ------------------------> | agent-data-plane       |  faultable, retryable)  | datadog-intake         |
| - Antithesis SDK       |                           | (standalone mode)      | ----------------------> | (mock fakeintake)      |
| - test template        | <------------------------ | UDP/TCP/UDS listeners  | <---------------------- | records payloads,      |
+------------------------+   backpressure / health   +------------------------+      acks / 5xx / hang  | queryable for asserts  |
                                                                                                          +------------------------+
```

| Container | Role | Image | Runs | Connections | Replicas |
|---|---|---|---|---|---|
| `adp` | Service (SUT) | reuse `docker/Dockerfile.agent-data-plane` (standalone build) | `agent-data-plane run` in **standalone mode** (`DD_DATA_PLANE_STANDALONE_MODE=true`, `DD_DATA_PLANE_DOGSTATSD_ENABLED=true`), no Core Agent dependency | receives DogStatsD from `workload-client`; forwards to `mock-intake` over HTTP | 1 |
| `mock-intake` | Dependency | reuse `docker/Dockerfile.correctness-tools` (the `datadog-intake` binary) | mock Datadog intake; record + count forwarded payloads; expose a query API the workload reads for assertions | receives ADP forwarder traffic; queried by `workload-client` | 1 |
| `workload-client` | Client (test driver) | thin Dockerfile layering the compiled test-command binaries + test templates + Antithesis Rust SDK | emits `setup_complete`, then `parallel_driver_send_dogstatsd` samples DogStatsD load (the `harness::payload::dogstatsd` feral/clean generator) and `finally_verify_delivery` checks the intake | sends DogStatsD to `adp`; queries `mock-intake` | 1 |

Notes:
- **The DSD link between `workload-client` and `adp` currently uses UDS** via a shared
  `dogstatsd-socket` volume (`DSD_SOCKET`). The tradeoff: the ingress link is no longer independently
  faultable (shared volume, same fate) and it couples origin-detection credentials. A UDP/TCP
  variant would keep the intake *and* the DSD-intake links independently faultable and let
  `malformed-dsd-no-crash` exercise the network listeners; track it as a follow-up (see
  "Listener-coverage variant").
- **Point ADP's forwarder at `mock-intake`** via `DD_URL` / forwarder endpoint config; set a real
  (fake) API key. This is the link that unlocks the entire egress data-loss cluster.
- The driver samples all randomness through `AntithesisRng` (boundary-biased `Probe`/`Boundary`
  samples and `random_choice` selections), so the workload is deterministic and simulator-steerable;
  Antithesis adds the fault dimension on top.

### What the primary topology covers

- **Memory & resource bounds (Cat A):** high-cardinality / many-timestamp load from the driver +
  `memory_mode`/`memory_limit` set on `adp`; node-throttling on `adp` to stress the limiter timing;
  observe RSS vs grant. `rss-bounded-under-cardinality`, `aggregate-context-limit-enforced`,
  `interner-full-bounded`, `memory-limiter-survives-rss-read-failure` (needs `/proc` fault — see
  faults), `retry-queue-bounded-under-outage`.
- **Data integrity & no silent loss (Cat B):** partition / delay / black-hole the `adp↔mock-intake`
  link, then heal it. `no-silent-interconnect-drop`, `forwarder-eventual-delivery`,
  `retry-queue-bounded-under-outage`, `shutdown-drains-no-loss`, `source-dispatch-no-misroute`.
- **Aggregation crash/clock (Cat C subset):** config exploration
  (`aggregate_window_duration_seconds`, now `NonZeroU64` — the `% 0` panic is closed upstream, so
  `aggregate-no-panic-any-window` is just a regression tripwire) and clock jitter.
  `aggregate-no-panic-any-window`, `aggregate-clock-skew-stable`. Sketch internals
  (`ddsketch-*`, `ddsketch-no-nan-poison`) ride the same workload with SUT-side assertions; the NaN
  bypass needs a `checks_ipc` Histogram producer (see "checks_ipc note").
- **Lifecycle (Cat D subset):** SIGINT / node-termination on `adp`. `graceful-shutdown-within-30s`,
  `data-component-failure-triggers-process-shutdown`, `topology-ready-before-intake`.
- **Untrusted input (Cat E) + concurrency (Cat F):** adversarial DogStatsD packets from the
  workload (`malformed-dsd-no-crash`); interner races and non-finite handling ride normal load with
  SUT-side assertions (`interner-reclamation-no-corruption`, `non-finite-values-handled-consistently`).
- **Events & service-checks (Cat B/E additions):** the workload must emit well-formed *and*
  malformed events + service-checks so `events-sc-no-silent-loss`, `malformed-event-sc-no-crash`, and
  the anti-vacuity anchor `events-sc-pipeline-reachable` are exercised — a metrics-only workload
  leaves these vacuous.
- **Transformer correctness (Cat G, primary-runnable subset):** `mapper-interner-bounded` rides a
  high-cardinality flood of distinct *mappable* names against a small `dogstatsd_mapper_string_interner_size`.
  The differential Cat G properties (`mapper-output-matches-agent`, `prefix-filter-ordering-matches-agent`)
  need Add-on 2; the reload ones need Add-on 1.

## Add-on 1 — Core Agent config stream (the `config-*` cluster)

Standalone mode bypasses the remote-agent config stream, so the config-stream properties need a
fourth container: a **Core Agent (or a minimal gRPC config-stream stub)** that ADP registers against
and receives snapshot/partial config from.

```text
+------------------------+   gRPC config stream (faultable)   +------------------------+
| core-agent-stub        | <--------------------------------> | adp (remote-agent mode)|
| - IPC/gRPC config srvr  |   register / snapshot / partial    | DD_DATA_PLANE_STANDALONE|
| - Status/Flare/Telem    |                                    |  _MODE=false            |
+------------------------+                                    +------------------------+
```

| Container | Role | Image | Runs | Why |
|---|---|---|---|---|
| `core-agent-stub` | Dependency | reuse `docker/Dockerfile.datadog-agent`, **or** a new minimal stub speaking the remote-agent IPC/gRPC config protocol | serves registration + config snapshot/partial over gRPC | exercises the no-timeout startup wait and runtime config-apply path |

Covers: `config-stall-no-deadlock` (delay/drop the config stream → quiescent indefinite hang at
`ready().await`, *the* falsification target), `config-incompatible-refuses-start` (send a
high-severity-incompatible non-default key at startup → expect exit 1),
`config-runtime-update-not-revalidated` (send the incompatible key *after* startup → observe silent
apply), and the **Category G runtime-reload cluster** — **`filter-config-reload-correct`** (push
filter config over the stream while metrics flow; explore `broadcast::Lagged` staleness, partial
apply, and key-deletion-clears-all-filtering) plus the reload facet of
`tag-filterlist-applied-consistently` (stale cache after a Lagged-dropped reload). This is the the design partner
design-partner's documented "Tag Filter RC Relay Stress Test" focus, so the stub must be able to
send adversarial/partial/laggy filter updates. A **stub is preferred over the full Datadog Agent** for state-space minimality and because we
need to send adversarial/malformed config the real Agent would never emit; flag as a build task.

## Add-on 2 — Diff-test for `aggregate-matches-agent` (heaviest; optional, separate run)

The differential property needs the Datadog Agent baseline and ADP comparison fed identical load,
each forwarding to its own intake, then compared. This is the existing `panoramic` correctness setup;
under Antithesis the comparison runs as a `finally_`/`eventually_` check during a quiet period.

```text
                +------------------+      +------------------+
   millstone -->| datadog-agent    | ---> | intake-baseline  |
   (same seed,  | (baseline)       |      +------------------+
    fan-out) -->| adp (comparison) | ---> | intake-comparison|  --> finally_: stele diff within ratio
                +------------------+      +------------------+
```

This doubles the container count and the state space, so run it as its **own test template/topology**,
not bundled with the fault-focused primary run. Keep faults light here (fault-induced flush timing
differences create false diffs unless the comparison runs in an `ANTITHESIS_STOP_FAULTS` quiet window
long enough to cover `FLUSH_WAIT`≈32s on both sides). Reuse `stele`/`panoramic` analysis logic.

Beyond `aggregate-matches-agent`, this add-on is also where the **Category G differential**
properties run, with identical config/profiles on both baseline and comparison:
**`mapper-output-matches-agent`** (identical `dogstatsd_mapper_profiles`, corpus of mappable names),
**`prefix-filter-ordering-matches-agent`** (corpus where keep/drop depends on stage order), and the
differential facet of **`tag-filterlist-applied-consistently`** (post-filter name/tags within ratio).
The corpus must actually exercise mapped/filtered metrics or these (and the `aggregate-matches-agent`
roll-up's implicit coverage of them) pass vacuously.

## Listener-coverage variant (secondary)

To cover UDS-datagram / UDS-stream listeners and the DogStatsD **replay** properties
(`replay-no-panic-on-malformed-capture`, `replay-corruption-not-silent-eof`): add a `replay-client`
that shares a volume with `adp` for the UDS socket and runs the `agent-data-plane dogstatsd replay`
CLI against **adversarially generated capture files** (the workload synthesizes corrupt/truncated/
zstd-bomb captures using the SDK's RNG). Replay runs as a separate CLI process, so its panic/OOM is
isolated from the data plane — install the SUT-side panic/reachability assertions in that CLI process.
No cross-container faults are needed for replay; it is pure untrusted-input exploration.

## Fault requirements (confirm enabled for the tenant)

Antithesis disables some faults by default. **The user confirmed (2026-05-28) that node
termination, clock jitter, and custom `/proc` faults are all enabled for this tenant**, so the
properties that depend on them are realizable rather than vacuous. The custom `/proc` fault still
needs a script.

| Fault | Needed by | Status |
|---|---|---|
| **Node termination** (kill/restart) | `disk-persisted-retry-survives-restart`, `data-component-failure-triggers-process-shutdown`, crash-recovery facets of `forwarder-eventual-delivery` and `aggregate-matches-agent` | **Confirmed enabled** |
| **Clock jitter** | `aggregate-clock-skew-stable` (and the clock facet of `aggregate-matches-agent`) | **Confirmed enabled** |
| Network partition / bad-node / congestion | entire egress data-loss cluster (Cat B), `no-silent-interconnect-drop` backpressure | Usually on |
| Node throttling / CPU modulation | `rss-bounded-under-cardinality` (limiter timing), `memory-limiter-survives-rss-read-failure`, interner-race timing | Usually on |
| **Custom fault** (`/proc` RSS-read failure) | `memory-limiter-survives-rss-read-failure` — needs a custom fault/script to make RSS unreadable mid-run | **Confirmed enabled** (script still TBD) |

- **Liveness properties** (`forwarder-eventual-delivery`, `disk-persisted-retry-survives-restart`,
  `shutdown-drains-no-loss`, `config-stall-no-deadlock`) need a quiet window to verify recovery: use
  `eventually_`/`finally_` commands or `ANTITHESIS_STOP_FAULTS` after healing the intake/partition.
- The intake-down scenario is also approximable **without** network faults by toggling
  `mock-intake` into reject/5xx/hang modes via a custom fault or an admin endpoint — useful where
  network-fault availability is limited.

## SDK selection

- **Workload client:** Rust — Antithesis Rust SDK (assertions + RNG for adversarial input
  generation: malformed packets, corrupt captures, config keys).
- **SUT (`adp`):** Rust — add the Antithesis Rust SDK for the **net-new SUT-side assertions** that
  catalog Categories C/E/F require (NaN-at-sketch-boundary, bin-count, interner-corruption,
  source-misroute, limiter-RSS-failure, replay-panic, and the aggregate divide-by-zero regression
  tripwire — now a closed vector, `NonZeroU64`). These internal
  states are invisible to a workload-only checker (see `property-catalog.md` "Catalog-wide notes").
  Build a dedicated ADP image with the SDK + Antithesis coverage instrumentation enabled.

## Assumptions & open questions

- **Standalone mode is acceptable for the primary topology.** `AGENTS.md` calls standalone "not for
  production," but it is the same mode the correctness suite uses and it removes the Core Agent from
  ~22 properties' state space. Config-stream properties use Add-on 1 with remote-agent mode. Confirm
  no standalone-only code path masks a production behavior we care about.
- **`datadog-intake` needs a controllable failure mode** (reject / 5xx / slow / hang, ideally
  toggleable at runtime) to drive the egress cluster without relying solely on network faults.
  Confirm whether the existing binary supports this or needs a small extension.
- **A minimal Core Agent config-stub** must be built (or the full `datadog-agent` image adapted) to
  send adversarial config the real Agent wouldn't — needed for Add-on 1.
- Whether the workload can drive DogStatsD over **UDP/TCP at the volume the driver targets** without
  loss confounding the assertions (UDP is lossy by nature; for no-loss assertions prefer TCP/UDS, and
  scope UDP cases to no-crash rather than no-loss).
- The `checks_ipc` Histogram NaN bypass (`ddsketch-no-nan-poison`) needs a **checks-IPC producer** in
  the topology (a check emitting a NaN histogram), which the DogStatsD-only primary topology lacks —
  add a minimal checks-IPC feeder or a unit-level SUT assertion for that one property.
