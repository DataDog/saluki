---
sut_path: /home/ssm-user/src/saluki
commit: 21b2072b4743ddbf4c84891d93abac7299dc4ce8
updated: 2026-06-01
external_references:
  - path: https://datadoghq.atlassian.net/wiki/spaces/DADP/
    why: ADP Confluence space — headline guarantees that frame these defects as bugs.
  - path: https://github.com/DataDog/saluki/pull/1768
    why: PR review #4393897611 / Codex P2 flagged the now-stale aggregate panic repro reconciled here.
---

# Bug Ledger

Accounting of every defect discovered during `antithesis-research`, with the vehicle used to
demonstrate it. Goal: burn each discovered bug into a local repro case or an Antithesis triage shot.
**No fixes were applied.** Following TDD, each local repro asserts the *desired* invariant, so it
**currently FAILS** against the buggy code — the failing test *is* the demonstration. It turns green
once the defect is fixed (a ready-made regression guard). Each test's comment describes why/how the
bug happens.

## Burned into local repro cases (failing unit tests)

| # | Property | Test | Defect |
|---|----------|------|--------|
| 1 | `ddsketch-no-nan-poison` | `lib/ddsketch/src/agent/sketch.rs::tests::bug_nan_sample_poisons_sum_and_avg` | One NaN sample permanently poisons sketch `sum`/`avg` (sticky), `count`/`min`/`max` stay valid → silent corruption; no finiteness guard at the sketch boundary. |
| 2 | `replay-corruption-not-silent-eof` | `lib/saluki-components/.../sources/dogstatsd/replay/reader.rs::tests::bug_corrupt_length_prefix_silently_drops_following_records` | A corrupt/oversized length prefix is read as clean EOF → all following well-formed records silently dropped (false replay fidelity / data loss). |
| 3 | `aggregate-clock-skew-stable` (forward-jump facet) | `lib/saluki-components/.../transforms/aggregate/mod.rs::tests::bug_forward_clock_jump_floods_zero_value_points` | No `current_time >= last_flush` guard; a forward wall-clock jump builds `zero_value_buckets` over the whole interval — O(jump) work/alloc — and floods one idle counter with points proportional to the jump. |
| 4 | `rss-bounded-under-cardinality` / `interner-full-bounded` (root cause) | `lib/saluki-context/src/resolver.rs::tests::bug_default_heap_fallback_makes_context_resolution_unbounded` | `allow_heap_allocations` defaults true → a full fixed-size interner silently spills to the heap and `resolve` never refuses → unbounded memory under a high-cardinality flood; the only bounded mode (heap disallowed) silently drops. |
| 5 | `config-stall-no-deadlock` | `lib/saluki-config/src/lib.rs::tests::bug_config_ready_hangs_forever_without_snapshot` | `GenericConfiguration::ready()` awaits the first dynamic snapshot with no internal timeout; with the sender held open and silent, it never resolves → ADP startup hangs forever. |

Run all five (expect five FAILURES — the failing tests are the demonstrations):
`cargo nextest run --no-fail-fast -E 'test(/bug_nan_sample_poisons_sum_and_avg|bug_corrupt_length_prefix_silently_drops_following_records|bug_forward_clock_jump_floods_zero_value_points|bug_default_heap_fallback_makes_context_resolution_unbounded|bug_config_ready_hangs_forever_without_snapshot/)'`

> **Workload reach (2026-06-01):** the live `parallel_driver_send_dogstatsd` feral DSD-line generator
> exercises only **one** of these five repros under a run — #4, the high-cardinality interner
> heap-fallback (`rss-bounded-under-cardinality`) — and even that needs a memory-capped `adp` container
> or a SUT-side RSS assertion to be *caught* (neither yet wired). The other four are off the DSD-socket
> input path:
> - **#1 `ddsketch-no-nan-poison`** — DSD drops non-finite at the codec; needs a `checks_ipc` gRPC
>   Histogram feeder.
> - **#2 `replay-corruption-not-silent-eof`** — needs the `agent-data-plane dogstatsd replay` CLI plus
>   crafted capture files.
> - **#3 `aggregate-clock-skew-stable` (forward-jump)** — needs a clock-skip fault.
> - **#5 `config-stall-no-deadlock`** — needs a config-stream stub that withholds the snapshot.

## Resolved upstream on main (repro now stale)

