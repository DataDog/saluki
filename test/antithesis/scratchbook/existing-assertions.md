---
sut_path: /home/ssm-user/src/saluki
commit: 2e4ae1b8be45143882f0dbeb5e74998021c5faf9
updated: 2026-05-31
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

**A bootstrap-and-workload assertion set exists, now with the first liveness instrumentation.** It
comprises **8 SDK call sites**: one lifecycle init and one bootstrap reachability probe in ADP, two
workload-side `assert_reachable!`/`assert_sometimes!` pairs in the harness drivers, and — added
2026-05-31 — the external `eventually_adp_alive` liveness `assert_always!` plus the **first in-SUT
property assertion**, an `assert_sometimes!` at the forwarder 2xx site in `saluki-components`. All
ADP/`saluki-components` sites are gated behind an `antithesis` cargo feature (no-op in production).
The bootstrap probe and the two driver anchors remain **integration probes / anti-vacuity anchors**;
the two new sites are real liveness instrumentation (Category H `adp-stays-alive` and the
good-function half of `adp-keeps-delivering` / in-SUT seed of `forwarder-eventual-delivery`).

> [!NOTE]
> History: an early version of this file claimed no SDK assertions existed (true before the harness
> commit; corrected 2026-05-30). Updated again 2026-05-31 when the liveness pieces landed (6 → 8
> sites).

## Assertions present

| File:line | Type | Message | Gating | Purpose |
|-----------|------|---------|--------|---------|
| `bin/agent-data-plane/src/main.rs:51` | `antithesis_init()` | (lifecycle init) | `#[cfg(feature = "antithesis")]` | Registers the assertion catalog before any are evaluated; no-op outside Antithesis, absent in prod builds. |
| `bin/agent-data-plane/src/main.rs:100` | `assert_reachable!` | "agent-data-plane completed bootstrap" | `#[cfg(feature = "antithesis")]` | Bootstrap-integration probe — proves the SDK is linked, cataloging works, the instrumentation path is wired. |
| `test/antithesis/harness/src/bin/finally_verify_delivery.rs:54` | `assert_reachable!` | "intake metrics dump query succeeded" | harness binary | Confirms the delivery-verification query path ran. |
| `test/antithesis/harness/src/bin/finally_verify_delivery.rs:59` | `assert_sometimes!` | "metrics delivered end-to-end to the intake" (`delivered > 0`) | harness binary | Workload-side liveness anchor — partially seeds `forwarder-eventual-delivery`. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:77` | `assert_reachable!` | "workload sent a dogstatsd batch" | harness binary | Confirms the DSD driver actually emitted load. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:87` | `assert_sometimes!` | "workload drove a high-cardinality dogstatsd flood" (`regime == High`) | harness binary | Anti-vacuity anchor that timelines reach the high-cardinality regime — seeds `rss-bounded-under-cardinality`. |
| `test/antithesis/harness/src/bin/eventually_adp_alive.rs:62` | `assert_always!` | "ADP booted: API reachable and DogStatsD socket present" | harness binary (`eventually_`, faults-paused) | Death-liveness for `adp-stays-alive` — fails the branch when ADP self-crashed (config panic / load) but stayed down through the quiet period. |
| `lib/saluki-components/src/common/datadog/io.rs:553` | `assert_sometimes!` | "ADP forwarded a payload to the intake" (`{ domain }`) | `#[cfg(feature = "antithesis")]` | First in-SUT property assertion — good-function liveness (the full pipeline ran to a 2xx) + replay checkpoint; good-function half of `adp-keeps-delivering`, in-SUT seed of `forwarder-eventual-delivery`. |

Dependency wiring: ADP gains the SDK only under the `antithesis` feature
(`bin/agent-data-plane/Cargo.toml:14` → `dep:antithesis_sdk`, `antithesis_sdk/full`,
`dep:antithesis-instrumentation`, and now `saluki-components/antithesis`). As of 2026-05-31
`saluki-components` has its own optional `antithesis` feature (`dep:antithesis_sdk`,
`antithesis_sdk/full`), enabled transitively by the ADP feature — this is what lets in-SUT property
assertions live in the component crate, not just in the ADP binary. The harness crate depends on
`antithesis_sdk` unconditionally (`test/antithesis/harness/Cargo.toml`). `antithesis-instrumentation`
is an external build-time instrumentation crate, not a source of in-tree assertions.

## How this was determined

Searched the repository with ripgrep over `*.rs` and `*.toml`:

- `rg -li "antithesis" -g '*.rs' -g '*.toml'` — matches in ADP `main.rs`, the two harness binaries,
  and the `Cargo.toml` files above.
- `rg "assert_always|assert_sometimes|assert_reachable|assert_unreachable|antithesis_sdk" -g '*.rs'`
  — the 6 call sites tabled above; **no `assert_always!` and no `assert_unreachable!` anywhere yet.**

## Implication for property work

Most catalog invariants are still **net-new instrumentation**, but the pattern is now proven in-SUT:

- `forwarder-eventual-delivery` now has an **in-SUT** `Sometimes(forwarded a payload)` at the 2xx
  site (io.rs:553) in addition to the workload-side `Sometimes(delivered > 0)`. The full no-loss
  `Always`/accounting reconciliation (delivered == accepted-and-retryable after a transient outage)
  is still net-new.
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
