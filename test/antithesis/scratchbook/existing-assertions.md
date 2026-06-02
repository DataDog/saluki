---
sut_path: /home/ssm-user/src/saluki
commit: a31a487f1b8f676391dae88064c7d3f64f202245
updated: 2026-06-01
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

**A bootstrap-and-workload assertion set exists, plus the first liveness and Tier-1 property
instrumentation.** It comprises **24 SDK call sites** (12 prior + 12 Tier-1 property assertions landed
2026-06-01, tabled below): one lifecycle init and one bootstrap reachability probe in ADP, a
`finally_verify_delivery` `assert_reachable!`/`assert_sometimes!` pair, the
`parallel_driver_send_dogstatsd` anchors (one `assert_reachable!` plus five `assert_sometimes!`
covering send success, multi-value emission, and batch composition), the external `eventually_adp_alive` liveness
`assert_always!`, and the **first in-SUT property assertion**, an `assert_sometimes!` at the
forwarder 2xx site in `saluki-components`. All ADP/`saluki-components` sites are gated behind an
`antithesis` cargo feature (no-op in production). The bootstrap probe and the driver anchors remain
**integration probes / anti-vacuity anchors**; the liveness sites are real liveness instrumentation
(Category H `adp-stays-alive` and the good-function half of `adp-keeps-delivering` / in-SUT seed of
`forwarder-eventual-delivery`).

> [!NOTE]
> History: an early version of this file claimed no SDK assertions existed (true before the harness
> commit; corrected 2026-05-30). Updated 2026-05-31 when the liveness pieces landed (6 → 8 sites),
> again when `parallel_driver_send_dogstatsd` added the clean/feral/mixed batch assertions
> (8 → 11 sites), again when the 12 Tier-1 in-SUT property assertions landed 2026-06-01
> (11 → 23 sites), and again when the multi-value send anchor landed (23 → 24 sites).

## Assertions present

| File:line | Type | Message | Gating | Purpose |
|-----------|------|---------|--------|---------|
| `bin/agent-data-plane/src/main.rs:51` | `antithesis_init()` | (lifecycle init) | `#[cfg(feature = "antithesis")]` | Registers the assertion catalog before any are evaluated; no-op outside Antithesis, absent in prod builds. |
| `bin/agent-data-plane/src/main.rs:100` | `assert_reachable!` | "agent-data-plane completed bootstrap" | `#[cfg(feature = "antithesis")]` | Bootstrap-integration probe — proves the SDK is linked, cataloging works, the instrumentation path is wired. |
| `test/antithesis/harness/src/bin/finally_verify_delivery.rs:54` | `assert_reachable!` | "intake metrics dump query succeeded" | harness binary | Confirms the delivery-verification query path ran. |
| `test/antithesis/harness/src/bin/finally_verify_delivery.rs:59` | `assert_sometimes!` | "metrics delivered end-to-end to the intake" (`delivered > 0`) | harness binary | Workload-side liveness anchor — partially seeds `forwarder-eventual-delivery`. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:92` | `assert_reachable!` | "workload ran a dogstatsd batch" | harness binary | Confirms the DSD driver ran a batch. Details carry the attempted-line count and socket path. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:96` | `assert_sometimes!` | "workload sent a dogstatsd line" (`attempted > 0`) | harness binary | A batch can sample count == 0, so running does not imply sending. Proves a timeline sometimes actually sends a line. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:101` | `assert_sometimes!` | "workload emitted a multi-value metric" (`multi_value`) | harness binary | Proves a timeline sometimes emits a `:`-packed multi-value metric, the form ADP splits on colons. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:106` | `assert_sometimes!` | "workload ran a fully clean batch" (`attempted > 0 && Clean`) | harness binary | Proves the clean branch is sometimes exercised. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:111` | `assert_sometimes!` | "workload ran a fully feral batch" (`attempted > 0 && Feral`) | harness binary | Proves the feral branch is sometimes exercised. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:116` | `assert_sometimes!` | "workload ran a mixed batch" (`attempted > 0 && Mixed`) | harness binary | Proves the mixed branch is sometimes exercised. |
| `test/antithesis/harness/src/bin/eventually_adp_alive.rs:63` | `assert_always!` | "ADP booted: API reachable and DogStatsD socket present" | harness binary (`eventually_`, faults-paused) | Death-liveness for `adp-stays-alive` — fails the branch when ADP self-crashed (config panic / load) but stayed down through the quiet period. |
| `lib/saluki-components/src/common/datadog/io.rs:556` | `assert_sometimes!` | "ADP forwarded a payload to the intake" (`{ domain }`) | `#[cfg(feature = "antithesis")]` | First in-SUT property assertion — good-function liveness (the full pipeline ran to a 2xx) + replay checkpoint; good-function half of `adp-keeps-delivering`, in-SUT seed of `forwarder-eventual-delivery`. |

