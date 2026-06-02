---
sut_path: /home/ssm-user/src/saluki
commit: fc4bb29728814ddf9321572b954ec28f58faeb53
updated: 2026-05-30
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: Datadog ADP Confluence space (design notes, weekly summaries, gap analyses) consulted for grounding.
  - path: https://datadoghq.atlassian.net/browse/DADP
    why: ADP Jira project for incidents and tracked gaps.
  - path: https://github.com/DataDog/saluki/pull/1768
    why: PR review #4393897611 — re-running research caught that the harness now adds SDK assertions this file previously denied.
---

# Existing Antithesis SDK Assertions

## Summary

**A small bootstrap-and-workload assertion set exists**, added by the Antithesis harness commit
(`chore(agent-data-plane): Antithesis test harness and workload`, the parent of this scratchbook
commit). It comprises **6 SDK call sites** across three binaries: one lifecycle init and one
bootstrap reachability probe in ADP (both gated behind the `antithesis` cargo feature, no-op in
production), plus two workload-side `assert_reachable!`/`assert_sometimes!` pairs in the harness test
commands. These are **integration probes and anti-vacuity anchors**, not the property-catalog
invariants — none of the 35 cataloged property assertions is implemented yet.

> [!NOTE]
> A prior version of this file stated no SDK assertions existed. That was true before the harness
> commit landed; it is now stale. Re-research on 2026-05-30 corrected it.

## Assertions present

| File:line | Type | Message | Gating | Purpose |
|-----------|------|---------|--------|---------|
| `bin/agent-data-plane/src/main.rs:51` | `antithesis_init()` | (lifecycle init) | `#[cfg(feature = "antithesis")]` | Registers the assertion catalog before any are evaluated; no-op outside Antithesis, absent in prod builds. |
| `bin/agent-data-plane/src/main.rs:100` | `assert_reachable!` | "agent-data-plane completed bootstrap" | `#[cfg(feature = "antithesis")]` | Bootstrap-integration probe — proves the SDK is linked, cataloging works, the instrumentation path is wired. |
| `test/antithesis/harness/src/bin/finally_verify_delivery.rs:54` | `assert_reachable!` | "intake metrics dump query succeeded" | harness binary | Confirms the delivery-verification query path ran. |
| `test/antithesis/harness/src/bin/finally_verify_delivery.rs:59` | `assert_sometimes!` | "metrics delivered end-to-end to the intake" (`delivered > 0`) | harness binary | Workload-side liveness anchor — partially seeds `forwarder-eventual-delivery`. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:77` | `assert_reachable!` | "workload sent a dogstatsd batch" | harness binary | Confirms the DSD driver actually emitted load. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:87` | `assert_sometimes!` | "workload drove a high-cardinality dogstatsd flood" (`regime == High`) | harness binary | Anti-vacuity anchor that timelines reach the high-cardinality regime — seeds `rss-bounded-under-cardinality`. |

Dependency wiring: ADP gains the SDK only under the `antithesis` feature
(`bin/agent-data-plane/Cargo.toml:14` → `dep:antithesis_sdk`, `antithesis_sdk/full`,
`dep:antithesis-instrumentation`); the harness crate depends on `antithesis_sdk` unconditionally
(`test/antithesis/harness/Cargo.toml`). `antithesis-instrumentation` is an external build-time
instrumentation crate, not a source of in-tree assertions.

## How this was determined

Searched the repository with ripgrep over `*.rs` and `*.toml`:

- `rg -li "antithesis" -g '*.rs' -g '*.toml'` — matches in ADP `main.rs`, the two harness binaries,
  and the `Cargo.toml` files above.
- `rg "assert_always|assert_sometimes|assert_reachable|assert_unreachable|antithesis_sdk" -g '*.rs'`
  — the 6 call sites tabled above; **no `assert_always!` and no `assert_unreachable!` anywhere yet.**

## Implication for property work

The catalog's invariants are still **net-new instrumentation**. The two `assert_sometimes!` anchors
above are workload-side only and serve anti-vacuity, not the safety/liveness invariants themselves:

- `forwarder-eventual-delivery` has a workload-side `Sometimes(delivered > 0)` but no SUT-side
  no-loss `Always`/accounting assertion — that remains to be added.
- `rss-bounded-under-cardinality` has its high-cardinality `Sometimes` anchor but no SUT-side RSS or
  interner-bound `Always` — also net-new.
- The ~17 properties requiring in-process SUT-side assertions (per evaluation R2) still need ADP to
  be forked and instrumented behind the `antithesis` feature. The feature scaffold now exists, which
  lowers that bar — the `antithesis_init()` + bootstrap probe prove the wiring works end-to-end.

Other existing (non-Antithesis) signals remain available to anchor assertions or workload-side
checkers:

- **Internal telemetry counters** via the `metrics` crate (`events_discarded_total`,
  `events_sent_total`, aggregate `context_limit` breach counters, forwarder queue-drop counters).
- **Rust unit tests** with std `assert!`/`assert_eq!`, dense across `saluki-components`,
  `saluki-core`, `saluki-io`, and `ddsketch` — not Antithesis assertions; run only under `cargo test`.
- A `loom` cfg in `lib/stringtheory/src/interning/` — the authors already treat the interner's
  reclamation path as concurrency-sensitive.
