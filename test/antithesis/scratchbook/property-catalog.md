---
sut_path: /home/ssm-user/src/saluki
commit: 2e4ae1b8be45143882f0dbeb5e74998021c5faf9
updated: 2026-05-31
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: ADP Confluence space — headline guarantees and gap analyses that seed properties.
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/pages/6497671050/What+Comes+After+DogStatsD
    why: "ADP will not crash under load, losing customer data" — root guarantee for the memory + data-loss families.
  - path: https://datadoghq.atlassian.net/browse/DADP
    why: ADP Jira project for tracked gaps/regressions.
  - path: https://github.com/DataDog/saluki/pull/1768
    why: PR review #4393897611 (Copilot) — priority alignments and the aggregate-panic-fixed-upstream update reconciled here.
---

# Property Catalog: Agent Data Plane (ADP)

37 properties across 8 categories. The system makes one headline guarantee — **"ADP will not
crash under load, losing customer data"** — which decomposes into the *Memory & Resource Bounds*
and *Data Integrity & No Silent Loss* families. The remaining categories cover aggregation
correctness, lifecycle/config, untrusted-input parsing, concurrency, and **transform & enrichment
correctness** (Category G, added after evaluation — ADP as a *transformer*, not just a transport).

> **Evaluation note (2026-05-28):** an 4-lens portfolio evaluation added 8 properties (G1 events/
> service-checks; G2 transform-chain + runtime filter config-reload), applied 9 refinements, and
> escalated one scope bias (traces/APM/logs/OTLP coverage). See `evaluation/synthesis.md`.

**Instrumentation is mostly net-new, with the first pieces landed** (`existing-assertions.md`: 8 call
sites). Beyond the ADP bootstrap probe and workload-side anchors, two liveness pieces landed
2026-05-31: the external `eventually_adp_alive` command (Category H) and the **first in-SUT property
assertion** — an `assert_sometimes!` at the forwarder 2xx site in `saluki-components`. Every other
`Invariant` below is still **net-new** SUT-side instrumentation. Several properties are **expected to
fail by design** under default config (memory limiter disabled, interner heap-fallback enabled, disk
persistence off) — these are flagged; they are the highest-value findings, not catalog errors.

Provenance tags `[Fn]` after each slug name the discovery focus that surfaced it:
`[RB]` resource boundaries, `[DL]` data-loss/recovery, `[AG]` aggregation/sketch,
`[LC]` lifecycle/config, `[RC]` replay/codec/concurrency, `[WC]` wildcard (from SUT analysis).

---

## Category A — Memory & Resource Bounds

The "deterministic resource usage" / "won't OOM" half of the headline guarantee. Critical finding:
the bound is a **startup assertion about declared sizes**, not a runtime invariant; the runtime
limiter is advisory (≤25ms backoff, 250ms sampling, cooperative), disabled by default, and the
interner spills to the heap by default. This category probes whether RSS is *actually* bounded.

### rss-bounded-under-cardinality — RSS bounded under high cardinality
> **Status (2026-05-29): WORKLOAD WIRED + ROOT CAUSE REPRO'D** — `parallel_driver_send_dogstatsd`
> (high-cardinality regime) floods distinct contexts in the Antithesis harness to drive this
> behavioral bug under a run; and the root cause is reproduced as a unit test in
> `lib/saluki-context/src/resolver.rs`
> `tests::bug_default_heap_fallback_makes_context_resolution_unbounded` (default heap fallback ⇒
> resolution never refuses ⇒ unbounded memory). Not fixed.
| | |
|---|---|
| **Type** | Safety (expected to FAIL by design under default config) |
| **Property** | Under a high-cardinality tag / many-distinct-timestamp flood, process RSS stays within the configured memory grant. |
| **Invariant** | `Always(rss <= grant.effective_limit_bytes() * tolerance)`, read from the same `process_memory::Querier` the limiter uses. `Always` fits — RSS-within-grant must hold on every check. SUT-side `Sometimes(backoff_applied && rss_still_climbing)` localizes the mechanism better than a workload RSS probe. |
| **Antithesis Angle** | 250ms RSS sampling + 25ms max advisory backoff (from 95%) means bursts blow past the limit between samples; only the source cooperates, the aggregate hot path never calls `wait_for_capacity`, and the interner heap-fallback default makes growth effectively unlimited. Scheduling/timing exploration surfaces the burst-vs-sample race the deterministic harness can't. |
| **Why It Matters** | Directly tests "won't crash under load" / deterministic resource usage; failure means OOM under cardinality floods. |
| **Priority** | High |

**Open Questions:**
- Acceptable RSS tolerance band over `effective_limit_bytes` (95%+25ms+250ms implies real overshoot even when healthy).
- With `allow_context_heap_allocs=false`, does RSS actually become bounded, or do aggregate-map / per-context `SmallVec`s still escape under the many-timestamp flood? `(partial: context count is bounded to context_limit exactly — confirmed; per-value memory still unaccounted)`
- Would the container OOM-kill ADP before the assertion fires (crash vs. over-grant reading)?
- _Resolved:_ nothing enables a memory limit by default — `memory_mode` defaults to `Disabled` (limiter is a no-op); cgroup auto-detect requires a non-empty `DOCKER_DD_AGENT` env var AND `memory_limit` unset AND a successful cgroup parse (`accounting.rs:107-121`). Confirms the fails-by-design framing.

### aggregate-context-limit-enforced — Aggregate context limit enforced
| | |
|---|---|
| **Type** | Safety (expected to HOLD) |
| **Property** | The aggregation map never exceeds `aggregate_context_limit` (default 1,000,000) live contexts; over-cap new contexts are dropped-and-counted; existing contexts always merge. |
| **Invariant** | `Always(contexts.len() <= context_limit)` — true `Always` (no path grows past the cap). Plus `AlwaysOrUnreachable(existing context never dropped by cap)`, and `Sometimes(context_limit_breached)` / `Sometimes(events_dropped_on_cap)` to prove the boundary is reached (avoid vacuity). |
| **Antithesis Angle** | Interleave a cardinality flood with flush timing and counter zero-value keep-alives (kept-alive counters still occupy slots) to exercise hitting/clearing the breach flag and re-admitting contexts — timing-sensitive. |
| **Why It Matters** | This hard, always-on, lock-free cap is the central memory-determinism lever for the aggregator (the one non-advisory runtime bound). |
| **Priority** | High |

**Open Questions:**
- _Resolved:_ the true live bound is exactly `context_limit` (not `limit + zero_value_count`). Zero-value keep-alive counters stay as ordinary entries in the single `contexts` map and DO count toward the cap (`mod.rs:568`; test `context_limit_with_zero_value_counters` at `mod.rs:1104-1157`); `len()` drops in the flush removal pass (`mod.rs:703-707`) when entries are empty AND past `counter_expire_secs`. The `Always(len <= context_limit)` target is correct.
- Caveat (not a question): the cap counts contexts, not bytes — one context with many timestamped values is one entry but unbounded value memory; prose must not overclaim "bounded memory" (see `rss-bounded-under-cardinality`).

### interner-full-bounded — Interner-full bounded vs. heap spill
> **Status (2026-05-29): BUG DEMONSTRATED** as a unit test (shared with `rss-bounded-under-cardinality`) —
> `lib/saluki-context/src/resolver.rs` `tests::bug_default_heap_fallback_makes_context_resolution_unbounded`
> shows the default heap-allowed mode never refuses (unbounded) while heap-disallowed refuses (bounded
> but lossy). Not fixed.
| | |
|---|---|
| **Type** | Safety (heap-disallowed: HOLDS; heap-allowed default: FAILS the bounded reading) |
| **Property** | When the fixed-size interner is full and heap allocations are disallowed, context resolution fails deterministically (metric dropped) instead of allocating; with heap allowed (the default), memory is no longer bounded. |
| **Invariant** | Heap-off: `AlwaysOrUnreachable(interner_full ⇒ metric dropped, no heap alloc)` (rare/optional path). `Sometimes(try_intern == None)` proves exhaustion is reached. Heap-on: `Sometimes(intern_heap_fallback > 0)` proves the unbounded spill path is reachable under default config. SUT-side needed to distinguish interned / inlined / heap-fallback / dropped. |
| **Antithesis Angle** | Small interner + high-cardinality flood fills the buffer; timing exploration stresses the loom-tested reclamation/tombstone path under concurrent intern-vs-drop. |
| **Why It Matters** | Interner determinism is the foundation of the context memory bound; the default flag flips ADP into the unbounded branch, voiding the bounded-memory guarantee silently. |
| **Priority** | High |

**Open Questions:**
- Does fragmentation make `try_intern` return `None` below nominal byte capacity under churn (earlier spill than the budget implies)?
- Is the real bound the sum across name + tag interning (they share one interner)?
- _Resolved:_ `dogstatsd_allow_context_heap_allocs` defaults to **true** (`sources/dogstatsd/mod.rs:149-151`; resolver fallback also true, `resolver.rs:258`); bounded mode (`with_heap_allocations(false)`) appears **only in `#[cfg(test)]`**. So bounded mode is opt-in/test-only and "bounded memory" is aspirational under default config.

