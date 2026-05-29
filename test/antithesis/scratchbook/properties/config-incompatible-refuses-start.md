---
slug: config-incompatible-refuses-start
title: High-severity incompatible config refuses to start the pipeline
family: Lifecycle Transitions & Configuration
type: Safety (Reachability / Unreachable)
priority: Medium
status: assertion-missing
sut_commit: fc4bb29728814ddf9321572b954ec28f58faeb53
---

# config-incompatible-refuses-start

## Origin

SUT analysis §5 Safety #5 ("Config incompatibility is fatal at startup (high-severity incompatible
non-default key → refuse to run)") and §6 (integration suite has "config-check exit codes" cases).

## Files / functions / lines

- `bin/agent-data-plane/src/cli/run.rs:157`:
  ```rust
  check_and_warn_config(&config).error_context("Incompatible configuration detected.")?;
  ```
  Crucial ordering: this is at run.rs:157, **before** `create_topology` (run.rs:168),
  `blueprint.build()` (run.rs:238), and `built_topology.spawn(...)` (run.rs:239). The `?` propagates
  the error out of `handle_run_command` before any data component is ever built or spawned.
- `bin/agent-data-plane/src/cli/run.rs:331-381` — `check_and_warn_config`:
  - Iterates `config.flattened_keys()` (run.rs:336-339).
  - `config_classifier.classify(&key, &val)` (run.rs:341) → `None` skips invalid/N-A keys.
  - `classification.is_default` (run.rs:346) → keys with default values are **skipped** (the Agent
    populates defaults, so only **non-default** values count).
  - Match on `support_level` (run.rs:351-370):
    - `Full` / `Incompatible(Low)` / `Partial` / `Incompatible(Medium)` → log + **Proceed**.
    - `Incompatible(Severity::High)` (run.rs:362-366) → `error!` log +
      `high_severity_incompatibilities += 1`.
    - `Ignored` / `Unrecognized` → silently ignored.
  - run.rs:373-378: if `high_severity_incompatibilities > 0`, returns
    `Err(generic_error!("{n} incompatible configuration detected. ADP cannot start. …"))`.
    **All keys are checked before returning** (so the count and all error logs are complete).
- `bin/agent-data-plane/src/main.rs:136-146` — the `Err` from `handle_run_command` maps to
  `Some(1)`; `main.rs:101-104` calls `std::process::exit(1)`. So a high-severity incompatibility →
  **process exit code 1, pipeline never spawned.**
- `lib/saluki-components/src/config_registry/classifier.rs:42-51` — `classify`: looks up the schema
  entry, returns `Classification { support_level, is_default }`.
- `lib/saluki-components/src/config_registry/classifier.rs:53-…` — `is_default_value`: compares the
  value against the schema's documented default (incl. null/empty-string handling, alias handling).
- `lib/saluki-components/src/config_registry/mod.rs:144-175` — `Severity { Low, Medium, High }` and
  `SupportLevel::{Full, Partial, Incompatible(Severity), Ignored, Unrecognized}`. Incompatible keys
  live in `config_registry/datadog/unsupported.rs` with their severity.

## Key observation

The refuse-to-start gate is correctly placed **before** topology build/spawn, so the safety
property "pipeline never runs with a high-severity-incompatible non-default key" is structurally
enforced by control flow, not by a runtime check inside the running pipeline. The two strongest
Antithesis assertions:

- **Unreachable:** "data pipeline spawned while a high-severity incompatible non-default config key
  is present." Place an `assert_unreachable!` reading a flag (set during `check_and_warn_config`
  when a high-severity incompatibility was seen) at/after `spawn()` (run.rs:239). Because the `?` at
  run.rs:157 returns first, this point is never reached with such a key set — Antithesis confirms it.
- **Reachable:** "ADP refused to start due to incompatible config" — mark the
  `high_severity_incompatibilities > 0` return path (run.rs:373) as reachable so the workload knows
  the refusal path is actually exercised under the incompatible-config workload.

## Failure scenario (Antithesis angle)

- Workload injects a config containing a known high-severity-incompatible key with a **non-default**
  value (sourced from `config_registry/datadog/unsupported.rs` `Severity::High` entries). Expect:
  process exits with code 1, no `topology_ready_ms` log, no listener bound, no data forwarded.
- Negative control: same key but at its **default** value → `is_default` true → skipped → ADP
  starts normally. Assert the refusal path is NOT taken (the gate must not be over-eager).
- Multiple high-severity keys → still exits 1, error reports the count (run.rs:374-377). Verify all
  are logged before exit (debuggability invariant).
- Medium/Low/Partial incompatible keys → ADP proceeds (warn/debug only). Assert pipeline DOES start
  — confirms severity gating is graded, not all-or-nothing.

## Config dependencies

