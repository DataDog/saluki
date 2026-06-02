---
slug: aggregate-no-panic-any-window
title: No aggregate_window_duration value causes a panic
type: Safety / Reachability
priority: Low
sut_path: /home/ssm-user/src/saluki
commit: fc4bb29728814ddf9321572b954ec28f58faeb53
updated: 2026-05-30
status: FIXED UPSTREAM (window now NonZeroU64, PR #1772); retained as regression tripwire
---

# aggregate-no-panic-any-window — No `aggregate_window_duration` value causes a panic

> **Update (2026-05-30): FIXED UPSTREAM on main.** The original `% 0` / `step_by(0)` panic vector
> documented below is now structurally impossible. The config key was renamed
> `aggregate_window_duration_seconds` and is typed `NonZeroU64` (`transforms/aggregate/mod.rs:95-98`);
> `bucket_width_secs` is `NonZeroU64` end-to-end and `align_to_bucket_start` divides by
> `bucket_width_secs.get()` (`:822-823`), which can never be zero. A zero/sub-second value now fails
> config deserialization instead of reaching the divisor (PR #1772). The forensic detail below is
> retained as the historical evidence trail for the original defect; the property survives only as a
> low-cost `Unreachable` regression tripwire. The repro test
> `tests::bug_sub_second_aggregate_window_panics_on_insert` (in a sibling stack commit) is now stale
> and should be dropped or converted to a passing guard — see the bug ledger.

## Property (one sentence)
No configured `aggregate_window_duration_seconds` value causes the aggregate transform to panic;
the bucket-width divisor is `NonZeroU64` and can never be zero, so zero/sub-second values are
rejected at config load rather than reaching the modulo path.

## Origin
- SUT analysis §7 #8 (Wildcard): "Sub-second `aggregate_window_duration` → guaranteed panic:
  `bucket_width_secs = window.as_secs()` with no validation; a value < 1s yields `% 0`
  divide-by-zero and `step_by(0)` panics."

## Files / functions / lines (CONFIRMED)
- `lib/saluki-components/src/transforms/aggregate/mod.rs`
  - `AggregationState::new` (542–560): `bucket_width_secs: bucket_width.as_secs()` (553).
    For any `window_duration < 1s`, `as_secs()` truncates to **0**. No validation.
  - `align_to_bucket_start` (817–819): `timestamp - (timestamp % bucket_width_secs)` →
    **`% 0` panics** (`attempt to calculate the remainder with a divisor of zero`).
    Called from `insert` (579) on every metric and from `flush` (620, 628).
  - `flush` zero-value loop (630): `(start..current_time).step_by(bucket_width_secs as usize)`
    → **`step_by(0)` panics** (`step_by called with step == 0`). Reached on the 2nd+ flush
    (`self.last_flush != 0`, 627).
  - First reachable panic is in `insert` via `align_to_bucket_start` (579) — i.e. on the very
    first metric, before any flush, if `bucket_width_secs == 0`.
- Config plumbing (CONFIRMED no validation):
  - `AggregateConfiguration.window_duration` (92–93) deserialized via serde with
    `default = default_window_duration` (10s); no `#[validate]` / range check.
  - `from_configuration` (187–189): `config.as_typed()` — pure deserialize, no bounds check.
  - `config_registry/datadog/aggregate.rs` (7–13, 50–58): `aggregate_window_duration` declared
    as `ValueType::String`, `SupportLevel::Full`, `default: None`, `test_json {"secs":42}`.
    No minimum/positive-value constraint anywhere in the registry.
  - Repo-wide grep for `aggregate_window_duration` / `window_duration` shows zero validation
    sites (only definition, default, two uses, and a telemetry nanos field at 1534).

## Failure scenario
Operator sets `aggregate_window_duration: 500ms` (or any value `< 1s`, e.g. `{"secs":0,"nanos":...}`).
- `bucket_width_secs = 0`.
- First DSD metric reaching `dsd_agg` calls `insert` → `align_to_bucket_start(ts, 0)` → `% 0`
  panic → aggregate task panics → data topology component finishes unexpectedly →
  `wait_for_unexpected_finish` → **whole-process shutdown** (SUT analysis §2 supervision; data
  components are fail-stop, not restarted). s6 restarts ADP, which re-reads the same bad config
  and panics again → crash loop. This directly violates the "won't crash" guarantee.
- Note: a `1500ms` window does NOT panic (`as_secs() == 1`) but silently truncates the window
  to 1s — a separate correctness surprise worth a `Sometimes` observation.

## Observations
- The panic is deterministic given the config; Antithesis value is exercising the config space
  (including the `{"secs":0,"nanos":N}` Duration shape the registry advertises) and catching the
  crash, OR validating the planned fix.
- `PassthroughBatcher` also receives `window_duration` as `bucket_width` (220–224) but only uses
  it as a `Duration` for the rate interval (`counter_values_to_rate`), not as a divisor — so the
  passthrough path does not panic on sub-second windows; only the aggregation state path does.

## Config dependencies
- `aggregate_window_duration` (the sole trigger). Truncation via `as_secs()` means any value in
  `(0, 1s)` → 0 → panic; `[1s, ...)` → floor to whole seconds (lossy but safe).

## Suggested assertion
- If the fix is **validation at config load**: `assert_always(window_secs >= 1, ...)` at the
  point `AggregationState` is constructed (or `AlwaysOrUnreachable` that the build path rejects
  sub-second windows), plus `Unreachable` on the `% 0` / `step_by(0)` code path.
- If no fix: `assert_unreachable("aggregate align_to_bucket_start reached with bucket_width_secs == 0")`
  placed in `align_to_bucket_start` / before the `step_by` loop — Antithesis flags it the moment a
  sub-second window is fed.
- `Reachable("aggregate constructed with sub-second window")` to confirm the workload actually
  explores that config region.

## Open questions
- Should the fix clamp (`max(1)`), reject at load (fatal config error, consistent with §5 #5
  "config incompatibility is fatal at startup"), or support genuine sub-second bucketing
  (would require changing `bucket_width_secs` from `u64` seconds to a finer unit)? This changes
  whether the assertion is `Always(validated)` vs `Unreachable(panic path)`.
- Does the gRPC dynamic-config stream allow pushing `aggregate_window_duration` at runtime? If
  so, a mid-run config update to a sub-second window is a live crash vector, not just a startup one.
- SUT-side instrumentation needed: the divisor lives deep in `align_to_bucket_start`; an
  `Unreachable` assertion there is the cleanest signal (workload-side cannot observe the divisor).