### memory-limiter-survives-rss-read-failure — Memory limiter survives RSS read failure
| | |
|---|---|
| **Type** | Safety / fault-tolerance (expected to FAIL by design) |
| **Property** | If RSS becomes unreadable mid-run, memory protection remains active (or the failure is surfaced) rather than silently freezing. |
| **Invariant** | `Unreachable("limiter RSS read failed — protection lost")` at the `.expect()` site (`limiter.rs:100-102`) — the panic-the-checker-thread state is a critical failure that must never be observed (today it can be). Fix-dependent alternative: `Sometimes(rss_read_failed_and_surfaced)` + liveness check that `active_backoff` is still being updated. Needs SUT-side instrumentation. |
| **Antithesis Angle** | Inject `/proc`/RSS read failure mid-run; the damaging race is reads failing *before* RSS crosses threshold, freezing backoff at 0 (fail-open). The bare `std::thread` death doesn't trigger process shutdown — silent. |
| **Why It Matters** | The limiter is already the only runtime memory mechanism; silently disabling it removes the last guard against OOM under load. |
| **Priority** | **High** (R9; upgraded — the user confirmed custom `/proc` faults are enabled for the tenant, so the failure state is reachable). Still requires the limiter to be explicitly enabled and a SUT-side assertion, since the bare-thread death is otherwise unobservable. |

**Open Questions:**
- Can `Querier::resident_set_size()` actually return `None`/error *after* succeeding at startup on the Antithesis Linux target, or only via injected `/proc` corruption? Pivotal for priority.
- Is the frozen backoff more likely 0 (fail-open) or nonzero (fail-stuck, over-throttle)? Opposite symptoms.
- Should correct behavior be "keep last-known protection" or "fail loudly and restart" (data components are fail-stop; s6 restarts ADP)? Changes Unreachable(panic) vs. Reachable(clean restart) framing.

### retry-queue-bounded-under-outage — Retry queue bounded under outage
| | |
|---|---|
| **Type** | Safety (byte cap) + Liveness (bound implies counted data loss) |
| **Property** | During a prolonged intake outage the forwarder retry queue (in-memory + disk) stays within its configured byte caps; overflow drops oldest (counted), never grows unbounded. |
| **Invariant** | `Always(total_in_memory_bytes <= max_in_memory_bytes)` and the analogous disk-bytes `Always` (eviction loops make these true invariants). `Sometimes(items_dropped > 0)` / `Sometimes(persisted_entries_dropped > 0)` prove saturation is reached and the bound is load-bearing. Optional `Reachable` on the "entry too large to ever fit" branch. |
| **Antithesis Angle** | Hold mock intake down (refused / black-hole / 5xx / slow) under sustained load until the queue saturates; interleaving stresses the shared circuit-breaker backoff + per-endpoint queues + disk eviction. |
| **Why It Matters** | Tests "won't crash, won't lose data" at its sharpest tension — memory bounded by design means prolonged outage forces counted-but-real data loss. |
| **Priority** | High |

**Open Questions:**
- Assert per-endpoint cap or aggregate `num_endpoints * cap` (+ disk)? Fan-out multiplies the bound; matters for RSS protection.
- Is there a time-based eviction policy (queue-duration) beyond the byte cap, like the Agent's `queue_duration_capacity`?
- _Resolved:_ corrupt/torn files are dropped and skipped without wedging the queue, and the byte cap holds across drops (see `disk-persisted-retry-survives-restart`); the disk-init-failure fallback keeps the in-memory byte cap and is surfaced only via an `error!` log.

---

## Category B — Data Integrity & No Silent Loss

The "won't lose customer data" half of the headline. Covers the internal backpressure path, egress
delivery, crash durability, event routing, and shutdown drain.

### no-silent-interconnect-drop — No silent inter-component drop on a wired edge
| | |
|---|---|
| **Type** | Safety |
| **Property** | Under sustained load with a slow downstream, a correctly-wired interconnect edge applies backpressure (await) and never silently discards events; backpressure propagates back to the socket. |
| **Invariant** | `Always(events_discarded_total delta == 0)` for a connected output under load. The discard branch fires only when `senders.is_empty()`, so a wired edge can never discard. Pair with `Sometimes(backpressure engaged)` so the test proves it reached the full-channel state. Not a blanket `Unreachable` on the discard site — disconnected outputs discard legitimately. |
| **Antithesis Angle** | Throttle the forwarder/intake so encoder→forwarder fills, cascading backpressure up every bounded mpsc edge to the DSD read loop; verify queue-and-await instead of drop. |
| **Why It Matters** | Directly the "no silent loss" half of the headline guarantee on the hottest internal path. |
| **Priority** | High |

**Open Questions:**
- Do any production DSD outputs ever have zero senders (e.g. conditional `dsd_debug_log_out`/`dsd_stats_out`)? If so, scope `Always` to always-wired outputs.
- _Resolved:_ `interconnect_capacity` default is **128** event buffers (`DEFAULT_INTERCONNECT_CAPACITY`, `topology/mod.rs:37`; every non-source edge is `mpsc::channel(128)`). No ADP override. This sizes the burst before backpressure; tunes the test, not the property.
- Exclude the non-atomic multi-sender partial-delivery case (a closed channel at teardown, not a full one) from the assertion window.

### forwarder-eventual-delivery — Eventual delivery after transient intake outage
> **Status (2026-05-29): PARTIALLY WIRED** — `finally_verify_delivery` asserts the fault-free
> eventual-delivery liveness baseline (`Sometimes(delivered>0)`); the post-outage-recovery facet
> (inject a 5xx/timeout storm, then heal) is a later iteration.
| | |
|---|---|
| **Type** | Liveness |
| **Property** | After a transient intake outage (5xx/timeouts/connection resets) clears, every accepted retryable transaction is eventually delivered, provided the retry queue did not overflow. |
| **Invariant** | `Sometimes(all-accepted-retryable-delivered-after-recovery)`: at least once, post-recovery delivered count equals accepted-retryable count submitted before/during the outage (no overflow). Plus `Reachable` on the `Error::Open` re-enqueue site to prove the breaker engaged. Liveness → assert progress, not an instantaneous invariant. |
| **Antithesis Angle** | Inject a bounded 5xx/timeout/connection-reset storm, then restore 2xx; circuit-breaker backoff + re-enqueue to low-priority queue must recover the backlog. |
| **Why It Matters** | Egress data-loss surface; retry path only unit-tested against in-process mocks with a virtual clock. |
| **Priority** | High |

**Open Questions:**
- Size the outage shorter than `queue_max_size_bytes` overflow, or exclude oldest-dropped txns; overflow is the intended bounded-memory escape valve (a test-setup constraint, not a property uncertainty).
- _Resolved:_ production forwarder requests are always `Clone` (`FrozenChunkedBytesBuffer`; `clone_request` returns `Some` unconditionally) → retryable failures always take the `Error::Open` re-enqueue path, never the non-cloneable `Error::Service` drop.
- _Resolved:_ the circuit-breaker backoff is **per-endpoint** (each `run_endpoint_io_loop` builds its own `Arc<Mutex<State>>`), so one slow endpoint cannot serialize others' recovery.

### disk-persisted-retry-survives-restart — Disk-persisted retry survives kill+restart
| | |
|---|---|
| **Type** | Liveness (with no-duplication + poison-drop safety sub-clauses) |
| **Property** | With disk persistence enabled, retry-queue transactions survive a process kill+restart and are eventually delivered with no systemic loss or duplication; corrupt entries are dropped, not retried forever, without aborting recovery. |
| **Invariant** | `Sometimes(persisted-backlog-fully-recovered)`: post-restart delivered set covers the persisted backlog, deduped. `AlwaysOrUnreachable(poison-dropped)`: any corrupt on-disk entry is dropped and recovery proceeds — never infinite retry, never abort. `Reachable(persistence-active)`: the silent in-memory fallback did NOT fire, else the test is vacuous. |
| **Antithesis Angle** | SIGKILL mid-outage (s6 restarts ADP), restore intake, reconcile delivery; separately inject a corrupted on-disk entry to exercise poison handling and torn-write recovery. |
| **Why It Matters** | Durability across crash is never tested system-level; delete-before-return and silent fallback are subtle correctness levers. |
| **Priority** | High |

**Open Questions:**
- At-most-once window: delete-before-return then crash-before-send loses one in-flight txn — the reconcile must tolerate small (~per-endpoint-concurrency) slack, not assert exact equality.
- _Resolved:_ a torn/partial write across a crash yields a valid filename + truncated content → `serde_json` fails → entry **dropped** (warn + `entries_dropped++`) on read; the scan `continue`s past any number of bad files, so **one bad file never wedges recovery** and the byte cap is never violated (dropping decrements the total). Note: `push` writes non-atomically straight to the final path (`persisted.rs:184`, despite a stale "temporary file" comment).
- _Resolved:_ the disk-init-failure → in-memory fallback is surfaced **only as an `error!` log (io.rs:406), no metric** — the workload must log-scrape or `assert_unreachable` at the fallback to keep the durability test non-vacuous; the in-memory byte cap still holds after fallback.

