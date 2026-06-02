---
sut_path: /home/ssm-user/src/saluki
commit: 042f41db3bd97118c38981765fd49696fce9d318
updated: 2026-05-28
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: ADP Confluence space.
---

# Implementability Evaluation — ADP Antithesis Property Catalog

Lens: **can each property actually be CHECKED as planned?** Three sub-questions per property:
(1) observability — workload-visible vs internal-state, and is the SUT-side instrumentation point
real and reachable; (2) topology — does the planned deployment support the failure scenario;
(3) preconditions — can the workload reliably construct the needed state within an Antithesis
timeline. Bias: surface what blocks a green check.

Verified against code this session (load-bearing for the findings below):
- `run.rs:91,94,486` — standalone mode is a config flag; `check_and_warn_config` (gate) runs on the
  *resolved* config regardless of source (`run.rs:157`).
- `run.rs:446` — `checks_ipc` pipeline is gated on `dp_config.checks().enabled()`, **independent of
  standalone mode**. `checks_ipc/mod.rs:5-13,39-48,57-77` — the source is a **gRPC server**
  (`ChecksServer`, TCP :5105) consuming `SendCheckPayloadRequest`; it needs an **external gRPC client
  speaking `datadog_protos::checks`** to emit anything.
- `forwarders/datadog/mod.rs:60-66` + `endpoints.rs:157-180` — `with_endpoint_override(dd_url, api_key)`
  exists; the forwarder CAN be pointed at a mock intake (primary egress topology is implementable).
- `limiter.rs:54` — the limiter checker is a bare `std::thread::Builder::new()` thread (confirms the
  silent-death framing of `memory-limiter-survives-rss-read-failure`).
- `transforms/aggregate/mod.rs:92-93,194` — `window_duration` is read once via serde `as_typed()` at
  build; **no `ConfigChangeEvent` subscriber** in the aggregate transform (grep: none). So
  `aggregate_window_duration` is **startup-only**, not runtime-reloadable.

Categories below: **F#** = a finding (something that blocks/weakens the planned check). The summary
at the end groups Findings / Passes / Uncertainties.

---

## Cross-cutting prerequisite (affects ~all SUT-side properties)

**F0 — The Antithesis Rust SDK is a hard prerequisite for ~17 of 27 properties, and it is not yet a
dependency.** `existing-assertions.md` confirms zero SDK usage and no `antithesis-sdk` in any
`Cargo.toml`/`Cargo.lock`. Every property whose check is an in-process `assert_*` (all crash/panic,
NaN-at-sketch, bin-count, interner-corruption, source-misroute, limiter-RSS-failure, context-limit,
retry-byte-cap, clock-skew, both replay properties, the lifecycle ordering/shutdown reachability
markers) is **blocked until a dedicated ADP image is built with the SDK linked in** and the
assertions are physically added at the cited code sites. The catalog acknowledges this in prose but
does not treat it as a gating work item with a per-site edit list. **Scope:** topology + build.
**Suggested action:** make "fork ADP, add `antithesis-sdk` dep, land the named assertions, build a
second instrumented image" an explicit milestone before any SUT-side property can pass; the
workload-only properties (below) can run first against a stock image. _Update 2026-05-30 — the SDK
dep and instrumented build are now in place (ADP `antithesis` cargo feature + bootstrap probe, see
`existing-assertions.md`); the remaining milestone is landing the per-site property assertions._

The properties that are **workload-only checkable** (no SUT fork strictly required, read telemetry /
mock-intake / process exit): `aggregate-context-limit-enforced` (counter-anchored), parts of
`no-silent-interconnect-drop` (counter), `forwarder-eventual-delivery` (intake reconciliation),
`disk-persisted-retry-survives-restart` (intake reconciliation), `retry-queue-bounded-under-outage`
(only the *reachability* Sometimes; the byte-cap Always is internal), `config-incompatible-refuses-start`
(exit code + log), `config-stall-no-deadlock` (CPU/log-rate + progress log), `aggregate-matches-agent`
(harness diff). Everything else needs the fork.

---

## Category A — Memory & Resource Bounds

### rss-bounded-under-cardinality — PASS (with a topology caveat)
- **Observability:** OK. RSS read workload-side (or SUT-side from the same `Querier`). Expected-to-fail
  is the finding, not a blocker.
- **F1 — the RSS bound is unobservable unless a limit is actually configured, and the cgroup auto-path
  is a silent trap.** The whole property presupposes `effective_limit_bytes()` exists. Under defaults
  the limiter is a noop (`accounting.rs:37-40,174-178`) and there is no grant to compare against — so
  the assertion has no threshold. The workload MUST set `memory_mode`+`memory_limit`. Worse
  (`rss-bounded-under-cardinality.md:118-139`): a **non-empty `DOCKER_DD_AGENT` env var** silently
  switches on cgroup auto-detect and changes the baseline. **Action:** pin `memory_mode`/`memory_limit`
  explicitly in the `adp` container env and assert (or log-scrape) that a non-noop limiter is active,
  else the run is vacuous; audit the base image for `DOCKER_DD_AGENT`.
