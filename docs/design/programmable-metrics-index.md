# Programmable metric processing: index

Start here. This page says what the programmable metrics work is, where it stands, and which document holds what, so
that the whole can be understood at a high level before reading any of the longer documents. Branch
`luke/programmable-metrics`; pushed once up to `eb1c4ee134`. The owner then pushed three signed commits on top of it
(N1a, the first iterations, and the stall study: `862764b247`, `7c7631a4bb`, `21f1d9d12f`), which were merged with the
local commits since (version 2 through V7b, and N1b) on 2026-11-04.

## In one paragraph

ADP's metrics path runs from a single logical plan instead of hard-wired components. Native configuration lowers to a
**default program**: a validated graph of operators with effect contracts, which a reference interpreter defines and an
equivalence oracle checks. The program is compiled onto a physical frame fixed at boot, where synchronous operators fuse
into **regions** run by a generic driver. Changes install live as **generations**, with no loss or duplication. An
operator, or a demo standing in for Remote Config, can replace the program for the whole metrics path with an
**artifact**, a detached program that owns its policy as literals, and roll it back. Aggregation is a stateful kernel
that carries its state across changes, cutting over when a change requires it, such as a new bucket width. Outputs and
recipients are live resources (stage 7), declared by configuration. The path can run on partitioned **lanes** across
cores (stage 9, opt-in since V7b); without them it is one region between its sources and outputs.

## Where it stands

| Stage | What it delivered | Commit |
|---|---|---|
| Spec | The normative design | `e435160474` |
| 1 | Semantic core: IR, operators and contracts, validator, reference interpreter, equivalence oracle, explain | `f11cffe4a1` |
| 2 | Native kernels extracted from the components; native configuration lowered to the default program | `3e058a556a` |
| Syntax, demo | Policy surface syntax; the live demo server and UI | `a4c72a4f2a`, `d991c4b923` |
| 3 | The metrics path runs from the plan: frame, regions, live stateless generations | `f6ef9cddcb` |
| 4 | Policy gateways as routing operators; decisions, memoization, rewrites | `4c64b4c522` |
| 5 | Policy artifacts installed live through an artifact channel (local stub), with rollback | `26af093620` |
| 6 | Aggregation as a stateful kernel; watermarks, cut transitions | `b01049372e` |
| Docs | The status document and the stages 7 and 9 design | `2729a33271`, `d7316c474f` |
| 7 | Outputs, recipients, and delivery settings change live (resource generations) | `b7e63f147b` |
| 9 | Stage 9 foundations and first measurements; then partitioned lanes | `b523d9002b`, `8b44c153aa` |
| After 9 | One editable scope, `metrics`, over the whole metrics path (scopes later removed by V4); at most one aggregation per program (amendments 168, 169) | `e9fd618187` |
| Detached programs, phase 1 | Binding classes and host facts, the recipient directory, one aggregation's identity, editor annotations, version 1 fingerprints removed | `cfd4a28072` |
| Detached programs, phase 2 | The detached export, composition by value, `program` in status, emits addressed to recipients with implied outputs, facts and signals only, no node provenance, adoption, the standalone configuration reload | `7f9d67a033` |
| Detached programs, phase 3 | The demo: teams B and C as additional endpoints, destinations added and removed live, no resources artifacts, the scenarios reworked | `c9e27cea81` |
| What does nothing | Detached programs leave out identity nodes and constant splits (amendment 191) | `b1be5f7b72` |
| Version 2 plan | The premise shift (the program is the system), two modes not coexistence, ten simplification steps | `79555ebcb4` |
| V2 | Resources artifacts removed (amendment 192) | `06e7301501` |
| V3 | Native configuration lowers to literals; bindings are facts and signals (amendment 196) | `e6599d63f3` |
| V4, V5 | Programs are whole and a snapshot holds one program document: scopes, `base`, staleness, fingerprints, and splicing removed (amendment 197) | `53cd99b576` |
| V6 | Elaboration as a phase: one pass to the core program, which installs and has the one hash (amendment 198) | `9fb3990648` |
| V10 | Names and a compatibility analysis: every node reports under its name, the telemetry contract as warnings, any number of aggregations, a renamed aggregation is a new lineage; the node namespace, `single_aggregation`, and the adoption of `dsd_agg` removed (amendment 199) | `7b1e126d16`, `64fe645ca9`, (V10 commit) |
| V7a | State owners in any region: a sub-region for any state owner, a watermark over several sources, chained owners with boundary flushes, cuts and telemetry by node (amendments 200, 201) | `09129a6a71` to `546d5a2e0f`, (V7a commit) |
| Pipeline syntax | Programs written as streaming queries, a printer that writes any program as a person would, normalization and registry defaults (amendments 193, 194), and the demo moved to it | `0cbc6677d3` to `d35c5f8453` |
| N1a | Production CPU limits under the SMP tiers' steady loads (`run-limits.sh`), the quick loop and first hypotheses, the ADP efficiency findings, and the stall study (the encoder's burst after a flush) | `862764b247`, `7c7631a4bb`, `21f1d9d12f` |
| N1b | Production CPU limits through a burst across a flush (`run-regime.sh`), gates G1 to G6 | `cfa417d5d7`, `3a871929f3` |
| V7b | One region with an interleaved flush: state owners fuse, R1's cost model deleted, lanes opt-in (amendments 203 to 206); done as optionality, the merged layout the default | `ed58c17def` to `7e6b437476`, `cba809db95`, `de240e4005` |