### source-dispatch-no-misroute — DSD source dispatch never mis-routes events
| | |
|---|---|
| **Type** | Safety |
| **Property** | A mid-buffer dispatch failure in the DogStatsD source never mis-routes eventd/service-check events into the metrics output path (or vice versa). |
| **Invariant** | **(R8) Primary facet — silent loss:** `Sometimes(dispatch_failed)` + an SUT-side check that a dispatch failure is **counted** (today the `error!` logs at `mod.rs:1688/1702/1713` may be the only signal — unlike interconnect `events_discarded_total`). **Secondary facet — misroute:** `Unreachable(misroute)` (an eventd/service-check Event reaching the metrics dispatch, or vice versa) — structurally improbable with the current `extract`-then-`send_all` ordering, so it functions as a future-refactor guard, not the live hazard. |
| **Antithesis Angle** | Mixed eventd+service-check+metric buffer while forcing a downstream output to error mid-`send_all`; verify failures are accounted (not silently dropped) and no cross-output leakage. |
| **Why It Matters** | The live hazard is **silent, uncounted loss** on dispatch failure (a finding); wrong-stream delivery would additionally corrupt data semantics, and `.expect()` on outputs is a latent crash. |
| **Priority** | Medium |

**Open Questions:**
- Can `extract` (swap_remove_back + collected indices) ever leave a matching event behind? This is the crux — if extraction is correct, misroute is impossible.
- Is dispatch-failure loss counted anywhere? The `error!` logs may be the only signal — possibly fully silent (a finding).
- Does a persistent downstream failure wedge the source ("until the process is restarted"), feeding the fail-stop story?

### shutdown-drains-no-loss — Shutdown drains accepted events to the forwarder
| | |
|---|---|
| **Type** | Liveness (with a safety boundary at the 30s timeout) |
| **Property** | Every event accepted before the shutdown signal that has reached a flushed window is forwarded before clean shutdown, provided the topology drains within the 30s grace window. |
| **Invariant** | `Sometimes(clean-drain-no-loss)`: at least once, shutdown completes cleanly within 30s and all pre-signal flushed-window events reach the mock intake. `AlwaysOrUnreachable(timeout-implies-forceful)`: whenever the 30s timeout fires, the run loudly reports forceful stop — in-flight loss is never silent. NOT a blanket Always-no-loss: open-window default-drop and forceful-stop are designed losses. |
| **Antithesis Angle** | Shutdown under load; combine with a slow/blocked intake to push past 30s and exercise the forceful-stop boundary; with disk persistence on, verify backlog persists instead of being lost. |
| **Why It Matters** | Graceful-shutdown drain is a stated guarantee; interacts with forwarder/disk-persistence at the timeout boundary. |
| **Priority** | High |

**Open Questions:**
- Does the source finish dispatching its current read buffer before signaling done, or are just-accepted events still in the source lost on clean shutdown?
- Does the final aggregate flush on stream close emit only closed windows, and do they clear the encoder→forwarder before the deadline under load?
- Realistic drain time at max load with healthy intake — if it approaches 30s, the clean case is fragile and 30s adequacy is itself a finding.
- Are PassthroughBatcher-buffered (pre-timestamped) metrics flushed on stop or dropped?

### events-sc-no-silent-loss — Events and service-checks delivered without silent loss
| | |
|---|---|
| **Type** | Liveness (with a Safety no-silent-drop clause) |
| **Property** | Under intake backpressure/outage, the events and service-checks sub-paths apply backpressure (never silently drop on a wired edge) and, after a transient outage clears, every accepted event/SC that did not legitimately overflow is eventually delivered. |
| **Invariant** | Safety `Always(no silent drop on a wired events/SC edge under load)` + Liveness `Sometimes(all-accepted-events-delivered-after-recovery)` / `Sometimes(all-accepted-SC-delivered-after-recovery)` reconciling `component_events_received_total{message_type}` vs `events_sent`. Two silent-loss sites need SUT-side coverage: the encoder recoverable-error drop (`events/mod.rs:179-194`, undercounted) and the wrong-type swallow (`try_into_eventd`→`Continue`, uncounted). No aggregation stage → ~1:1 accept→deliver (modulo `MAX_EVENTS_PER_PAYLOAD=100`), a cleaner reconcile than metrics. |
| **Antithesis Angle** | Throttle/down the mock intake so the always-on `dsd_in.{events,service_checks} → *_enrich → dd_*_encode → dd_out` edges fill; verify queue-and-await, then restore and reconcile. Extends `no-silent-interconnect-drop` / `forwarder-eventual-delivery` to two always-on edges the catalog ignored. |
| **Why It Matters** | The "won't lose customer data" guarantee on two always-on production paths no other property watches. |
| **Priority** | High |

**Open Questions:**
- Is the encoder recoverable-error branch ever hit on healthy intake with well-formed events, or only on oversized single events? (Decides the optional `Always` guard.)
- Do events/SC retries share `dd_out`'s per-endpoint queue with metrics (cross-stream eviction by a metric flood)?
- Are events/SC requests `Clone` (retryable failures take the re-enqueue path), as confirmed for metrics?
- Does `dispatch_events` count anything when `send_all` errors, or is dispatch-time loss fully silent? (ties to `source-dispatch-no-misroute`)

### events-sc-pipeline-reachable — Events and service-check sub-pipelines are actually exercised
| | |
|---|---|
| **Type** | Reachability |
| **Property** | At least once per run, a well-formed event and a well-formed service-check are parsed/accepted at the source and delivered through the encoder to the intake — so the event/SC safety/liveness properties cannot pass vacuously. |
| **Invariant** | `Sometimes(event_parsed_and_accepted)` / `Sometimes(service_check_parsed_and_accepted)` (source `component_events_received_total{message_type=events|service_checks}`) + `Sometimes(event_delivered)` / `Sometimes(service_check_delivered)`. Strengthen to `Reachable` if the workload guarantees ≥1 well-formed event + SC. This is the **R4 anti-vacuity anchor** for `events-sc-no-silent-loss` and `malformed-event-sc-no-crash`. |
| **Antithesis Angle** | Anti-vacuity anchor for a metrics-dominated workload; also catches a wiring/`EnablePayloads`-default regression that silently removes an always-on path. |
| **Why It Matters** | The catalog is otherwise metrics-only; events/SC are rare in real traffic, so the new event/SC properties need an explicit reachability obligation to mean anything. |
| **Priority** | Medium |

**Open Questions:**
- Delivery anchor on encoder `events_sent` vs the mock intake receiving the `/api/v1/events_batch` / service-check POST? Intake observation is stronger but needs the mock to distinguish endpoints.
- One anchor per stream, or four (event/SC × parsed/delivered) to localize a parse-but-not-deliver regression?

---

## Category C — Aggregation & Sketch Correctness

ADP must match the Datadog Agent bit-for-(approximately-)bit. The diff-test suite checks happy-path
equivalence; these properties target correctness under faults, edge cases, and timing — plus a
guaranteed-crash clock hazard (`aggregate-clock-skew-stable` forward-jump) the suite cannot reach.
(The former sub-second-window divide-by-zero crash is now fixed upstream, PR #1772, and survives only
as a regression tripwire.)

### aggregate-matches-agent — Aggregated output matches the Datadog Agent
| | |
|---|---|
| **Type** | Safety |
| **Property** | For the same input stream, ADP's aggregated output (counter→rate via bucket width, half-open `[start,start+width)` buckets, histogram/distribution stats) equals the Datadog Agent's, and equivalence is preserved under faults. |
| **Invariant** | Workload/harness-side `Always(diff within ratio)` on the normalized `stele` diff per flush window, with faults active; `Sometimes(fault injected during window)`. Differential property — anchored on the `panoramic`/`stele` diff harness, not a single in-process assertion. |
| **Antithesis Angle** | Diff-test equivalence under delayed/skipped flush, process kill+restart mid-window, and downstream backpressure — faults the deterministic `panoramic` harness cannot inject. |
| **Why It Matters** | Wrong aggregates are silent, customer-visible data corruption; the headline correctness guarantee. |
| **Priority** | High |

**Open Questions:**
- Can `panoramic` survive an ADP restart mid-run, and is `FLUSH_WAIT=32s` enough once faults delay flushes (timing-artifact false diffs)?
- Is the Agent baseline's bucket width guaranteed identical to ADP's `aggregate_window_duration`?
- Are zero-value counters emitted identically by both sides across a skipped flush?