- **Precondition / timing:** the "many distinct timestamped values" inflation path needs sustained
  high-cardinality load for long enough that interner heap-spill + `SmallVec` growth diverge from the
  grant. Feasible with `millstone`, but the **container cgroup may OOM-kill ADP before the assertion
  reads over-grant RSS** (the property's own open question). On a kill, the observable is a process
  crash, not an RSS reading — still a guarantee violation, but it lands on a *different* property
  (`data-component-failure...`/no-crash) and can confuse triage. **Action:** give the container
  headroom above ADP's configured grant so the assertion fires before the kernel kill.

### aggregate-context-limit-enforced — PASS
- The `Always(len <= context_limit)` is a single-task, lock-free, local invariant — the cleanest
  SUT-side `Always` in the catalog. `Sometimes(breached)` is anchorable to the existing
  `events_dropped`/breach counter (workload-readable). **Precondition:** lower `aggregate_context_limit`
  (e.g. 1000) so the boundary is reachable in-run — straightforward with a cardinality flood. No
  topology or timing blocker.

### interner-full-bounded — PASS (mode B), PASS-with-precondition (mode A)
- **Observability:** distinguishing interned/inlined/heap-fallback/dropped needs SUT-side state, as the
  file says; `intern_fallback_total` exists as a `Sometimes` anchor (mode B, default) and is
  workload-readable. Mode B is easy.
- **F2 — mode A (bounded) preconditions are fiddly and fragility-prone.** To fill a fixed interner you
  must (a) set a *small* `dogstatsd_string_interner_size_bytes`, (b) set
  `dogstatsd_allow_context_heap_allocs=false` (opt-in, test-only in shipping code per
  `interner-full-bounded.md:117-124`), and (c) use **names/tags > 31 bytes** so `MetaString` inlining
  doesn't bypass the interner entirely (`:89-91`). Miss any one and the property is vacuous (never
  fills, or spills to heap). Plus fragmentation under churn can make `try_intern` return `None` below
  nominal capacity (open question), which muddies the "bounded == drops at budget" reading.
  **Action:** bake (a)/(b)/(c) into the mode-A workload corpus and add a `Sometimes(try_intern==None)`
  guard so a non-filling run is flagged rather than passing green.

### memory-limiter-survives-rss-read-failure — CONDITIONAL (needs a custom fault + SUT fork)
- **Topology:** requires a **custom `/proc` RSS-read fault** (deployment-topology.md:144 flags it as
  "Custom; must script"). Antithesis cannot inject this out of the box; someone must write a fault
  hook that makes `process_memory::Querier::resident_set_size()` return `None` mid-run on the target.
- **F3 — the failure may be unreachable on the Antithesis Linux target without that custom fault, and
  the property's whole value hinges on it.** The file's own pivotal open question
  (`:93-98`): can `resident_set_size()` actually fail post-startup on this platform, or only via
  injected corruption? If `/proc/self/statm` essentially never fails on the target, the realistic
  panic is reachable *only* through the scripted fault — making this a fault-injection curiosity, not
  a production risk. Also requires the limiter to be ON (`memory_mode!=disabled`+limit), i.e. the same
  config prerequisite as F1.
- **Observability:** the `.expect()` thread death is **silent** (bare `std::thread`, no shutdown, no
  metric) — confirmed `limiter.rs:54`. So this is **not workload-observable** at all without the SUT
  fork: you must replace the `.expect()` site with an `assert_unreachable` (or panic-hook). **Action:**
  confirm tenant supports the custom `/proc` fault AND that the SUT fork lands the assertion; if the
  fault is unavailable, demote/park this property — it cannot be checked.

### retry-queue-bounded-under-outage — PASS (split observability)
- **Topology:** needs `adp↔mock-intake` across a container boundary so the outage is faultable —
  primary topology provides it. Mock intake needs a controllable reject/5xx/hang mode (topology open
  question; `datadog-intake` may need a small extension). **Action:** confirm/extend the mock intake's
  failure-mode toggle (also needed by `forwarder-eventual-delivery`, `shutdown-drains-no-loss`).
- **Observability:** the byte-cap `Always` is internal to `RetryQueue::push` → SUT fork. The
  `Sometimes(items_dropped>0)` is telemetry → workload-readable. Disk-cap branch requires disk
  persistence ON and the silent-fallback (`io.rs:405`) to NOT have fired — the file already flags this
  (must `assert_unreachable`/log-scrape the fallback or the disk-cap test is vacuous). All tractable.
- **Precondition:** saturate the queue within a timeline — feasible with sustained load + a held-down
  intake; size load vs. the 15 MiB default cap. Multi-endpoint fan-out multiplies the bound (decide
  per-endpoint vs aggregate assertion — affects what threshold you check).

---

## Category B — Data Integrity & No Silent Loss