- The exact set of `Severity::High` keys lives in `config_registry/datadog/unsupported.rs`
  (generated/maintained list). The workload needs at least one current High-severity key+non-default
  value to exercise the refusal path; this list can drift, so the workload should source the key
  dynamically or be pinned to the commit.
- Config arrives either from bootstrap YAML/env (run.rs:149 branch) or from the Core Agent dynamic
  config (run.rs:107-145 branch). `check_and_warn_config` runs on the **final** resolved `config`
  (run.rs:157) regardless of source, so an incompatible key delivered *over the config stream* is
  also gated. (Note: on the dynamic path the gate runs once at startup after `ready()`; a key that
  becomes incompatible via a later partial update is NOT re-checked — see Open Questions.)

## Assertion (MISSING — net-new instrumentation)

No Antithesis SDK assertions exist. Proposed SUT-side:
- In `check_and_warn_config`, when `high_severity_incompatibilities > 0`, before returning Err:
  `assert_reachable!("ADP refused to start: high-severity incompatible config")`.
- Set a process-local flag `saw_high_severity_incompat = true` in that branch; add
  `assert_unreachable!("pipeline spawned with high-severity incompatible config",
  saw_high_severity_incompat)` immediately after `built_topology.spawn(...)` at run.rs:239 (it
  should be statically unreachable because the `?` already returned, but the assertion makes the
  guarantee explicit and catches any future reordering regression).
- Alternatively / additionally, workload-side: `process_exits_with(1)` + assert no
  `topology_ready_ms` log + intake never receives data (mirrors existing integration "config-check
  exit codes" cases but under fault injection).

## SUT-side instrumentation needs

- Antithesis SDK dependency (none today).
- Reachable marker on the refusal branch; Unreachable marker after spawn keyed on a
  high-severity-seen flag.
- Workload must supply a current `Severity::High` key with a non-default value.

### Investigation Log

#### Are runtime partial config updates re-validated by the incompatibility gate? (2026-05-28)

**Examined:**
- `bin/agent-data-plane/src/cli/run.rs:157` (`check_and_warn_config` call site), `:331-381`
  (`check_and_warn_config` body), `:14` (the only import of `ConfigClassifier`/`Severity`/
  `SupportLevel`).
- `lib/saluki-config/src/lib.rs:541-651` (`run_dynamic_config_updater` — the task that applies all
  runtime `Snapshot`/`Partial` updates over the gRPC config stream).
- Grep across `lib/saluki-config/` and `bin/agent-data-plane/src/` for `ConfigClassifier`,
  `check_and_warn_config`, `classify(`.

**Found — gate is startup-only, NOT re-run at runtime:**
- `check_and_warn_config` constructs a fresh `ConfigClassifier::new()` (run.rs:333) and is invoked
  exactly once, at run.rs:157, before `create_topology`/`build`/`spawn`. The `?` returns the
  process before the pipeline is built (matches the existing "Key observation" section).
- The runtime updater `run_dynamic_config_updater` rebuilds the figment on every update
  (lib.rs:564-578 for the initial snapshot, lib.rs:621-649 for subsequent updates) and dispatches
  `ConfigChangeEvent`s via `dynamic::diff_config` (lib.rs:633-639), but it contains **no reference
  to `ConfigClassifier` or `check_and_warn_config`** and performs no support-level/severity check.
  A `Partial` update is applied via `upsert(&mut dynamic_state, &key, value)` (lib.rs:612) and the
  new figment is swapped in (lib.rs:645) — unconditionally.
- The grep confirms `ConfigClassifier` and `check_and_warn_config` appear ONLY in run.rs (the import
  at :14 and the call/definition at :157/:331). The saluki-config crate that owns the dynamic updater
  has zero awareness of the classifier.

**Not found:** No runtime re-validation hook, no severity check on `ConfigChangeEvent`, no path that
re-enters `check_and_warn_config` after startup, and no mechanism that would refuse/halt on a
runtime-delivered high-severity key. The classifier crate (`saluki-components::config_registry`) is
not a dependency of the dynamic-update path.

**Conclusion (RESOLVED, scope confirmed):** The incompatibility gate runs **only once at startup**.
A config key that flips to a high-severity-incompatible value via a later `Partial` (or `Snapshot`)
update over the gRPC stream is applied to the live figment and broadcast as a change event — ADP
**keeps running**; it does NOT refuse-to-start or shut down. The `config-incompatible-refuses-start`
property is therefore correctly scoped to **startup configuration only** (the `?` at run.rs:157
guards topology spawn against the *startup-resolved* config, including the first snapshot + env
overlays, per the existing notes). Runtime re-validation is a genuine GAP and warrants a separate
property/finding ("runtime config update can introduce a high-severity-incompatible key with no
re-gate") — not folded into this safety property. Recommend filing that gap; this file's Open
Questions are now resolved.
