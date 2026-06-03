---
sut_path: /home/ssm-user/src/saluki
commit: 21b2072b4743ddbf4c84891d93abac7299dc4ce8
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

**A bootstrap-and-workload assertion set exists, now with the first liveness instrumentation.** It
comprises **11 SDK call sites**: one lifecycle init and one bootstrap reachability probe in ADP, a
`finally_verify_delivery` `assert_reachable!`/`assert_sometimes!` pair, the
`parallel_driver_send_dogstatsd` anchors (one `assert_reachable!` plus four `assert_sometimes!` —
delivered, clean, feral, mixed batch composition), the external `eventually_adp_alive` liveness
`assert_always!`, and the **first in-SUT property assertion**, an `assert_sometimes!` at the
forwarder 2xx site in `saluki-components`. All ADP/`saluki-components` sites are gated behind an
`antithesis` cargo feature (no-op in production). The bootstrap probe and the driver anchors remain
**integration probes / anti-vacuity anchors**; the liveness sites are real liveness instrumentation
(Category H `adp-stays-alive` and the good-function half of `adp-keeps-delivering` / in-SUT seed of
`forwarder-eventual-delivery`).

> [!NOTE]
> History: an early version of this file claimed no SDK assertions existed (true before the harness
> commit; corrected 2026-05-30). Updated 2026-05-31 when the liveness pieces landed (6 → 8 sites),
> and again when `parallel_driver_send_dogstatsd` added the clean/feral/mixed batch assertions
> (8 → 11 sites).

## Assertions present

| File:line | Type | Message | Gating | Purpose |
|-----------|------|---------|--------|---------|
| `bin/agent-data-plane/src/main.rs:51` | `antithesis_init()` | (lifecycle init) | `#[cfg(feature = "antithesis")]` | Registers the assertion catalog before any are evaluated; no-op outside Antithesis, absent in prod builds. |
| `bin/agent-data-plane/src/main.rs:100` | `assert_reachable!` | "agent-data-plane completed bootstrap" | `#[cfg(feature = "antithesis")]` | Bootstrap-integration probe — proves the SDK is linked, cataloging works, the instrumentation path is wired. |
| `test/antithesis/harness/src/bin/finally_verify_delivery.rs:54` | `assert_reachable!` | "intake metrics dump query succeeded" | harness binary | Confirms the delivery-verification query path ran. |
| `test/antithesis/harness/src/bin/finally_verify_delivery.rs:59` | `assert_sometimes!` | "metrics delivered end-to-end to the intake" (`delivered > 0`) | harness binary | Workload-side liveness anchor — partially seeds `forwarder-eventual-delivery`. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:67` | `assert_reachable!` | "workload ran a dogstatsd batch" | harness binary | Confirms the DSD driver ran a batch; details carry the attempted-line count and socket path. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:68` | `assert_sometimes!` | "workload delivered a dogstatsd line" (`attempted > 0`) | harness binary | Anti-vacuity anchor: a batch can sample count == 0, so "ran" does not imply "sent"; this proves a timeline sometimes actually delivers a line, else delivery checks are vacuous. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:73` | `assert_sometimes!` | "workload ran a fully clean batch" (`attempted > 0 && Clean`) | harness binary | Composition anchor: proves the clean branch is sometimes exercised, so the clean delivery surface is non-vacuous. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:78` | `assert_sometimes!` | "workload ran a fully feral batch" (`attempted > 0 && Feral`) | harness binary | Composition anchor: proves the feral branch is sometimes exercised. |
| `test/antithesis/harness/src/bin/parallel_driver_send_dogstatsd.rs:83` | `assert_sometimes!` | "workload ran a mixed batch" (`attempted > 0 && Mixed`) | harness binary | Composition anchor: proves the mixed branch is sometimes exercised. |
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
> Its five assertions above are the `assert_reachable!` batch anchor plus four `assert_sometimes!`
> anchors (delivered, and the clean/feral/mixed batch-composition checks).

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
  — the 11 call sites tabled above (`assert_always!` now present in `eventually_adp_alive`); **no
  `assert_unreachable!` anywhere yet.**

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