### aggregate-no-panic-any-window — No window duration causes a panic
> **Status (2026-05-30): FIXED UPSTREAM on main** — the `% 0` panic vector is now structurally
> impossible. The config key is renamed `aggregate_window_duration_seconds` and deserializes as
> `NonZeroU64` (`transforms/aggregate/mod.rs:95-98`); `align_to_bucket_start` takes a `NonZeroU64`
> and divides by `bucket_width_secs.get()` (`:822-823`), so zero/sub-second values fail config
> parsing instead of reaching the divisor (PR #1772). The earlier repro
> `tests::bug_sub_second_aggregate_window_panics_on_insert` is therefore **stale** — see the bug
> ledger (the repro lives in a sibling commit and should be dropped or converted to a passing guard).
> Property retained as a low-cost regression tripwire.
| | |
|---|---|
| **Type** | Safety / Reachability |
| **Property** | No `aggregate_window_duration_seconds` value causes a panic; the divisor is never zero. |
| **Invariant** | `Unreachable("align_to_bucket_start reached with bucket_width_secs == 0")` as a regression tripwire — should never fire now that the type is `NonZeroU64`. Fires only if a future refactor reintroduces a zero-able window or a finer-grained (sub-second) divisor. |
| **Antithesis Angle** | Cheap: explore the (now `NonZeroU64`) config space and confirm no divisor-zero path is reachable. Primary value is guarding against a regression, not finding the original (closed) bug. |
| **Why It Matters** | The original guaranteed crash + restart loop is closed; the tripwire keeps it closed. |
| **Priority** | Low (R-2026-05-30: demoted from High — panic vector closed upstream; retained as a regression guard). |

**Open Questions:**
- _Resolved (2026-05-30):_ the fix landed as a type change (`NonZeroU64`), not runtime clamping, so zero/sub-second windows are rejected at config load.
- Can the gRPC dynamic-config stream push `aggregate_window_duration_seconds` at runtime? Even so, a non-`NonZeroU64` value cannot deserialize, so there is no live divisor-zero vector.

### aggregate-clock-skew-stable — Aggregation stays sane across wall-clock skew
> **Status (2026-05-29): BUG DEMONSTRATED** (forward-jump facet) as a unit test —
> `lib/saluki-components/src/transforms/aggregate/mod.rs`
> `tests::bug_forward_clock_jump_floods_zero_value_points`. A forward wall-clock jump makes `flush`
> build `zero_value_buckets` over the whole jumped interval (O(jump) work/alloc) and flood one idle
> counter with points proportional to the jump. Backward-jump gap facet not yet repro'd. Not fixed.
| | |
|---|---|
| **Type** | Safety |
| **Property** | A backward/forward wall-clock jump never floods zero-value points nor silently breaks counter continuity; bucketing stays bounded and well-formed. |
| **Invariant** | `Always(zero_value_buckets.len() <= ceil(flush_interval/window)+slack)` and `Always(current_time >= last_flush)` inside `flush`; `Sometimes(clock jumped during flush)`. CONFIRMED two-clock hazard: wall-clock bucketing vs monotonic flush cadence, no monotonicity guard; forward jump floods the zero-value loop, backward jump empties it. |
| **Antithesis Angle** | Clock fault injection (NTP step backward/forward) during a steady counter stream. |
| **Why It Matters** | Metric flood + memory spike (forward) or silent continuity gap and mis-expiry (backward); diverges from Agent. |
| **Priority** | High |

**Open Questions:**
- Fix: monotonic source vs clamp `current_time = max(., last_flush)` + cap loop? Decides `Unreachable(flood)` vs `Always(bounded)`.
- Any guard against `get_unix_timestamp()` returning 0 (pre-epoch via `unwrap_or_default`)? None found.
- Does the Agent behave identically under the same step (ties to `aggregate-matches-agent`)?

### ddsketch-bin-count-bounded — Bin count never exceeds bin_limit
| | |
|---|---|
| **Type** | Safety |
| **Property** | After any inserts / multi-weight inserts / interpolations / merges, an agent `DDSketch`'s bin count never exceeds `bin_limit` (4096). |
| **Invariant** | `Always(self.bins.len() <= bin_limit)` after every mutating method (post-`trim_left`); `Reachable("trim_left collapsed bins")`. CONFIRMED enforced today by `trim_left` at every mutation site; already has unit + proptests. SUT-side. |
| **Antithesis Angle** | Live regression tripwire on real sketches after arbitrary interleavings (histogram→distribution, cross-window merges) — catches a new mutator that forgets `trim_left`. The `Reachable(trim_left collapsed bins)` anchor is **essential** or the `Always` is vacuous (real corpora rarely exceed 4096 keys). |
| **Why It Matters** | Bin explosion → memory blowup and historically an encoder panic. |
| **Priority** | Medium (R6: demoted from High — substantially duplicates existing proptests; the unique value is a live tripwire for a future `trim_left`-forgetting mutator). |

**Open Questions:**
- Is test-only `insert_raw_bin` (bypasses `trim_left`) truly unreachable in release?
- _Resolved:_ only the **agent sketch (4096)** is on the live path; `ddsketch::DDSketch` re-exports the agent impl (`lib.rs:56`). The canonical sketch (2048 bins) has no non-test usage. Assert against the agent `bin_limit` (4096).

### ddsketch-relative-error-bound — Quantile accuracy + merge associativity
| | |
|---|---|
| **Type** | Safety |
| **Property** | For in-range values, the sketch's quantile estimates are within configured relative error (eps≈0.78%) and merges are associative/commutative — a **library invariant** of the agent DDSketch, since ADP does not query quantiles on the live path (see below). |
| **Invariant** | Harness/SUT-side `Always(|q_est - v| <= eps_rel*|v|)` for in-range inputs and `Always(merge order-independent within tolerance)`, exercised against the agent sketch directly (not a production runtime call). The lower-value live facet — faithful bin serialization + bin count ≤ 4096 — is already covered by `ddsketch-bin-count-bounded`. eps=1/128, gamma=1+2·eps confirmed; `avg`/`sum` in `merge` are f64 order-sensitive. |
| **Antithesis Angle** | Merge order under interleaving (delayed flush / backpressure reordering) and accuracy at the representable-range boundary — invisible to the single-order diff test. |
| **Why It Matters** | Quantile/avg drift is silent wrong customer data. |
| **Priority** | Low (R5: demoted Medium→Low — the accuracy guarantee is a library/proptest invariant, not a live ADP runtime invariant; ADP never calls `DDSketch::quantile` on the customer path). |

**Open Questions:**
- Acceptable f64 tolerance for `avg`/`sum` under reordered merges vs diff-test's 1e-8?
- _Resolved:_ ADP does **not** call `DDSketch::quantile` on the live customer path — histogram percentiles use raw-sample `HistogramSummary::quantile`; distribution sketches are serialized as raw bins and quantiled server-side. Only the agent sketch is live. So this property is a library/harness-side invariant, not a production runtime assertion.

### ddsketch-no-nan-poison — NaN never silently poisons sum/avg
> **Status (2026-05-29): BUG DEMONSTRATED** as a unit test —
> `lib/ddsketch/src/agent/sketch.rs` `tests::bug_nan_sample_poisons_sum_and_avg`. A single NaN sample
> permanently poisons `sum`/`avg` (sticky) while `count`/`min`/`max` stay valid (silent corruption);
> no finiteness guard at the sketch boundary. Not fixed (demonstration only).
| | |
|---|---|
| **Type** | Safety / Reachability |
| **Property** | A single NaN sample must never silently poison a sketch's `sum`/`avg`; for finite input, sum/avg stay finite, and the boundary rejects/sanitizes non-finite values. |
| **Invariant** | `Always(v.is_finite())` at `adjust_basic_stats` entry (or `Unreachable` on absorbed-NaN path) + backstop `Always(self.sum.is_finite())`. CONFIRMED no finiteness guard in `adjust_basic_stats`; `sum += v*n` makes NaN sticky; `key(NaN)` still yields a valid bin; `insert_n` called directly from aggregate. Guarded only per-source. SUT-side. |
| **Antithesis Angle** | **A confirmed LIVE bypass:** a `checks_ipc` Histogram metric carrying a NaN value (`checks_ipc/mod.rs:195`, no finiteness check) routes `checks_ipc_in.metrics → metrics_enrich → dd_metrics_encode`, bypassing both the DSD `FloatIter` and the aggregate transform, and reaches `ddsketch.insert_n(...)` in the Datadog metrics encoder (`encoders/datadog/metrics/mod.rs:1054`) → `adjust_basic_stats` → `sum += NaN`. Poison then propagates through `merge`. |
| **Why It Matters** | Permanent silent corruption of sum/avg across sketches and downstream — and it is reachable today, not hypothetical. |
| **Priority** | High |

**Open Questions:**
- Fix policy: reject/skip (match codec drop) vs clamp; confirm against Agent baseline.
- _Resolved:_ the hazard is **LIVE** via `checks_ipc` Histogram metrics (above). OTLP is closed (number values filtered by `is_skippable`; histogram sketches reconstruct finite bounds) and the DSD `aggregate insert_n` path is closed (DSD-only, finiteness-filtered upstream). The robust fix/assertion belongs at the **sketch boundary** (`adjust_basic_stats`/`insert*`), justified by the concrete checks_ipc bypass. Target sum/avg (not `quantile`, which has its own NaN fallback); ±Inf is in scope (`is_finite` covers both).

---

## Category D — Lifecycle & Configuration

Startup ordering, the no-timeout config-stream wait, fail-fast on incompatible config, bounded
graceful shutdown, and the fail-stop model (data components are not restarted).

### topology-ready-before-intake — Topology becomes ready before data intake begins
| | |
|---|---|
| **Type** | Liveness + Safety (ordering) |
| **Property** | The internal supervisor reaches `all_ready` before the data topology is built/spawned, and the full topology eventually reaches `all_ready`. |
| **Invariant** | `Always(internal-supervisor-ready precedes blueprint.build())` + `Sometimes(full topology reaches all_ready)`. Ordering of readiness milestones is the defensible guarantee. |
| **Antithesis Angle** | Stall a downstream component's readiness (forwarder blocked on dead intake at init) or fail an internal-supervisor child to init; verify build/spawn never precedes supervisor-ready. |
| **Why It Matters** | If the topology spawned before dependencies were ready, sources could read into an unwired/uninitialized pipeline. |
| **Priority** | High |

**Open Questions:**
- Does `dsd_in` bind/accept on its listeners *before* `mark_ready`? Determines whether the property strengthens from "milestone ordering" to "no socket bound pre-ready."
- Readiness is not latched; a late-registered component could drop `all_ready` back to false. Confirm all data components register before the wait.

### config-stall-no-deadlock — Config-stream stall does not deadlock or busy-loop startup
> **Status (2026-05-29): BUG DEMONSTRATED** as a unit test —
> `lib/saluki-config/src/lib.rs` `tests::bug_config_ready_hangs_forever_without_snapshot`. With
> dynamic config enabled and the sender held open but silent, `GenericConfiguration::ready()` never
> resolves (no internal timeout) → ADP startup would hang forever. Not fixed.
| | |
|---|---|
| **Type** | Liveness |
| **Property** | When the Core Agent config stream is delayed/dropped/erroring, ADP either progresses to "Initial configuration received" or remains quiescently blocked at the wait — never crashing or busy-looping. |
| **Invariant** | `Sometimes("config received")` + `Reachable("config wait entered")`; no panic, no busy-loop (workload-side CPU/log-rate check on the quiescent hang). CONFIRMED no timeout on `ready()` nor bootstrap registration await. |
| **Antithesis Angle** | Drop the snapshot (registered but never streamed) → **quiescent indefinite hang** at `ready().await` (no timeout) — the primary falsification target; flap stream → 5s reconnect. |
| **Why It Matters** | ADP assumes Core Agent reachability; a true indefinite hang with no timeout is operationally surprising (ADP never starts the pipeline, no diagnostic deadline). |
| **Priority** | High |

**Open Questions:**
- _Resolved (busy-loop hazard downgraded):_ a steadily-erroring stream does NOT busy-loop. The tonic `Streaming` yields at most one `Err` per stream (initial error fuses to `Terminated`; mid-stream error yields once then `None`), so the loop logs once, `continue`s once, then exits to the **5s sleep** (`remote_agent.rs:302`). No unbounded spin.
- _Resolved:_ `init_reg_rx.await` is always eventually resolved — `session_id` always starts empty so the first tick takes the register branch, which always sends Ok/Err on `initial_registration_tx`.
- Confirmed real gap: `GenericConfiguration::ready()` (`lib.rs:694-704`) and the bootstrap registration await have **no timeout** — an open-but-silent stream hangs startup forever by design.

### config-incompatible-refuses-start — High-severity incompatible config refuses to start
| | |
|---|---|
| **Type** | Safety (Reachability / Unreachable) |
| **Property** | ADP never spawns the data pipeline when a high-severity-incompatible non-default config key is present; it exits 1 instead. |
| **Invariant** | `Unreachable("pipeline spawned with high-severity incompatible non-default key")` + `Reachable("ADP refused to start")`. The gate `check_and_warn_config` runs **before** create_topology/build/spawn; its `Err` → `exit(1)`. |
| **Antithesis Angle** | Inject a current `Severity::High` non-default key; expect exit 1, no listener, no data. Negative controls: same key at default → starts; Medium/Low/Partial → starts. |
| **Why It Matters** | Running with an incompatible setting risks wrong aggregates / silent data corruption — fail-fast is the safety stance. |
| **Priority** | Medium (R7: demoted from High — a deterministic ordered gate already covered by the integration suite's config-check-exit-code cases; the `Unreachable` is statically unreachable, so the real artifact is the `Reachable(refused)` exploration). |

**Open Questions:**
- Confirm env-var overrides are visible to the classifier at the gate.
- _Resolved:_ partial config updates over the stream are NOT re-validated (the gate runs once at startup, `run.rs:157`; `ConfigClassifier` is referenced only in `run.rs`). This property is correctly scoped to **startup**; the runtime gap is now tracked as its own property `config-runtime-update-not-revalidated`.

### config-runtime-update-not-revalidated — Runtime config updates bypass the incompatibility gate
| | |
|---|---|
| **Type** | Safety (Reachability / scope gap) |
| **Property** | A high-severity-incompatible non-default config key delivered over the runtime config stream is never applied silently — or, if startup-only gating is intentional, the unguarded runtime-apply path is at least documented and observable. |
| **Invariant** | `Unreachable("pipeline running with high-severity incompatible non-default key after a runtime config update")`, or `Reachable` on the unguarded runtime-apply path if the design is startup-only. The startup gate (`check_and_warn_config`, `run.rs:157`) is the only classifier callsite; runtime `Partial`/`Snapshot` updates are applied without re-validation. |
| **Antithesis Angle** | Start ADP clean (passes the gate), then inject a config-stream update carrying a high-severity-incompatible non-default key; observe whether ADP refuses/flags or silently applies it. Exercises the control-plane → data-plane config path the diff-test never touches. |
| **Why It Matters** | Running with an incompatible setting risks wrong aggregates / silent data corruption — the exact outcome the startup gate prevents. The protection has a runtime hole. |
| **Priority** | Medium |

**Open Questions:**
- Is startup-only gating intentional (runtime updates trusted as authoritative from the Core Agent) or an oversight? `(needs human input)`
- Can a `Severity::High` key actually be delivered over the config stream, or does the Core Agent pre-filter what it sends to remote agents? Determines real-world reachability.

### graceful-shutdown-within-30s — Graceful shutdown completes within 30s without forceful kill
| | |
|---|---|
| **Type** | Liveness (bounded-time) + Reachability |
| **Property** | On SIGINT (or unexpected component finish) under bounded in-flight load, the data topology stops cleanly within the 30s grace window without the forceful-stop path. |
| **Invariant** | `Sometimes(clean topology shutdown completed)` + `Reachable(forceful-stop path under adversarial load)`. Distinct from `shutdown-drains-no-loss` (which owns *what data survives*); this owns *clean completion in time*. |
| **Antithesis Angle** | SIGINT under bounded load → clean exit; SIGINT with forwarder wedged on dead intake → forced-stop after 30s → exit 1; schedule a component to finish at the 30s boundary. |
| **Why It Matters** | Forceful kill drops in-flight data and risks state corruption; bounded clean shutdown is the operational contract. |
| **Priority** | High |

**Open Questions:**
- The **internal supervisor** shutdown has **no timeout** — the process could exceed 30s even when the topology met it. Scope assertions to topology-shutdown completion, or file a separate property.
- On forceful stop the `JoinSet` is dropped, aborting tasks; confirm no shared-state corruption (overlaps data-loss family).
- Confirm the `shutdown_coordinator` cascade reliably reaches every component.

### data-component-failure-triggers-process-shutdown — Data component finish triggers whole-process shutdown
| | |
|---|---|
| **Type** | Safety (Always) + Reachability |
| **Property** | Because data components have no restarting supervisor, any data component finishing unexpectedly deterministically triggers whole-topology shutdown — never a silent half-running pipeline. |
| **Invariant** | `Reachable(unexpected-finish → shutdown path)` + temporal `Always(component-death always followed by process exit)`. Data topology uses a plain `JoinSet` with no restart; supervisor restart is internal-only. |
| **Antithesis Angle** | Induce a component panic (hot-path `.expect`/`unreachable!`; note the former sub-second `aggregate_window_duration` panic vector is closed upstream) or a clean early finish; verify fail-stop fires and the process exits (s6 restarts it). |
| **Why It Matters** | A half-running pipeline silently drops/mis-routes data while appearing alive; fail-stop + full-process restart is the recovery model. |
| **Priority** | High |

**Open Questions:**
- Brief gap between spawn and the `select!`: a component dying there is still buffered by the `JoinSet` and caught once the select polls — confirm safe.
- `expect("no components to wait for")` panics on an empty topology; confirm a spawned topology is always non-empty (guarded by `data_pipelines_enabled`). Low priority.

---

## Category E — Untrusted Input Parsing

The DogStatsD codec and the new (zero-suite-coverage) capture/replay reader parse untrusted bytes.
Antithesis treats malformed input as a first-class fault dimension.

### malformed-dsd-no-crash — Malformed DSD packets never crash process/socket
| | |
|---|---|
| **Type** | Safety |
| **Property** | Malformed/adversarial DogStatsD packets on any listener never crash the process or kill a connectionless socket; bad packets are skipped/cleared-and-continued. |
| **Invariant** | `Always(process up)` + `Always(connectionless socket survives a bad packet)` + `Unreachable` at codec panic sites (`unreachable!`/`from_utf8_unchecked`); `Sometimes(framing_errors>0)`, `Sometimes(*_decode_failed>0)`. |
| **Antithesis Angle** | Untrusted packet input across UDP/TCP/UDS-dgram/UDS-stream; oversized / invalid-UTF8 / truncated-extension / NUL / huge-tag packets exploring codec + framing error paths. |
| **Why It Matters** | Clear-and-continue is the socket-survival mechanism; codec error policy is undecided (4 TODOs). Only a non-exhaustive proptest covers this today. |
| **Priority** | High |

**Open Questions:**
- Codec error policy for unknown/trailing chunks is undecided (silently permissive); resolving it changes expected-drop accounting, not no-crash.
- Does a malformed packet ever cause partial dispatch / mis-routing (ties to `source-dispatch-no-misroute`)?
- TCP oversized frame `break`s the connection — verify no per-connection resource leak.

### replay-no-panic-on-malformed-capture — Replay never panics on malformed capture
| | |
|---|---|
| **Type** | Safety |
| **Property** | Parsing an arbitrary/corrupt/truncated/zstd DogStatsD capture file never panics or aborts the replay process. |
| **Invariant** | `Unreachable` at any panic/abort in the reader or its zstd/prost decode calls; pair with `Reachable(replay-parse-executed)`. A panic on untrusted input is a crash, invisible to a workload that already expects a non-zero exit — needs SUT-side instrumentation. |
| **Antithesis Angle** | Untrusted file input + adversarial bytes / zstd-bomb / protobuf-recursion; pure input exploration of an unfuzzed, zero-suite-coverage path. |
| **Why It Matters** | Newest/largest ADP feature (+1765 LOC, validated only with `cargo check`); reader parses untrusted files inside an ADP process. |
| **Priority** | High |

**Open Questions:**
- _Resolved:_ replay runs as a **separate `agent-data-plane dogstatsd replay` CLI process** (`dogstatsd.rs:394`) that forwards records to the running ADP over the DSD UDS socket — so the panic-catch / SUT assertion belongs in the replay CLI process, not the data-plane process.
- _Resolved (now first-class hazards, not just questions):_ `reader.rs:40-41` `fs::read` loads the whole file with **no size guard** (OOM vector), and `reader.rs:44` `zstd::stream::decode_all` runs on untrusted input with **no decompressed-size cap** (decompression-bomb vector → unbounded memory on a valid-but-huge stream). Both overlap the memory family and are strong workload inputs.

### replay-corruption-not-silent-eof — Corruption distinguishable from clean EOF
> **Status (2026-05-29): BUG DEMONSTRATED** as a unit test —
> `lib/saluki-components/src/sources/dogstatsd/replay/reader.rs`
> `tests::bug_corrupt_length_prefix_silently_drops_following_records`. A corrupt/oversized length
> prefix is treated as clean EOF, silently dropping all following well-formed records. Not fixed
> (demonstration only).
| | |
|---|---|
| **Type** | Safety (data fidelity) |
| **Property** | A corrupt/oversized record length prefix is detectable as truncation, not silently reported as a clean replay completion. |
| **Invariant** | `AlwaysOrUnreachable(faithful completion)`: when `read_next` terminates with `Ok(None)`, the offset reached the real trailer separator, not an overrunning/mid-stream prefix; `Sometimes(corruption-detected)`. Honestly framed: code intentionally returns `Ok(None)` today (tests assert it). |
| **Antithesis Angle** | Untrusted file input; flip/zero a length prefix mid-stream and observe the tool report success having sent only N of M records. |
| **Why It Matters** | The reader collapses legitimate-EOF, truncation, and corruption into one `Ok(None)`; the driver stops sending silently → false replay-fidelity confidence. |
| **Priority** | Medium |

**Open Questions:**
- No record-count/total-length field exists — distinguishing truncation from clean EOF may need a format change; determines strict `Always` vs. heuristic.
- Maintainers may consider silent truncation acceptable (best-effort tool); the asserting tests suggest "accepted." `(needs human input)`
- A wrong-but-small `size` could decode garbage as a valid record — a third outcome (silent wrong record, not just truncation).

### malformed-event-sc-no-crash — Malformed event / service-check payloads never crash
| | |
|---|---|
| **Type** | Safety |
| **Property** | Adversarial/malformed DogStatsD event and service-check payloads on any listener never crash the process or kill a connectionless socket; bad frames are counted and skipped. |
| **Invariant** | `Always(process up)` + `Always(connectionless socket survives a bad event/SC packet)` + SUT-side `Unreachable` at any panic site in `parse_dogstatsd_event` / `parse_dogstatsd_service_check` and shared `helpers::*`; `Sometimes(event_decode_failed>0)`, `Sometimes(service_check_decode_failed>0)` as anchors. Extends `malformed-dsd-no-crash` to the two separate ~394/~312-LOC codecs (per R1, the no-crash check is SUT-side `Unreachable`, not container liveness). |
| **Antithesis Angle** | Untrusted event/SC frames across UDP/TCP/UDS: pathological `_e{title_len,text_len}` prefixes with `take()` on attacker lengths (`event.rs:36-49`), invalid UTF-8, the per-packet `.replace("\\n","\n")` heap alloc (amplification under flood), malformed `d:`/`card:` parsers, origin-detection-gated branches. |
| **Why It Matters** | These codecs are entirely separate from the metric codec, so existing coverage gives no assurance; a codec panic on any listener thread crashes a fail-stop data component → crash-loop under flood. |
| **Priority** | High |

**Open Questions:**
- Do the shared `helpers::*` parsers (`unix_timestamp`, `tags`, `cardinality`, `local_data`, `external_data`, `utf8`) contain any panic/unwrap/length-based pre-allocation? (Pivotal for the `Unreachable` guard.)
- Does `take(title_len)`/`take(text_len)` or the message UTF-8 parser pre-allocate on the declared length before bounds-checking the buffer?
- Can a malformed frame be mis-classified by `parse_message_type` (`mod.rs:1466`), incrementing the wrong decode-failure counter?

---

## Category F — Concurrency & Boundary Conditions

Interleaving-sensitive paths the deterministic diff-test cannot reach, plus the non-finite-value
boundary that cross-cuts intake and aggregation.

### interner-reclamation-no-corruption — No corrupt/overlapping interner entries under races
| | |
|---|---|
| **Type** | Safety |
| **Property** | Under concurrent intern + drop-last-ref, the interner never returns overlapping or corrupt (`0x21`-filled) entries; worst case is a benign duplicate. |
| **Invariant** | `Always(no reclaimed entry overlaps a live entry / no live &str reads the 0x21 sentinel)`, i.e. `Unreachable` on the corruption-detected branch; `Sometimes(reclaimed-slot reused)`, `Sometimes(drop re-check found resurrected entry)`. SUT-side. |
| **Antithesis Angle** | Concurrency/interleaving under the real scheduler + real load with many shards/entries on a near-full interner — explores beyond the bounded loom model. |
| **Why It Matters** | Most concurrency-unsafe component in the bounded-memory story; raw pointers as `'static &str` into a buffer overwritten on reclaim; loom only bounds interleavings. |
| **Priority** | High |

**Open Questions:**
- Confirm both the `try_intern` increment and the drop re-check take the same `InternerState` mutex (only race window is decrement→lock, which the re-check covers).
- Cross-shard handles: can a shard-A `InternedString` ever be dropped against shard-B's lock?
- _Resolved:_ the reclamation buffer-fill IS present in release (no cfg gate). **Two implementations use different sentinels:** `map.rs:392` fills `0x21`, `fixed_size.rs:458` fills `0xAA`. An assertion must use the correct sentinel per implementation, or check overlap directly (implementation-independent — preferred).

### non-finite-values-handled-consistently — Non-finite values consistent, no ghost metric
| | |
|---|---|
| **Type** | Safety |
| **Property** | Non-finite metric values never crash; an all-non-finite packet produces no downstream metric and consumes no interner/cache resources; no NaN reaches a DDSketch. |
| **Invariant** | `Always(value.is_finite() at DDSketch insert boundary)` + `AlwaysOrUnreachable(no zero-point metric reaches aggregation)` + `Sometimes(non-finite dropped)`. Add `Sometimes(ghost-metric path)` ONLY if a codec-bypassing producer is confirmed. |
| **Antithesis Angle** | Untrusted input: all-NaN/Inf and mixed packets across all metric types, checking the source `num_points==0` gate and the sketch boundary. |
| **Why It Matters** | NaN-poisons-sketch and ghost-metric hazards. Honestly framed: the **ghost-metric** shape (zero-point metric) is DSD-`FloatIter`-specific and gated (`num_points==0 → Ok(None)` at `handle_frame` `mod.rs:1478`), so on the DSD path it is expected Unreachable. The **NaN-poison** facet, however, is LIVE via a non-DSD producer (see `ddsketch-no-nan-poison`). |
| **Priority** | Medium |

**Open Questions:**
- Do the empty-iter `*_fallible` constructors return Err on empty input, or an empty value?
- _Resolved:_ the NaN-reaches-sketch hazard is LIVE via `checks_ipc` Histogram metrics (encoder `insert_n`), while OTLP, replay (re-injects through DSD codec), and the DSD aggregate path are all finiteness-gated. The ghost-metric (zero-point) shape stays DSD-specific and gated → expected Unreachable on the DSD path.

---

## Category G — Transform & Enrichment Correctness

Added after evaluation. The other categories treat ADP as a *transport* (don't crash, don't lose
bytes); this one treats it as a *transformer* — mapping, filtering, and tag-filtering customer data,
much of it driven by **runtime config that mutates while data flows**. This is the the design partner design-
partner's documented focus (the "Tag Filter RC Relay Stress Test"). These properties need the
**config-stream add-on topology** (not standalone) and/or the **diff-test add-on** (Agent baseline).

### mapper-output-matches-agent — DogStatsD mapper output matches the Datadog Agent
| | |
|---|---|
| **Type** | Safety |
| **Property** | For the same input names and `dogstatsd_mapper_profiles`, ADP's mapper produces the same rewritten metric name and injected tags as the Datadog Agent mapper. |
| **Invariant** | Differential `Always(mapped (name,tags) within ratio of Agent mapper)` per flush window on the `panoramic`/`stele` harness with a mapper-exercising corpus + identical profiles; `Sometimes(a metric was remapped)` for non-vacuity; SUT-side `Sometimes(cache hit == fresh miss)`. Mirrored expansion/wildcard logic (`dogstatsd_mapper/mod.rs:259-342`) is only self-tested today. |
| **Antithesis Angle** | Overlapping/ambiguous profiles probing first-match ordering, names at the wildcard char-class boundary, and fault-induced flush-timing skew; runs on the diff-test add-on. |
| **Why It Matters** | Mapper rename feeds every downstream filter (`run.rs:674-675`); a divergence is silent, customer-visible name/tag corruption. |
| **Priority** | High |

**Open Questions:**
- Does the Agent apply all matching mappings per profile, or only the first (ADP returns on first match, `mod.rs:332`)? `(needs human input)`
- Does the Agent restrict wildcard chars to the same `[a-zA-Z0-9\-_*.]` class?
- The mapper has no `watch_for_updates` — it appears static-only, so its runtime-config facet is likely unreachable (unlike the filters).

### mapper-interner-bounded — The mapper's second interner is bounded / fails visibly
| | |
|---|---|
| **Type** | Safety (heap-off: silent non-remap hazard; heap-on default: bounded claim fails by design) |
| **Property** | The mapper's own 64 KiB string interner stays bounded; when full, the metric is not silently forwarded under its original (unmapped) identity without accounting. |
| **Invariant** | Heap-off: `AlwaysOrUnreachable(mapper interner full ⇒ metric forwarded under ORIGINAL context, accounted)` + `Sometimes(mapper resolve == None)`. Heap-on (default): `Sometimes(mapper intern heap fallback > 0)`. SUT-side needed (no counter). The `?` at `mod.rs:317-321`/`:277-282` makes resolve-`None` → keep original context with no drop/else. |
| **Antithesis Angle** | Small `dogstatsd_mapper_string_interner_size` + a flood of distinct mappable names fills the mapper interner independently of the source interner; resolver-churn scheduling stress (30s idle expiry). |
| **Why It Matters** | A SECOND bounded interner (distinct from `interner-full-bounded`) whose default heap-on voids its declared firm bound, and whose heap-off path silently emits the wrong (pre-map) identity — a load-dependent identity flip. |
| **Priority** | High |

**Open Questions:**
- The mapper never calls `with_heap_allocations(false)` (defaults true) — intentional, or an oversight making its firm bound unenforceable? `(needs human input)`
- Can the same name be remapped on one call but silently not on the next purely due to interner pressure?

### filter-config-reload-correct — Live filter config reload applies correctly, never stale or silently cleared
| | |
|---|---|
| **Type** | Safety (data-correctness under live config reload) |
| **Property** | When the Core Agent pushes filter config over the RC stream while metrics flow, ADP applies the new filters to live data — never keeping stale filters nor silently clearing all filtering. |
| **Invariant** | `Always(after a settled filter update, the next metric is filtered per the NEW config)` (SUT-side at apply); `Unreachable("filter update Lagged-dropped with no reconciliation")` on `watcher.rs:61` (no re-read exists); `AlwaysOrUnreachable(tag filtering not silently fully-cleared by an unintended event)`; `Sometimes(reload while metrics in flight)`. Three confirmed hazards: (1) `broadcast::Lagged` warn+continue on a cap-100 broadcast → permanent staleness; (2) partial-deserialize skip; (3) `diff_recursive` is additive so key **deletion fires no event** (stale), while explicit-empty clears `tag_filterlist` but is ignored by the prefix/post-agg filters. |
| **Antithesis Angle** | Burst config updates faster than the filter task drains its receiver (node-throttle `adp` to widen the lag window) interleaved with sustained metric load; explore deletion vs explicit-empty vs malformed-entry. **The design partner's explicit stress-test focus.** |
| **Why It Matters** | Stale/half-applied/cleared filtering on live customer data is silent and customer-visible — the single most product-relevant transform property. |
| **Priority** | High |

**Open Questions:**
- Is the `Lagged` drop accepted as best-effort (a dropped final update is permanent staleness, no re-read)? `(needs human input)`
- Should removing a filter key from RC clear the filter? ADP's additive diff keeps it stale — confirm vs Agent RC semantics. `(needs human input)`
- The `tag_filterlist` (clears on `None`) vs prefix/post-agg (ignores `None`) asymmetry — intended?
- Requires the config-stream add-on; does **not** fire in standalone mode.

### tag-filterlist-applied-consistently — tag_filterlist applies cached and type-gated filtering consistently
| | |
|---|---|
| **Type** | Safety |
| **Property** | tag_filterlist's context-cache results always agree with a fresh computation, only Counter+sketch metrics are filtered, and output matches the Agent's time-sampler tag filtering. |
| **Invariant** | `Always(cache-hit filtered tags == fresh filter_metric_tags)` (SUT-side, catches stale/colliding cache entries, `mod.rs:240-263`); `AlwaysOrUnreachable(only Counter+sketch metrics have tags removed)` (`mod.rs:235-237`); optional differential `Always(post-filter (name,tags) within ratio of Agent)`; `Sometimes(tag removed)` + `Sometimes(cache hit served filtered result)`. |
| **Antithesis Angle** | Mixed-type load with overlapping names stressing the 100k/30s-TTI context cache, interleaved with reloads (compose with `filter-config-reload-correct`) + node-throttling to widen the reload-vs-apply window so a cache entry outlives the reload that should invalidate it. |
| **Why It Matters** | Counter+sketch-only gate and a hot-path cache, both claiming Agent equivalence with only self-tests; a stale entry or wrong type-gate silently leaks/drops tags (cardinality/PII). |
| **Priority** | Medium (High if the type gate diverges from the Agent) |

**Open Questions:**
- Does the Agent filter the same Counter+sketch subset? `(needs human input)`
- Cache keyed by full `Context` incl. origin tags — can origin-tag-only differences collide?
- If a reload is Lagged-dropped, the cache is not rebuilt and stale filtered contexts persist to TTI — confirm this compound failure.

### prefix-filter-ordering-matches-agent — Prefix/blocklist and post-aggregate filtering match the Agent's stage split
| | |
|---|---|
| **Type** | Safety (ordering + differential) |
| **Property** | ADP's listener-side prefix/blocklist filter and post-aggregate histogram-series filter run in the correct pipeline order and split responsibility over the shared keys exactly as the Datadog Agent does. |
| **Invariant** | Differential `Always(end-to-end keep/drop + final name within ratio of Agent)`; `AlwaysOrUnreachable(non-histogram-shaped entry not applied post-aggregate)` and converse; `AlwaysOrUnreachable(post_agg never drops a sketch)`; optional blueprint-shape `Always(dsd_prefix_filter between dsd_enrich and dsd_tag_filterlist; dsd_post_agg_filter after dsd_agg)` (`run.rs:674-679`); `Sometimes(prefix added/listener drop/post-agg drop)`. |
| **Antithesis Angle** | Corpus where a name's keep/drop depends on stage order + fault-induced flush-timing skew on the diff run; composes with `mapper-output-matches-agent` and `filter-config-reload-correct`. |
| **Why It Matters** | A past fix "moved DSD prefix/filter in front of enrich" (bug-history-sensitive); the listener-vs-time-sampler split over four shared keys is subtle, fragile, and has no end-to-end regression guard. |
| **Priority** | Medium (High as the ordering-regression tripwire) |

**Open Questions:**
- Does the Agent split on exactly the `<metric>.<aggregate-suffix>` shape `contains_filter_entry` uses? `(needs human input)`
- Is the prefix-filter-after-mapper ordering load-bearing for equivalence, with any guard besides this property?
- A reload updating one filter but lagging the other could filter at one stage but not the other for the same rule — confirm reachability.

## Category H — Liveness & Availability

Properties that demonstrate ADP **boots and stays alive**. They exist because the
generated per-replay `datadog.yaml` configs and adversarial load *should* sometimes crash ADP
(`interner-full-bounded`, `rss-bounded-under-cardinality`, `config-incompatible-refuses-start`), yet
the bootstrap `assert_reachable!` only fires on success and cannot report a branch where ADP died.
The death-liveness catch watches from *outside* the SUT and is **fault-gated**: it evaluates in a
quiet period (faults paused — the `eventually_`/`finally_` prefixes do this, or
`ANTITHESIS_STOP_FAULTS`), so a node-fault-induced outage recovers and passes while a self-inflicted
crash (panic on startup, crash from load) persists and fails. That gating is exactly the requirement:
*trigger on self-inflicted death, not on injected node faults.* A complementary **in-SUT
good-function** `Sometimes` (the forwarder shipped a payload) shows a booted ADP actually works.

> **Detection provenance (clarification, 2026-05-31).** Before this category landed, the *only* place
> a config-driven boot panic was ever observed was local **`snouty validate`** — a single-config
> smoke run done at launch time, outside any Antithesis timeline (e.g. an oversized
> `dogstatsd_string_interner_size` → `capacity would overflow isize::MAX`). No **in-run** mechanism
> caught it: a panicking ADP cannot self-assert, and the bootstrap `assert_reachable!` is silent on
> the dead branch. `eventually_adp_alive` (below) is what makes such boot/load crashes
> **in-run-detectable** — `snouty validate` catching one static config is *not* the same as an
> Antithesis shot finding it across the drawn-config space.

### adp-stays-alive — ADP boots and stays serving (self-inflicted-crash liveness)
> **Status (2026-05-31): LANDED** as the `eventually_adp_alive` test command
> (`test/antithesis/harness/src/bin/eventually_adp_alive.rs`). Valid in *this* harness because the adp
> image is a bare binary + boot wrapper (no s6 supervisor) — unlike the production image, where API
> liveness is vacuously green (note R1; use restart-count there).
| | |
|---|---|
| **Type** | Liveness (ADP eventually serves), evaluated in a faults-paused window |
| **Property** | After the per-replay config is applied and the workload runs, ADP's unprivileged API (`:5100`) is reachable **and** the DogStatsD listener socket exists; if neither comes up in a quiet period, ADP died of its own config/load, not an injected fault. |
| **Invariant** | The `eventually_` command (faults already paused) polls `:5100` and the DSD socket for ~60×1s, then `assert_always!(api_reachable && socket_present, …)`. Fault-induced down recovers in the quiet period → passes; a deterministic config/load crash crash-loops or stays dead → never binds → fails. |
| **Antithesis Angle** | The crash is config-driven (drawn `dogstatsd_*` boundary values); a deterministic boot panic stays down across the whole quiet period regardless of restart policy, so the quiet period cleanly separates a real bug from a transient node fault. |

### adp-keeps-delivering — ADP still processes and delivers after load (functional liveness)
> **Status (2026-05-31): PARTIAL.** The in-SUT good-function half **landed**: an `assert_sometimes!`
> at the forwarder's 2xx site (`lib/saluki-components/src/common/datadog/io.rs`, behind the new
> `saluki-components/antithesis` feature) fires when ADP ships a payload — proving a booted ADP runs
> the whole pipeline, and giving Antithesis a replay checkpoint anchored on a healthy state. The
> stronger **per-branch wedge detector** (an `assert_always!(delivered_recently && reachable)` in a
> faults-paused `finally_`) is **still net-new** — the landed `Sometimes` does not fail a branch where
> ADP accepted load then wedged.
| | |
|---|---|
| **Type** | Liveness (accepted load is eventually delivered), faults-paused window |
| **Property** | After the load drivers, in a quiet period the mock intake has received metrics *and* ADP still serves `:5100` — ADP is not merely up but not wedged. |
| **Invariant** | _Landed:_ in-SUT `Sometimes(forwarder shipped a payload)`. _Pending:_ `finally_` command that, in the faults-paused window, polls the mock intake's dump endpoint and `:5100` and asserts `Always(delivered_recently && reachable)` — catches "alive but stuck" that a bare reachability check (and a run-wide `Sometimes`) both miss. |

**Open Questions (Category H)**
- Does Antithesis's built-in container-exit detection already see ADP boot-panics here? Runs show it
  not firing — confirm via log search before assuming Category H is the only catch. `(needs human input)`
- Terminal `finally_`/`eventually_` only checks end-of-branch; a mid-run crash the workload papers
  over needs a `ANTITHESIS_STOP_FAULTS` liveness loop (deferred).

## Catalog-wide notes

- **Default config is hostile to the bounded-memory family:** memory limiter disabled
  (`MemoryMode::Disabled`), interner heap-spill enabled (`dogstatsd_allow_context_heap_allocs=true`),
  disk retry persistence off. Workloads must opt into protective settings to test the "holds"
  branches and leave defaults to capture the "fails by design" branches. This is the single most
  important workload-configuration decision (see `deployment-topology.md`).
- **One guaranteed-crash finding** needs no fault injection, only clock exploration:
  `aggregate-clock-skew-stable` (forward jump → flood) — cheap, high-value first target. Its former
  sibling `aggregate-no-panic-any-window` (sub-second window → divide-by-zero) was **fixed upstream**
  (window is now `NonZeroU64`, PR #1772) and is retained only as a regression tripwire.
- **Confirmed-live latent bug:** `ddsketch-no-nan-poison` is reachable today via a `checks_ipc`
  Histogram metric carrying NaN, which bypasses the per-source finiteness filters and poisons the
  encoder sketch's sum/avg permanently. The fix belongs at the sketch boundary.
- **Resource exhaustion via untrusted input:** `replay-no-panic-on-malformed-capture` carries two
  confirmed vectors — unguarded whole-file `fs::read` and uncapped `zstd::decode_all` — both reachable
  in the separate replay CLI process.
- **Differential property:** `aggregate-matches-agent` is anchored on the existing
  `panoramic`/`stele` diff harness, not an in-process SDK assertion.
- **SUT-side instrumentation is required** (not optional) for: all crash/panic properties, NaN-at-
  sketch-boundary, bin-count, interner-corruption, source-misroute, and limiter-RSS-failure — these
  internal states are invisible to a workload-only checker. Existing telemetry counters
  (`events_discarded_total`, `framing_errors`, `*_decode_failed`, queue-drop counters) serve only as
  `Sometimes`-reachability anchors, not as the safety assertions themselves.
- **(R1) The container's s6 supervisor auto-restarts ADP on exit**, so "process is up" workload
  assertions are vacuously green even during a crash-loop. Every no-crash property
  (`malformed-dsd-no-crash`, `malformed-event-sc-no-crash`, `replay-no-panic-on-malformed-capture`,
  the aggregate-crash pair) must assert SUT-side `Unreachable` at panic sites — or assert on
  restart-count — **never** container liveness.
- **(R2, updated 2026-05-31) The Antithesis Rust SDK is wired into ADP** behind the `antithesis`
  cargo feature (`antithesis_init()` + a bootstrap `assert_reachable!`), and as of 2026-05-31 the
  **first in-SUT property assertion** is landed: an `assert_sometimes!` at the forwarder 2xx site in
  `saluki-components` (its own new `antithesis` feature, enabled transitively by
  `agent-data-plane/antithesis`). So in-process instrumentation is no longer just the bootstrap probe
  — the path from a catalog property to a real SUT-side assertion is proven end-to-end. ~16 properties
  still need their net-new in-process SUT-side **invariant** assertions; the remaining workload-only
  properties (retry-queue bounds, shutdown, config-gate, RSS) can run first.
- **(R3) No-loss properties must use TCP or UDS ingress, not UDP** — UDP's inherent packet loss
  confounds any "accepted == delivered" reconciliation (`no-silent-interconnect-drop`,
  `forwarder-eventual-delivery`, `disk-persisted-retry-survives-restart`, `shutdown-drains-no-loss`,
  `events-sc-no-silent-loss`). Reserve UDP for the no-crash properties.
- **(R4) Anti-vacuity:** safety properties gated by hard-to-reach `Sometimes` anchors (bin collapse,
  interner resurrection race, events/SC reachability) require the workload to force the anchor
  config/corpus, and the run synthesizer must report an **unreached `Sometimes` as inconclusive, not
  passing**.
- **(G2 topology dependency)** the runtime filter config-reload properties
  (`filter-config-reload-correct`, and the reload facets of the tag-filterlist/prefix-filter
  properties and `config-runtime-update-not-revalidated`) require the **config-stream add-on
  topology** (Core Agent or stub) — they pass vacuously in standalone mode because the config watcher
  never fires.
- **(R5, updated 2026-05-31) Liveness observability is valid in this harness, and the catch is now
  landed (cf. R1's production case).** The harness `adp` image is a bare binary + boot wrapper
  (`deploy/Dockerfile` adp stage: `ENTRYPOINT ["/entrypoint.sh"]` → `agent-data-plane run`) with **no
  s6 supervisor**, so external `:5100` / DSD-socket liveness is a real signal for Category H — a
  deterministic config/load crash leaves them permanently unbound. As of 2026-05-31 the
  `eventually_adp_alive` command realizes this, and the workload no longer gates on adp health
  (`docker-compose.yaml`: `service_started`, not `service_healthy`) so the check runs even when ADP is
  down. R1's "never use container liveness" applies to the production s6 image; if the harness ever
  adopts an auto-restart image, switch Category H to a restart-count assertion.
- **(R6) Observed fault availability contradicts the Scope note.** Despite faults being recorded as
  tenant-enabled below, runs launched with the `basic_test` webhook have injected **zero** fault
  events — the `node - kill/pause/stop/throttle` and `clock - skip` total-fault-event properties
  report `0/0`. So Category H's fault-gating is currently moot (no faults to mistake for a crash) but
  is the right design for when faults fire; the webhook's fault configuration needs confirming.

## Scope (confirmed with user, 2026-05-28)

- **In scope:** the DogStatsD pipeline end-to-end — metrics, events, service-checks — plus the
  `saluki-core` runtime invariants (memory bounds, backpressure, lifecycle, pooling, interning) and
  the runtime filter config-reload surface.
- **Deferred (documented exclusion, not an oversight):** the **traces/APM, logs, and OTLP** pipelines
  (`run.rs:506-591,700-758`). These are wired in ADP but are not the first-customer (the design partner / Agent
  7.80.0) surface. They carry their own untrusted-input risk (notably the SQL-parsing
  `trace_obfuscation/sql.rs` and a second OTLP protobuf source + forwarder) and are the natural next
  expansion if/when they enter scope.
- **Fault availability (confirmed enabled for the tenant):** **node termination**, **clock jitter**,
  and **custom `/proc` faults** are all enabled — so the crash-recovery
  (`disk-persisted-retry-survives-restart`, `data-component-failure-triggers-process-shutdown`),
  clock-skew (`aggregate-clock-skew-stable`), and limiter-RSS-failure
  (`memory-limiter-survives-rss-read-failure`) properties are realizable rather than vacuous.

## Open Questions (catalog-wide / analysis-level)

- "METRIC_CONTROL relay" naming from Confluence has no source identifier — config flows through the
  generic snapshot/partial stream. Confirm with the team.
- _Resolved:_ `interconnect_capacity` default = **128** event buffers (`topology/mod.rs:37`).
- _Resolved:_ no protective memory setting is on by default (`memory_mode` = `Disabled`,
  `allow_context_heap_allocs` = true) → "bounded memory" is opt-in/aspirational under default config.
  This pins Category A's "fails by design under defaults" framing.