### no-silent-interconnect-drop — PASS (scope carefully)
- `events_discarded_total` is emitted and workload-readable; `Always(delta==0)` on wired edges is
  checkable without a fork (counter scrape), and the discard branch only fires for zero-sender
  outputs. **Caveat (file's open question):** confirm no production DSD output is ever
  conditionally-unwired (`dsd_debug_log_out`/`dsd_stats_out`); if one is, scope the `Always` to the
  always-wired edges or it false-positives. **Precondition:** must actually reach the full-channel
  state — 128-buffer edges (`built.rs:653`) mean you need a genuinely slow downstream + sustained load;
  the `Sometimes(backpressure engaged)` anchor needs a stall signal (rising send latency). Tractable
  via throttling the intake.
- **Transport note:** this is an internal-edge property; UDP lossiness upstream does NOT confound it
  (the assertion is on the encoder→forwarder internal edges, not the wire). Fine on UDP or TCP.

### forwarder-eventual-delivery — PASS (TCP/UDS for the input side)
- **Observability:** primary check is workload-side reconciliation of accepted-retryable vs
  mock-intake-received → no fork needed for the core; the `Reachable(Error::Open re-enqueue)` anchor
  needs the fork.
- **F4 — UDP input confounds the "all accepted ... delivered" reconciliation; this property MUST use
  TCP or UDS for the DSD ingress.** The liveness claim equates *accepted* input to *delivered* output.
  If `millstone→adp` is UDP (the topology's default suggestion), packets can be dropped on the wire
  *before* acceptance, so "accepted" is unmeasurable from the sender side and the reconciliation set is
  ill-defined. deployment-topology.md:175 hints at this ("for no-loss assertions prefer TCP/UDS"), but
  the property files don't state it as a hard requirement. **Action:** pin DSD ingress to **TCP** for
  this property (UDS needs a shared volume → not faultable, and the egress link is what we fault here,
  so TCP ingress is fine). Same applies to `disk-persisted-retry-survives-restart` and
  `shutdown-drains-no-loss`.
- **Precondition:** outage must be **shorter than retry-queue overflow** (else drop-oldest legitimately
  sheds data and the reconciliation must exclude it). Needs a quiet/heal window for the eventual check
  (`eventually_`/`ANTITHESIS_STOP_FAULTS`). Both standard.

### disk-persisted-retry-survives-restart — CONDITIONAL (node-termination fault) — PASS otherwise
- **Topology:** needs the **node-termination/kill fault** (topology table: "Commonly disabled — must
  enable") + s6 restart. If the tenant has kill disabled, this property can't run.
- **F5 — the silent in-memory fallback makes this vacuous unless explicitly guarded, and the
  persistence-active signal is log-only (no metric).** `io.rs:405-408` falls back to in-memory with
  only an `error!` log. The property file already prescribes treating the fallback as
  `assert_unreachable` (fork) or log-scraping. Without that, a misconfigured disk path silently turns
  this into an in-memory test that "passes" while proving nothing. **Action:** enforce the
  persistence-active check as setup-gating.
- **Observability/precondition:** reconciliation at the mock intake with transaction-identity dedup
  (workload-side, OK); tolerate the ~1-txn at-most-once window (delete-before-return). Corrupt-entry
  poison drop is checkable either by injecting a hand-crafted file or naturally via SIGKILL-mid-write
  (non-atomic write, confirmed). Use TCP/UDS ingress (see F4).

### source-dispatch-no-misroute — PASS (fork required; misroute structurally improbable)
- **Observability:** the routing decision is internal — NOT telemetry-visible — so the
  `Unreachable(misroute)` must be an in-process `assert_unreachable` checking the metrics dispatch
  buffer is metrics-only (`mod.rs:~1707`). Fork required.
- **F6 — the failure may be structurally unreachable, risking a vacuous/never-firing assertion.** The
  file's own analysis (`:33-52`) shows `extract`-then-`send_all` removes matched events from the buffer
  *before* the send can fail, so a send error causes *loss*, not misroute — the assertion likely never
  fires. That's fine as a **regression tripwire**, but it means this property cannot demonstrate value
  in a run (no `Sometimes` can prove the bad state is reachable, because it isn't). **Action:** keep it
  as a guard, but set expectations that it is a latent-regression assertion, not a falsifiable-now
  property; pair it with the *loss-counting* sub-claim (B) which IS observable. Also: the two
  `.expect("...output should always exist")` are real crash sites if a deployment omits an output —
  worth a separate guard but only reachable by mis-wiring (not normal load).

### shutdown-drains-no-loss — PASS (conditional, intricate preconditions)
- **Topology:** SIGINT/termination on `adp`; slow/blocked intake to push past 30s (needs the mock-intake
  hang mode, F4-adjacent). OK in primary topology.
- **F7 — the "accepted-before-signal that reached a flushed window" set is hard to construct precisely,
  making the no-loss reconciliation fragile.** Two designed-loss boundaries (open aggregation window
  dropped unless `flush_open_windows`; 30s forceful stop) mean the reconciliation set must *exclude*
  open-window and post-timeout data. Determining exactly which input metrics "reached a flushed window"
  at the instant of the signal requires knowing the aggregate flush cadence vs. the signal time — a
  timing-coupled boundary the workload can only approximate. **Action:** set `flush_open_windows=true`
  to remove one boundary, drive only *closed-window* (pre-signal, aged > window) data into the no-loss
  set, and assert the clean case as `Sometimes` (not `Always`). Use TCP/UDS ingress (F4). Realistic
  drain time near 30s under max load is itself a finding (size load conservatively).

---

## Category C — Aggregation & Sketch Correctness

### aggregate-matches-agent — CONDITIONAL (separate heavy topology) — implementable but expensive
- **Topology:** Add-on 2 (datadog-agent baseline + adp + two intakes + panoramic/stele). Doubles
  containers and state space; must run as its own template.
- **F8 — fault injection and the differential check are in fundamental tension; the diff is only valid
  in a fault-free quiet window, which limits what this property actually tests.** Injected scheduler
  pauses/clock steps create *timing-artifact* diffs (delayed flush → different bucket) that are false
  positives, not correctness bugs. The topology doc concedes the comparison must run inside an
  `ANTITHESIS_STOP_FAULTS` window long enough to cover `FLUSH_WAIT≈32s` on both sides. So the property
  largely re-runs the existing deterministic diff test under Antithesis with faults *paused* — the
  net-new coverage (equivalence *under* faults) is the hardest part and is exactly where false diffs
  bite. **Open implementability questions unresolved:** can `panoramic` survive an ADP restart mid-run
  (it may assume a single long-lived process)? Is the Agent baseline's bucket width pinned identical to
  ADP's window (else the stele `interval_a==interval_b` check, metrics.rs:171, false-positives)?
  **Action:** treat as a low-fault, quiet-window equivalence run; verify `panoramic` restart-tolerance
  before committing; this is the highest-effort, lowest-certainty property to operationalize.

### aggregate-no-panic-any-window — PASS (cheapest high-value target)
- Deterministic crash from config alone; no fault injection needed, just config-space exploration of
  the `{"secs":0,"nanos":N}` shape. The `Unreachable` at `align_to_bucket_start`/`step_by` needs the
  fork to be a *clean* signal, but even **without** the fork the panic → process exit → s6 crash-loop
  is workload-observable (no listener / repeated restart). **Resolved here:** the runtime-config-push
  open question is **NO** — `window_duration` is read once at build (`mod.rs:92-93,194`), no
  `ConfigChangeEvent` subscriber, so this is a **startup-only** crash vector. Update the property to
  drop the "gRPC live-push" angle.

### aggregate-clock-skew-stable — CONDITIONAL (clock-jitter fault) — PASS otherwise
- **Topology:** needs **clock-jitter fault** ("Commonly disabled — must enable"). If unavailable, the
  property can't run.
- **Observability:** `zero_value_buckets.len()` and the `last_flush`/`current_time` pair are internal →
  fork required for the crisp `Always(bounded)`/`Always(monotonic)`. Downstream flood/gap is only
  *indirectly* visible workload-side (a spike in zero-value points at the mock intake) — a weaker proxy.
- **Precondition:** step the container clock forward (flood) / backward (gap) *during* a flush — the
  `Sometimes(clock jumped during flush)` anchor confirms coincidence. Forward-jump flood is easy to
  observe (memory + point count). All tractable given the fault. **Action:** confirm clock fault
  enabled; land the SUT-side bucket-count assertion.

### ddsketch-bin-count-bounded — PASS (fork; needs to drive >4096 bins)
- `bin_count()` is internal → fork. **Precondition:** must push a sketch past 4096 bins so `trim_left`
  fires and the `Reachable("trim_left collapsed")` is non-vacuous — needs thousands of distinct
  histogram/distribution sample values per flush. Feasible via `millstone` distribution corpus.
  Otherwise the `Always` is vacuously true. No topology blocker (rides normal DSD load).

### ddsketch-relative-error-bound — PASS as a HARNESS/library test only (NOT a live runtime check)
- **F9 — this is not checkable as a live ADP runtime invariant; it can only be a SUT-side unit/harness
  assertion.** The file resolves (`:104-128`) that ADP **never calls `DDSketch::quantile` on the
  customer path** — it ships raw bins; quantiles are computed server-side. So there is no production
  call site to anchor `Always(quantile within eps)`. The only realizable form is an in-tree test-harness
  assertion over the agent sketch in isolation (essentially the existing proptests with SDK
  annotations). **Action:** reframe explicitly as a library-invariant harness check (the catalog
  already demotes it to Medium and says this); do not plan a topology/workload path for it. Merge-order
  facet (f64 `avg`/`sum` non-associativity) is likewise only meaningfully testable harness-side.

### ddsketch-no-nan-poison — CONDITIONAL — the planned producer needs a NOT-YET-BUILT gRPC feeder
- **F10 — the only live NaN path requires a checks_ipc gRPC producer that the primary (DSD-only)
  topology lacks and that nobody has built; without it the property is unreachable.** Confirmed this
  session: `checks_ipc` is a **gRPC server** (`ChecksServer` on TCP :5105, `checks_ipc/mod.rs:5-13,
  39-77`) consuming `SendCheckPayloadRequest`; the NaN bypass (`checks_ipc/mod.rs:195` → encoder
  `insert_n`) only fires if some client sends a Histogram with a NaN value. The DSD path is
  finiteness-gated (FloatIter), OTLP is gated, the aggregate path is DSD-only — so **no DSD workload can
  reach the poisoning site.** deployment-topology.md:177-179 hand-waves "add a minimal checks-IPC
  feeder or a unit-level SUT assertion." That feeder is a real build task: a gRPC client speaking
  `datadog_protos::checks` that emits a NaN histogram. The source is enabled by `dp_config.checks().
  enabled()` and works in standalone mode (no Core Agent needed for checks_ipc itself), but the
  *producer* is missing. **Action:** EITHER build the checks_ipc NaN feeder (client + enable the
  pipeline) for an end-to-end check, OR fall back to a SUT-side unit assertion at the sketch boundary
  (`adjust_basic_stats`) exercised by an in-process test — the catalog should pick one explicitly, not
  leave it as "or." The sketch-boundary `Always(v.is_finite())` assertion itself is sound and is the
  right fix location.

---

## Category D — Lifecycle & Configuration

### topology-ready-before-intake — PASS (reframed to milestone ordering)
- The file already honestly narrows this to **readiness-milestone ordering** (sup-ready before
  build/spawn; eventually all_ready), NOT "no bytes read before ready" — because the source binds and
  reads gated only by backpressure. The defensible assertion is log/flag ordering (`sup_ready_ms`
  before `topology_ready_ms`), which is workload-observable from logs even without the fork.
  **Uncertainty (open question):** does `dsd_in` bind listeners during `initialize()` before
  `mark_ready`? If so a `port_listening` probe pre-ready would show binding-before-ready — strengthens
  or weakens the claim. Needs a read of the DSD listener-bind vs mark_ready ordering to finalize
  assertion strength. Not a blocker; just bounds the claim.

### config-stall-no-deadlock — PASS — but the busy-loop falsification target is dead
- **Topology:** needs Add-on 1 (core-agent-stub or minimal gRPC config-stream stub) — a **build task**
  (topology open question). Standalone mode bypasses the config stream entirely, so this property is
  N/A without the stub.
- **F11 — the headline "busy-loop" falsification target is unreachable through the tonic stack; the
  real (and only) check is a quiescent-hang detector.** The file's investigation (`:120-187`) resolves
  that a steadily-erroring stream terminates after one error and backs off 5s — no spin. So the
  realizable check is: drop the snapshot → ADP blocks **quiescently** (low CPU) at `ready().await`
  forever, no panic. That is checkable workload-side (CPU/log-rate monitor + "Waiting for initial
  configuration" present, "Initial configuration received" absent). **Action:** drop the busy-loop
  scenario from the workload; keep the quiescent-hang + flap-reconnect(5s) scenarios. The stub must be
  able to register ADP then withhold the snapshot — confirm the stub supports that.

### config-incompatible-refuses-start — PASS (workload-observable)
- Exit code 1 + absence of `topology_ready_ms` + no data at intake — fully workload-observable; the
  `Unreachable`-after-spawn and `Reachable`-at-refusal markers are nice-to-have fork additions but not
  required for the core check. **Precondition:** the workload must supply a **current `Severity::High`
  non-default key** from `config_registry/datadog/unsupported.rs`; that list drifts, so pin it to the
  commit or source it dynamically. Needs Add-on 1 stub to deliver config (or bootstrap YAML/env, which
  also works — `check_and_warn_config` runs on the resolved config regardless of source, confirmed
  `run.rs:157`). So this is actually runnable **without** the stub via env/YAML config — easier than
  the topology doc implies.

### config-runtime-update-not-revalidated — CONDITIONAL — reachability depends on an unanswered product
question
- **Topology:** Add-on 1 stub, in remote-agent mode (must push a runtime `Partial`/`Snapshot` carrying
  a high-severity key).
- **F12 — the property's reachability hinges on a `(needs human input)` product question the catalog
  hasn't resolved: can a `Severity::High` key actually traverse the config stream, or does the Core
  Agent pre-filter it?** If the real Agent never emits such a key, only an adversarial stub can, which
  makes this a "the gate doesn't exist at runtime" documentation finding rather than a falsifiable
  property. The check itself (Reachable on the unguarded apply path, or Unreachable on running-with-key)
  is implementable via the stub, but its *value* is gated on the product answer. **Action:** get the
  team's answer before investing; if startup-only gating is intentional, demote to a documented gap +
  a single `Reachable` marker.

### graceful-shutdown-within-30s — PASS (conditional on fault + scope to topology)
- SIGINT clean case (bounded load) + wedged-intake forceful case. Reachability markers on both branches
  of `shutdown_with_timeout` (fork) or log-observable (`"All components stopped."` /
  `"Forcefully stopping topology"`). **Caveat (file open question):** the **internal-supervisor
  shutdown has no timeout** (`run.rs:294`), so the *process* can exceed 30s even when the *topology*
  met it — a workload asserting "process exits within ~35s" can false-fail for an out-of-scope reason.
  **Action:** scope the assertion to topology-shutdown completion (the log/marker inside
  `shutdown_with_timeout`), not process exit. Forceful path needs the mock-intake hang mode (F4-adjacent).

### data-component-failure-triggers-process-shutdown — PASS (temporal/log check)
- Best realized as an Antithesis **query-logs temporal assertion**: whenever "Topology component
  unexpectedly finished" appears, a process exit follows — workload/triage-side, no fork strictly
  needed (a `Reachable` marker on the select arm helps). **Precondition:** induce a component finish —
  cheapest via the sub-second-window panic (`aggregate-no-panic-any-window`) which is a guaranteed
  deterministic finish. So this property piggybacks on C's crash target. Needs node-termination/kill
  NOT required (the component finishes on its own). Solid.

---

## Category E — Untrusted Input Parsing

### malformed-dsd-no-crash — PASS (UDP fine here; this is the no-crash property)
- **Transport:** explicitly the property where **UDP is appropriate** — it tests the connectionless
  clear-and-continue path; UDP/UDS-datagram listener survival is *part of the property*. The file
  correctly scopes "socket never dies" to connectionless and excludes TCP `break`. No loss assertion
  here, so UDP lossiness doesn't confound.
- **Observability:** `Always(process up)` is workload-side liveness; `framing_errors`/`*_decode_failed`
  are existing counters for the `Sometimes` anchors. The `Unreachable` at the `unreachable!` /
  `from_utf8_unchecked` codec sites needs the fork (a parser-regression panic is otherwise just a crash).
  **Precondition:** SDK-RNG-generated adversarial packets across all 4 listeners — straightforward.
  Covering UDS-datagram/stream needs the shared-volume sidecar (listener-coverage variant), an extra
  topology piece but documented.

### replay-no-panic-on-malformed-capture — PASS (instrument the REPLAY CLI process, not ADP)
- **F13 — the panic assertion must live in the separate replay CLI process, which means a SECOND
  instrumented binary, and the realistic panic surface is in zstd/prost deps, not ADP code.** Confirmed
  (`reader.rs` + `dogstatsd.rs:394`): replay parses the file **in the `agent-data-plane dogstatsd
  replay` CLI process** and forwards payloads over UDS to the running ADP. So (a) the SUT-side panic
  hook / `assert_unreachable` belongs in the replay CLI, requiring SDK linkage in that code path too;
  (b) the reader's own two `expect`s are bounds-guarded (not panic sites) — the real risk is a panic
  *inside* `zstd::stream::decode_all` / `prost::decode` on adversarial input, which is harder to assert
  on (it'd be a dep panic → SIGABRT). **Also two confirmed resource-exhaustion vectors** (unbounded
  `fs::read`, uncapped `zstd::decode_all`) are OOM, not panic — they're a *different* observable
  (process killed) and overlap the memory family. **Action:** instrument the replay CLI; treat OOM
  vectors as a separate resource property or a size-cap fix; use the listener-coverage variant
  (shared-volume `replay-client`) — no cross-container faults needed (pure input exploration). **The
  capture files must be SDK-RNG-generated adversarial bytes** — a workload generator build task.

### replay-corruption-not-silent-eof — CONDITIONAL — likely needs a format change OR stays heuristic
- **F14 — distinguishing truncation from clean EOF may be impossible without a format change, so a
  strict `Always` is not implementable today; only a heuristic/`Sometimes` is.** The file resolves
  (`:89-92`): there is **no record-count or total-length field**, and the code intentionally returns
  `Ok(None)` for truncation (tests assert it). To assert "completion was faithful" you'd need to know
  the true record count, which the format doesn't carry. So the realizable check is a SUT-side
  assertion at `reader.rs:95` distinguishing `size==0 && at_trailer_boundary` (clean) from
  overrun/mid-stream (corrupt) — which requires the reader to *track* the trailer boundary it doesn't
  today. Without that instrumentation change, the property degrades to `Sometimes(an overrunning prefix
  was seen)` — proves the corrupt branch is reachable but NOT that completion is faithful. Plus a
  `(needs human input)` question on whether maintainers even consider silent truncation a bug.
  **Action:** scope to the `Sometimes(corruption-reached-the-(b)-branch)` form + the CLI-process
  instrumentation, and flag the strict-fidelity `Always` as fix-dependent (format change needed).

---

## Category F — Concurrency & Boundary Conditions

### interner-reclamation-no-corruption — CONDITIONAL — relies on Antithesis scheduling to hit the race
- **F15 — the corruption branch is loom-proven-safe under the modeled interleavings; whether Antithesis
  can construct an interleaving loom doesn't cover is unknown, so the `Sometimes(contended path hit)`
  anchors may never fire, risking a vacuous green.** The whole value proposition is "explore beyond
  loom's bounded model under the real scheduler," but the workload cannot *force* the
  decrement→lock→re-check race; it can only create pressure (small interner, high-cardinality churn,
  short-lived contexts) and hope Antithesis schedules the contended interleaving. The `Sometimes(drop
  re-check found resurrected entry)` / `Sometimes(reclaimed-slot reused)` anchors are the guard against
  vacuity — but if they never fire, the property neither passes meaningfully nor fails. **Observability:**
  the corruption check (overlap or sentinel-run in a resolved `&str`) is internal → fork; note the
  **two different sentinels** (`0x21` in map.rs, `0xAA` in fixed_size.rs) — a hard-coded check would
  miss one impl; use direct overlap detection. **Precondition:** small interner + heap-fallback OFF so
  reclamation is actually pressured (else strings spill to heap and reclamation is never exercised).
  **Action:** land the overlap-based (not sentinel-hardcoded) SUT assertion + the `Sometimes` contention
  anchors; accept that reachability of the race is Antithesis-scheduler-dependent and report the anchor
  status in triage so a never-contended run isn't mistaken for a pass.

### non-finite-values-handled-consistently — PASS (DSD facet) / shares F10 (NaN-poison facet)
- The DSD ghost-metric facet is checkable on the primary topology: all-non-finite packets →
  `num_points==0` gate (`mod.rs:1478`) → `Ok(None)`; the `AlwaysOrUnreachable(no zero-point metric
  reaches aggregation)` and `Sometimes(non-finite dropped)` are anchorable (the latter needs a
  `non_finite_dropped` counter that doesn't exist — currently only a `debug!` log, so add a counter or
  the fork). The **NaN-at-sketch facet inherits F10** (needs the checks_ipc gRPC feeder to be live; on
  the DSD path it's correctly Unreachable). **Action:** add the `non_finite_dropped` reachability anchor
  (counter or SUT marker); keep the sketch-boundary `Always(is_finite)` as the producer-independent
  assertion but understand its live exercise depends on F10's feeder.

---

## Summary

### Findings (blockers / weakeners — Property | Concern | Scope | Evidence | Action)

- **F0 | SDK not present; ~17 properties need an instrumented ADP fork before they can pass | build/topology |
  existing-assertions.md (zero SDK); deployment-topology.md:153-161 | Make "fork + add SDK + land named
  assertions + second image" an explicit gating milestone; run workload-only properties first.**
- **F1 | rss-bounded-under-cardinality — no grant to assert against under defaults; `DOCKER_DD_AGENT` silently
  flips the baseline | config | rss-bounded-under-cardinality.md:118-139; accounting.rs:37-40,107-121 | Pin
  memory_mode+memory_limit, assert non-noop limiter, audit image for DOCKER_DD_AGENT; give container RSS
  headroom so assertion fires before OOM-kill.**
- **F2 | interner-full-bounded mode A — three coupled preconditions (small interner, heap-off, >31B strings) or
  vacuous | config/precondition | interner-full-bounded.md:89-91,117-124 | Bake all three into the corpus +
  Sometimes(try_intern==None) guard.**
- **F3 | memory-limiter-survives-rss-read-failure — needs a custom /proc fault that may be the ONLY way to reach
  the failure; silent thread death is unobservable without the fork | topology+fault+SUT | deployment-topology.md:144;
  limiter.rs:54,100-102; property open Q :93-98 | Confirm tenant supports the custom fault AND the fork; else
  park — uncheckable.**
- **F4 | forwarder-eventual-delivery / disk-persisted-retry / shutdown-drains — UDP ingress confounds no-loss
  reconciliation; MUST use TCP (UDS needs shared volume) | topology | deployment-topology.md:175; forwarder-
  eventual-delivery.md:69-74 | Pin DSD ingress to TCP for these three properties.**
- **F5 | disk-persisted-retry — silent in-memory fallback (log-only, no metric) makes it vacuous unless guarded |
  observability | disk-persisted-retry-survives-restart.md:121-130; io.rs:405-408 | Gate the run on a
  persistence-active assert_unreachable/log-scrape.**
- **F6 | source-dispatch-no-misroute — misroute is structurally unreachable (extract-then-send); assertion can't
  fire in a run | falsifiability | source-dispatch-no-misroute.md:33-52 | Keep as regression tripwire; pair with
  the observable loss-counting sub-claim.**
- **F7 | shutdown-drains-no-loss — the "accepted-before-signal, flushed-window" set is timing-coupled and hard to
  construct precisely | precondition | shutdown-drains-no-loss.md:55-60 | Set flush_open_windows=true, restrict to
  closed-window data, assert as Sometimes.**
- **F8 | aggregate-matches-agent — faults create false diffs; net-new coverage only valid in a fault-paused
  window; panoramic may not survive restart | topology/method | aggregate-matches-agent.md:89-96; deployment-
  topology.md:117-120 | Run as a low-fault quiet-window equivalence; verify panoramic restart-tolerance first.**
- **F9 | ddsketch-relative-error-bound — no live runtime call site (ADP ships raw bins, no quantile); only a
  library/harness test | observability | ddsketch-relative-error-bound.md:104-128 | Reframe as in-tree harness
  assertion; no topology/workload path.**
- **F10 | ddsketch-no-nan-poison (and NaN facet of non-finite) — only-live NaN path needs a checks_ipc gRPC NaN
  feeder not in the primary topology and not yet built | topology/build | checks_ipc/mod.rs:5-13,39-77,195;
  run.rs:446; deployment-topology.md:177-179 | Build the gRPC checks feeder OR fall back to a SUT-unit sketch-
  boundary assertion — pick one explicitly.**
- **F11 | config-stall-no-deadlock — busy-loop falsification target is unreachable through tonic; needs a config-
  stream stub | method/topology | config-stall-no-deadlock.md:120-187 | Drop busy-loop scenario; keep quiescent-
  hang; confirm stub can register-then-withhold-snapshot.**
- **F12 | config-runtime-update-not-revalidated — reachability gated on an unresolved product question (can a
  High-severity key traverse the stream?) | product input | config-runtime-update-not-revalidated.md:42-47 | Get
  team answer before investing; else demote to documented gap + Reachable marker.**
- **F13 | replay-no-panic — assertion must live in the SEPARATE replay CLI process (second instrumented binary);
  real panic surface is in zstd/prost deps; +2 OOM vectors | topology/build | replay-no-panic-on-malformed-
  capture.md:83-130; reader.rs:40-44; dogstatsd.rs:394 | Instrument the replay CLI; SDK-RNG capture generator;
  treat OOM vectors separately.**
- **F14 | replay-corruption-not-silent-eof — strict fidelity Always needs a format change (no record count exists);
  only a heuristic Sometimes is implementable today | format limitation | replay-corruption-not-silent-eof.md:89-92 |
  Scope to Sometimes(corrupt branch reached) + CLI instrumentation; flag strict Always as fix-dependent.**
- **F15 | interner-reclamation-no-corruption — race is loom-safe; hitting an un-modeled interleaving depends on the
  Antithesis scheduler; Sometimes anchors may never fire (vacuous) | scheduler-dependence | interner-reclamation-
  no-corruption.md:55-128 | Use overlap-based (not sentinel-hardcoded) assertion; heap-off + small interner; report
  anchor status so a never-contended run isn't read as a pass.**

### Passes (implementable as planned, modulo the SDK fork in F0 and any noted scoping)

- aggregate-context-limit-enforced (cleanest SUT Always; counter-anchored Sometimes).
- aggregate-no-panic-any-window (config-only crash; resolved: startup-only, drop the runtime-push angle).
- ddsketch-bin-count-bounded (fork + drive >4096 bins).
- no-silent-interconnect-drop (counter-readable; scope to always-wired edges).
- retry-queue-bounded-under-outage (split observability; mock-intake failure-mode toggle needed).
- config-incompatible-refuses-start (workload-observable via exit code; runnable even without the stub via env/YAML).
- topology-ready-before-intake (reframed to milestone ordering; log-observable).
- graceful-shutdown-within-30s (scope to topology shutdown, not process exit).
- data-component-failure-triggers-process-shutdown (log/temporal check; piggybacks on the C crash target).
- malformed-dsd-no-crash (UDP appropriate here; counters + liveness; fork for codec Unreachable).
- aggregate-clock-skew-stable (needs clock fault enabled; otherwise solid).
- non-finite-values-handled-consistently (DSD ghost-metric facet; needs a non_finite_dropped anchor).

### Uncertainties (need an answer to finalize the check)

- Does `dsd_in` bind listeners during `initialize()` before `mark_ready`? Decides whether
  topology-ready-before-intake can strengthen to "no socket bound pre-ready" (file open Q).
- Is any production DSD output (`dsd_debug_log_out`/`dsd_stats_out`) ever conditionally unwired? Decides
  the scope of no-silent-interconnect-drop's `Always(delta==0)` (file open Q).
- Can `process_memory::Querier::resident_set_size()` fail post-startup on the Antithesis Linux target
  without the custom fault? Pivotal for F3's priority (file open Q).
- Can a `Severity::High` config key actually traverse the Core Agent → ADP config stream? Pivotal for
  F12's value (file open Q, needs human input).
- Does the mock `datadog-intake` binary support a runtime-toggleable reject/5xx/slow/hang mode, or does it
  need extension? Needed by retry-queue, forwarder-eventual-delivery, shutdown forceful-path (topology open Q).
- Which faults are enabled on the target tenant (node-termination, clock-jitter both "commonly disabled",
  custom /proc fault)? Several CONDITIONAL properties can't run if these are off (topology fault table).
- Can `panoramic` survive an ADP process restart mid-run? Decides whether aggregate-matches-agent's
  restart-equivalence facet is testable at all (file open Q).