> **Load driver (2026-06-01):** `parallel_driver_send_dogstatsd` replaced the `parallel_driver_load`
> driver (the four-profile C1–C4 ladder and the `harness::load_gen` Generator/Profile module are gone).
> The driver samples a batch size (`random_range(0..=10_000)`), and for each line calls
> `harness::payload::dogstatsd::send`, which picks a message type via `random_choice` and dispatches to
> `metrics`/`events`/`service_checks`, then writes the bytes to the DSD UDS socket and exits. The
> generator builds names, tags, values, and headers combinatorially from finite segment pools joined by
> sampled separators (`harness::payload::dogstatsd::common`), with counts from the finite
> `harness::rand::Boundary` sampler. A per-message `Vibe` toggle is either clean (by-the-book) or feral
> (aberrant bytes, cursed-but-equivalent number encodings, skewed `_e{len,len}` event header lengths).
> Its six assertions above are the `assert_reachable!` batch anchor plus five `assert_sometimes!`
> anchors covering send success, multi-value emission, and batch composition.

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
  — the 12 call sites tabled above (`assert_always!` now present in `eventually_adp_alive`).
  `assert_unreachable!` is now present in-SUT as well: the ADP panic hook
  (`bin/agent-data-plane/src/main.rs`), the Tier-1 dispatch sites below (`sources/dogstatsd/mod.rs`),
  the object pools (`pooling/{elastic,fixed}.rs`), the DogStatsD codec (`deser/codec/dogstatsd/mod.rs`),
  and config readiness (`saluki-config/src/{lib.rs,dynamic/watcher.rs}`).

## Tier-1 in-SUT property assertions (landed 2026-06-01)

The first batch of net-new in-SUT **property** assertions landed in `saluki-components` behind the
existing `antithesis` feature — **12 call sites**, all using the numeric-guidance macros for bounded
invariants. They give three already-reproduced bugs an in-run falsification target.

| Site | Type | Property |
|------|------|----------|
| `transforms/aggregate/mod.rs` `insert` | `assert_always_less_than_or_equal_to!(contexts.len() <= context_limit)` + `assert_sometimes!(breached)` | `aggregate-context-limit-enforced` |
| `transforms/aggregate/mod.rs` `flush` | `assert_always!(current_time >= last_flush)` + `assert_always_less_than_or_equal_to!(bucket span)` + `assert_sometimes!(zero-value buckets generated)` | `aggregate-clock-skew-stable` (CONFIRMED bug) |
| `encoders/datadog/metrics/mod.rs` `encode_sketch_metric` | `assert_always!(value.is_finite())` at the sketch insert | `ddsketch-no-nan-poison` (CONFIRMED bug; encoder-bypass stopgap) |
| `sources/dogstatsd/replay/reader.rs` `read_next` | `assert_always_or_unreachable!(size == 0)` at the trailer/corruption fork | `replay-corruption-not-silent-eof` (CONFIRMED bug) |
| `sources/dogstatsd/mod.rs` `dispatch_events` | 2× `assert_unreachable!(output missing)` + 3× `assert_sometimes!(dispatch failed)` | `source-dispatch-no-misroute` |

The remaining net-new assertions (Tiers 2-3) each add the `antithesis` feature to one more crate; the
code-verified plan with HEAD lines, macros, and conditions is in `sut-side-instrumentation.md`.

## Implication for property work

Most catalog invariants are still **net-new instrumentation**, but the pattern is now proven in-SUT:

- `forwarder-eventual-delivery` now has an **in-SUT** `Sometimes(forwarded a payload)` at the 2xx
  site (io.rs:556) in addition to the workload-side `Sometimes(delivered > 0)`. The full no-loss
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