Stage 8 (the production Remote Config channel) was skipped on purpose, and stage 10 (measured extensions) is not
started. **Current work is version 2**: its steps through V7b are done; N1 is measured twice
([where it stands](programmable-metrics-next.md#where-it-stands)), and N2 is now four layers, of which V7b built the
first; pacing and priority for admission are open. V8, V9, and V1 are next on the plan side; a new session starts at
the version 2 [handoff](programmable-metrics-v2.md#handing-off). Current state, in detail: [status: where things
stand](programmable-metrics-status.md#where-things-stand).

## Reading order

1. This index.
2. [The specification](programmable-metrics.md): the normative design. It is never edited by the implementation;
   changes are proposed as numbered amendments in the design documents.
3. [Status](programmable-metrics-status.md): the current state, how to run everything, a code map, changes users can
   see, **decisions awaiting the owner**, the amendments index, known limitations, and deferred work by stage.
4. [Deployment context and next steps](programmable-metrics-next.md): **what production looks like (limits of 0.3 to
   1.2 cores, 50 millicores in the worst case), what that means for the work, and the next chunk of work**, and the
   owner's direction that programs replace native policy. Its design is
   [detached programs](programmable-metrics-detached-programs.md), whose phases 1 to 3 (the last, the demo) are built.
   Then [version 2 simplifications](programmable-metrics-v2.md): **what the old premise needed and the order in which
   it is being removed**, with its progress.
5. [Implementation log](programmable-metrics-log.md): the history. What was tried, what failed, what reviews and
   integration found, measured results, and the decisions made along the way.
6. The designs as built, for depth:
   - [Implementation design, stages 1 to 6](programmable-metrics-implementation.md), with short as-built sections for
     [stage 7](programmable-metrics-implementation.md#stage-7-resource-generations-as-built) and
     [stage 9](programmable-metrics-implementation.md#stage-9-partitioned-execution-as-built).
   - [Design for stages 7 and 9](programmable-metrics-stages-7-and-9.md), with
     [stage 7 as built](programmable-metrics-stages-7-and-9.md#stage-7-as-built) and
     [stage 9 as built](programmable-metrics-stages-7-and-9.md#stage-9-as-built).
7. Reference: [artifact channel stub protocol](programmable-metrics-stub-protocol.md), the
   [measurement runbook](programmable-metrics-measurement.md), the container harnesses'
   [README](../../test/scaling/README.md), and [ADP efficiency findings](adp-efficiency-findings.md): what could make
   ADP cheaper independently of the plan.

## Where to find

| Question | Where |
|---|---|
| How do I run the demo, the checks, the gate? | [Status: running it](programmable-metrics-status.md#running-it) |
| Where does the code live? | [Status: code map](programmable-metrics-status.md#code-map) |
| What are we simplifying, and in what order? | [Version 2 simplifications](programmable-metrics-v2.md) |
| What changed for users against native ADP? | [Status: changes users can see](programmable-metrics-status.md#changes-users-can-see) |
| What did we change outside the new code, and is it permanent? | [Status: changes outside the new code](programmable-metrics-status.md#changes-outside-the-new-code) |
| What is waiting on a decision? | [Status: decisions awaiting the owner](programmable-metrics-status.md#decisions-awaiting-the-owner); [stages 7 and 9 open questions](programmable-metrics-stages-7-and-9.md#12-open-questions) |
| Which spec amendments are proposed? | [Status: amendments index](programmable-metrics-status.md#spec-amendments-index) (1 to 87); [stages 7 and 9 amendments](programmable-metrics-stages-7-and-9.md#10-proposed-spec-amendments) (88 to 192) |
| What are the risks and limitations? | [Status](programmable-metrics-status.md#known-limitations-and-risks); [implementation risks](programmable-metrics-implementation.md#risks); [stages 7 and 9 risks](programmable-metrics-stages-7-and-9.md#11-risks) |
| What should we work on next? | [Next steps](programmable-metrics-next.md#the-next-chunk-of-work) |
| Do programs replace native policy, and how? | [Direction](programmable-metrics-next.md#direction-programs-replace-native-policy); [detached programs](programmable-metrics-detached-programs.md) |
| What is left to build? | [Status: deferred work by stage](programmable-metrics-status.md#deferred-work-by-stage) |
| What alternatives were rejected? | [Stage 7](programmable-metrics-stages-7-and-9.md#418-alternatives-considered), [stage 9](programmable-metrics-stages-7-and-9.md#516-alternatives-considered), and the [log](programmable-metrics-log.md) |
| What are the performance numbers? | At production limits: [N1a and N1b](programmable-metrics-next.md#the-two-n1-measurements), [the first iterations](programmable-metrics-next.md#what-the-first-iterations-showed), and [V7b against N2](programmable-metrics-next.md#v7b-against-n2); earlier: [implementation risks](programmable-metrics-implementation.md#risks) (stages 3 to 6), [stage 9 measurements](programmable-metrics-stages-7-and-9.md#86-measurement-harness-and-what-is-measured-first), the [log](programmable-metrics-log.md) |
| What could improve ADP independently of the plan? | [ADP efficiency findings](adp-efficiency-findings.md) |

## Key results

- **Semantics.** The default program reproduces native behavior, checked against frozen copies of the native code,
  the reference interpreter, compatibility-matrix fixtures, and the spec's 15 transition traces, run serially and
  across lanes. Every intentional difference is listed under changes users can see and in the release notes.
- **Live changes.** Configuration, policy artifacts, aggregation width cuts, and output and recipient changes install
  without loss or duplication. One artifact edits the whole metrics path, routing and aggregation together.
  Continuous-input, fault, and 1,000-cycle reclamation tests cover them, and the demo check covers its scenarios S0 to
  S14.
- **Performance** (ratios to native, measured on one macOS host, not the gate):
  - stage 7: throughput 1.11 to 1.21, CPU per metric at most 1.01, p99 0.27 to 0.65;
  - stage 9 through the real DogStatsD source: throughput 0.96 to 1.07 at 1 to 8 workers, where stage 6's pipelining
    fell to 0.70 at 8; high-cardinality scaling ahead of native (2.84 against 2.56 times at 8 workers);
  - CPU per metric on lanes is 4 to 13% above native at 4 workers and more, from a fixed cost per buffer, which
    matters little now that production runs without lanes.
- **Performance at production CPU limits** (Linux, 0.05 to 1.2 cores, ratios to native): CPU per metric 0.93 to 1.00
  (4 to 6% less from the SMP medium tier up), 5 to 9% more capacity at saturation, and less memory under a crunch;
  below, [what the production regime showed](#what-the-production-regime-showed).

## What we learned about deployment

Production ADP runs with CPU limits of 0.3, 0.6, or 1.2 cores, and only 50 millicores guaranteed in Nodeless, where the
largest metric bursts arrive during CPU crunches. So **efficiency per metric and behavior under a CPU crunch matter
most; multi-core scaling barely matters.** Fusion (stages 3 to 6) is the relevant win; stage 9's lanes are off at these
sizes; R1's cost model adds hops for nothing; and the serial, blocking aggregation flush is the biggest practical
problem. The next chunk of work is to measure at the production limits, make the flush non-blocking without sharding,
fuse maximally at small budgets, and improve efficiency per metric: see
[deployment context and next steps](programmable-metrics-next.md). What happened since is below.

## What the production regime showed

**N1 was measured twice, and the two agree** ([the two N1
measurements](programmable-metrics-next.md#the-two-n1-measurements)).
[N1a](programmable-metrics-next.md#n1a-steady-load-against-the-smp-tiers) ran steady SMP-tier loads (`run-limits.sh`): from medium load up and from 0.3 to 1.2 cores, the plan uses 4 to 6% less
CPU per metric and less memory than `main`, and sustains 5 to 9% more at saturation (14 to 24% more at 0.05); under a
CPU crunch it keeps memory inside the SMP bounds where `main` does not; and the flush stall shows in steady state on
both. [N1b](programmable-metrics-next.md#n1b-bursts-across-a-flush) ran a burst across a flush (`run-regime.sh`): the
plan's CPU per metric is 0.93 to 1.00 of native and its peak memory 0.54 to 0.95, because its datagram pool barely
grows, but in an overloaded burst at 0.3 CPUs with 100,000 contexts it drops more than native, which parks the burst
in its pool. **The first iterations** ([results](programmable-metrics-next.md#what-the-first-iterations-showed))
found that more than 85% of ADP's instructions are in code the plan shares with `main`, that maximal fusion saves no
instructions, and that the plan's own runtime costs less than the Tokio overhead its fusion removes; so the plan work
now focuses on behavior under load, starting with flush stalls (N2). **Instrumenting a flush**
([results](programmable-metrics-next.md#what-stalls-admission-at-a-flush)) showed that the drain pauses admission for
only 30 to 75 ms, and that most of the stall is the encoder's burst of two to three seconds after it, CPU starvation
under the quota, the same on `main`; N2 became the first of four layers: non-blocking drain, pacing, priority for
admission, and shedding. **V7b built the first layer** with N3 (one region, R1's cost model gone, lanes opt-in): the
drain no longer blocks admission, but admission still falls after a flush on every image, which the encoder's burst
explains ([V7b against N2](programmable-metrics-next.md#v7b-against-n2)); fused and isolated layouts cost the same.
Findings that could land in ADP on their own are in [ADP efficiency findings](adp-efficiency-findings.md).

## What we decided about native policy

On 2026-10-14 the owner decided that **programs replace native policy**: a published program owns its policy settings
as literals and binds only facts and signals; native policy configuration only feeds the default program, which runs
when no artifact is published; status says when native policy settings are not applied. The
[direction](programmable-metrics-next.md#direction-programs-replace-native-policy) records it, and
[detached programs](programmable-metrics-detached-programs.md) designs the simplification that follows.

The premise behind stages 1 to 9, that programmability is added inside a native system that stays the authority, needed
machinery for coexistence. With native configuration as a front end and the program as the system, that machinery is
being removed step by step; program mode and native mode stay compatible and differentially testable, as two modes
rather than one shared system: [version 2 simplifications](programmable-metrics-v2.md).

## Main open questions

- The open questions of the [next steps](programmable-metrics-next.md#open-questions): what is acceptable during a CPU
  crunch, which policy production configurations use, and why the parallel quick loop skews CPU time.
- How to pace a flush and give admission priority over the encoder (N2's second and third layers), and what replaces
  G6's thresholds ([gates](programmable-metrics-next.md#gates-for-n2-to-n4)).
- A **Linux gate host** with at least 20 homogeneous cores is needed to decide the stage 9 gate, now a lower priority
  ([Q2](programmable-metrics-stages-7-and-9.md#12-open-questions)). Lanes are opt-in since V7b (amendment 204), and
  R1's fusion cost model is deleted (amendment 203).
- ADP's allocator setting, which caps parallel gains on Linux, and the DogStatsD datagram queue's ceiling
  ([decisions](programmable-metrics-status.md#decisions-awaiting-the-owner)).
- The spec amendments that need a decision, the clock-reset threshold, and the other items in that table.

## Notes outside the repository

The raw working notes (per-part handoffs, design drafts, judge verdicts, review findings, and measurement data) are
archived outside the repository, in `~/code/saluki-programmable-metrics-notes/` on the machine that did the work. The
documents above are the maintained record; the archive is only for digging into a specific finding.
