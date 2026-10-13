# Programmable metric processing: index

Start here. This page says what the programmable metrics work is, where it stands, and which document holds what, so
that the whole can be understood at a high level before reading any of the longer documents. Branch
`luke/programmable-metrics`; local commits only.

## In one paragraph

ADP's metrics path runs from a single logical plan instead of hard-wired components. Native configuration lowers to a
**default program**: a validated graph of operators with effect contracts, which a reference interpreter defines and
an equivalence oracle checks. The program is compiled onto a physical frame fixed at boot, where synchronous operators
fuse into **regions** run by a generic driver. Changes install live as **generations**, with no loss or duplication.
An operator, or a demo standing in for Remote Config, can replace the policy part of the program with an **artifact**
and roll it back. Aggregation is a stateful kernel that carries its state across changes, cutting over when a change
requires it, such as a new bucket width. Outputs and recipients are live resources (stage 7). The path runs on
partitioned **lanes** across cores (stage 9).

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

Stage 8 (the production Remote Config channel) was skipped on purpose, and stage 10 (measured extensions) is not
started. Current state, in detail: [status: where things stand](programmable-metrics-status.md#where-things-stand).

## Reading order

1. This index.
2. [The specification](programmable-metrics.md): the normative design. It is never edited by the implementation;
   changes are proposed as numbered amendments in the design documents.
3. [Status](programmable-metrics-status.md): the current state, how to run everything, a code map, changes users can
   see, **decisions awaiting the owner**, the amendments index, known limitations, and deferred work by stage.
4. [Deployment context and next steps](programmable-metrics-next.md): **what production looks like (limits of 0.3 to
   1.2 cores, 50 millicores in the worst case), what that means for the work, and the next chunk of work.**
5. [Implementation log](programmable-metrics-log.md): the history. What was tried, what failed, what reviews and
   integration found, measured results, and the decisions made along the way.
6. The designs as built, for depth:
   - [Implementation design, stages 1 to 6](programmable-metrics-implementation.md), with short as-built sections for
     [stage 7](programmable-metrics-implementation.md#stage-7-resource-generations-as-built) and
     [stage 9](programmable-metrics-implementation.md#stage-9-partitioned-execution-as-built).
   - [Design for stages 7 and 9](programmable-metrics-stages-7-and-9.md), with
     [stage 7 as built](programmable-metrics-stages-7-and-9.md#stage-7-as-built) and
     [stage 9 as built](programmable-metrics-stages-7-and-9.md#stage-9-as-built).
7. Reference: [artifact channel stub protocol](programmable-metrics-stub-protocol.md) and the
   [measurement runbook](programmable-metrics-measurement.md).

## Where to find

| Question | Where |
|---|---|
| How do I run the demo, the checks, the gate? | [Status: running it](programmable-metrics-status.md#running-it) |
| Where does the code live? | [Status: code map](programmable-metrics-status.md#code-map) |
| What changed for users against native ADP? | [Status: changes users can see](programmable-metrics-status.md#changes-users-can-see) |
| What did we change outside the new code, and is it permanent? | [Status: changes outside the new code](programmable-metrics-status.md#changes-outside-the-new-code) |
| What is waiting on a decision? | [Status: decisions awaiting the owner](programmable-metrics-status.md#decisions-awaiting-the-owner); [stages 7 and 9 open questions](programmable-metrics-stages-7-and-9.md#12-open-questions) |
| Which spec amendments are proposed? | [Status: amendments index](programmable-metrics-status.md#spec-amendments-index) (1 to 87); [stages 7 and 9 amendments](programmable-metrics-stages-7-and-9.md#10-proposed-spec-amendments) (88 on) |
| What are the risks and limitations? | [Status](programmable-metrics-status.md#known-limitations-and-risks); [implementation risks](programmable-metrics-implementation.md#risks); [stages 7 and 9 risks](programmable-metrics-stages-7-and-9.md#11-risks) |
| What should we work on next? | [Next steps](programmable-metrics-next.md#the-next-chunk-of-work) |
| What is left to build? | [Status: deferred work by stage](programmable-metrics-status.md#deferred-work-by-stage) |
| What alternatives were rejected? | [Stage 7](programmable-metrics-stages-7-and-9.md#418-alternatives-considered), [stage 9](programmable-metrics-stages-7-and-9.md#516-alternatives-considered), and the [log](programmable-metrics-log.md) |
| What are the performance numbers? | [Implementation risks](programmable-metrics-implementation.md#risks) (stages 3 to 6); [stage 9 measurements](programmable-metrics-stages-7-and-9.md#86-measurement-harness-and-what-is-measured-first); the [log](programmable-metrics-log.md) |

## Key results

- **Semantics.** The default program reproduces native behavior, checked against frozen copies of the native code,
  the reference interpreter, compatibility-matrix fixtures, and the spec's 15 transition traces, run serially and
  across lanes. Every intentional difference is listed under changes users can see and in the release notes.
- **Live changes.** Configuration, policy artifacts, aggregation width cuts, and output and recipient changes install
  without loss or duplication. Continuous-input, fault, and 1,000-cycle reclamation tests cover them, and the demo
  check covers its scenarios S0 to S14.
- **Performance** (ratios to native, measured on one macOS host, not the gate):
  - stage 7: throughput 1.11 to 1.21, CPU per metric at most 1.01, p99 0.27 to 0.65;
  - stage 9 through the real DogStatsD source: throughput 0.96 to 1.07 at 1 to 8 workers, where stage 6's pipelining
    fell to 0.70 at 8; high-cardinality scaling ahead of native (2.84 against 2.56 times at 8 workers);
  - CPU per metric on lanes is 4 to 13% above native at 4 workers and more, from a fixed cost per buffer: the main
    open performance item.

## What we learned about deployment

Production ADP runs with CPU limits of 0.3, 0.6, or 1.2 cores, and only 50 millicores guaranteed in Nodeless, where the
largest metric bursts arrive during CPU crunches. So **efficiency per metric and behavior under a CPU crunch matter
most; multi-core scaling barely matters.** Fusion (stages 3 to 6) is the relevant win; stage 9's lanes are off at these
sizes; R1's cost model adds hops for nothing; and the serial, blocking aggregation flush is the biggest practical
problem. The next chunk of work is to measure at the production limits, make the flush non-blocking without sharding,
fuse maximally at small budgets, and improve efficiency per metric: see
[deployment context and next steps](programmable-metrics-next.md).

## Main open questions

- The open questions of the [next steps](programmable-metrics-next.md#open-questions): which workloads and bursts to
  model, and what is acceptable during a CPU crunch.
- A **Linux gate host** with at least 20 homogeneous cores is needed to decide the stage 9 gate, now a lower priority
  ([Q2](programmable-metrics-stages-7-and-9.md#12-open-questions)). Lanes are on by default before that gate
  ([Q7](programmable-metrics-stages-7-and-9.md#12-open-questions)).
- Whether to delete R1's fusion cost model ([P9.7](programmable-metrics-stages-7-and-9.md#p97-r1s-cost-model)).
- ADP's allocator setting, which caps parallel gains on Linux, and the DogStatsD datagram queue's ceiling
  ([decisions](programmable-metrics-status.md#decisions-awaiting-the-owner)).
- The spec amendments that need a decision, the clock-reset threshold, and the other items in that table.

## Notes outside the repository

The raw working notes (per-part handoffs, design drafts, judge verdicts, review findings, and measurement data) are
archived outside the repository, in `~/code/saluki-programmable-metrics-notes/` on the machine that did the work. The
documents above are the maintained record; the archive is only for digging into a specific finding.