- **`aggregate-no-panic-any-window` — sub-second window `% 0` panic (was bug #1).** Fixed on main:
  the config key is renamed `aggregate_window_duration_seconds` and typed `NonZeroU64`, and
  `align_to_bucket_start` divides by `bucket_width_secs.get()` end-to-end
  (`transforms/aggregate/mod.rs:95-98,822-823`), so a zero/sub-second window now fails config
  parsing instead of reaching the divisor (PR #1772). The repro
  `tests::bug_sub_second_aggregate_window_panics_on_insert` lives in a **sibling stack commit**
  (`chore(agent-data-plane): failing repros for six discovered bugs`), not this docs commit. **Action
  required there (out of scope for this commit):** delete that test or convert it to a passing
  regression guard, since the desired invariant now holds. The catalog entry is reframed as a
  low-cost `Unreachable` regression tripwire.

## Burned into an Antithesis triage shot (submitted run)

- **`rss-bounded-under-cardinality` (behavioral)** and **`forwarder-eventual-delivery` (baseline liveness)** — run id (redacted; tracked internally) (test-name `saluki-adp-bug-hunt`, 30 min, submitted 2026-05-29). The `parallel_driver_send_dogstatsd` driver (a sampled batch of feral DSD lines whose names/tags/values are built combinatorially from finite segment pools) floods distinct contexts and drives memory growth; `finally_verify_delivery` checks delivery. Triage with the `antithesis-triage` skill once it completes. **Caveat:** `rss-bounded-under-cardinality` only becomes a *caught* failure with a memory-capped `adp` container (OOM ⇒ `eventually_adp_alive`) or a SUT-side RSS assertion — neither yet wired.

## Antithesis-shot-only — blocked on harness infrastructure (not locally reproducible)

These discovered defects cannot be reproduced as local unit tests and require infrastructure not yet
built. Each is a follow-up Antithesis shot, not a current repro.

- **`memory-limiter-survives-rss-read-failure`** — `check_memory_usage` does `querier.resident_set_size().expect(...)`; an RSS-read failure panics the checker thread, silently disabling memory protection. **Not locally reproducible:** the `Querier` is constructed internally with no injection seam. **Shot blocker:** a custom `/proc` fault (enabled for the tenant) + a memory-limiter-enabled ADP config variant + a SUT-side `assert_unreachable!` at the `.expect` site.
- **`config-runtime-update-not-revalidated`** — the incompatibility gate runs only at startup; a runtime config-stream update can introduce a high-severity-incompatible key with no re-gate. **Shot blocker:** config-stream add-on **and** human confirmation of intent (intentional trust of the authoritative Agent vs. oversight) — flagged `(needs human input)` in the catalog.
- **`shutdown-drains-no-loss` / `graceful-shutdown-within-30s` (forceful-stop data loss)** — behavioral; needs the running harness shut down under a slow/blocked intake to exceed the 30s grace window. **Shot blocker:** an intake failure-mode toggle + a shutdown driver.

## Recorded as covered / not a distinct defect

- **`aggregate-clock-skew-stable` (backward-jump gap)** — dual of bug #4; same root cause (no monotonicity guard on `current_time` vs `last_flush`). Covered by #4's triage.
- **aggregate context-limit counts contexts, not bytes** — a single context with many timestamped values has unbounded value memory; a facet of `rss-bounded-under-cardinality`, partially covered by #5 and the catalog's open question.
- **`ddsketch-relative-error-bound` merge non-associativity** — f64 ordering sensitivity; a library-level numeric property, not a clear ADP runtime defect (the catalog demoted it to a harness/proptest invariant).
- **`source-dispatch-no-misroute`** — misroute is structurally improbable with the current `extract`-then-`send_all` ordering; the live facet (silent uncounted loss on a downstream dispatch error) would need a failing-dispatcher harness. Candidate future local repro; not a confirmed defect today.

## Status

Cleanly-reproducible discovered bugs that remain open: **5 local repros + 1 submitted run.** One
original repro (the aggregate sub-second `% 0` panic) was **fixed upstream on main** (PR #1772) and is
recorded above as resolved-stale — its repro in the sibling stack commit needs removal/conversion. The
remaining items are explicitly blocked on harness infrastructure (custom `/proc` fault, config-stream
add-on, intake failure toggle) or human input, and are recorded above as follow-up Antithesis shots.
No further bug is reproducible without building that infrastructure.
