# Adapt the local-report prototype to the final `smp report` CLI (ADR-007)

Revision 2 — updated after the parity harness was enriched with synthetic
`report.v1.json` fixtures and real PR-comment reference outputs. Supersedes the
`PLAN.md` root draft (scratch, left in place).

## Context

The branch `greg/smp-ci-reporting` prototypes locally-rendered SMP reports in
saluki CI (ADR-007). It was written against an **intermediate draft of the
`smp report render` CLI** and drifted from the final implementation in
`../smp-smp-ci-reporting`:

| Aspect | Prototype (old draft CLI) | Final CLI (`target/debug/smp`, v0.29.0-alpha.0) |
|---|---|---|
| Invocation | `--report-json --output-file --target-config-dir --template-file` | `smp report render --report <report.v1.json> --template-file F [--extra E]` |
| Output | written to `--output-file` | rendered text on **stdout** (banner goes to stderr) |
| Context | flat: `optimization_goals`, `checks`, `experiments[name].report_links`, `job_id`, `baseline_sha`, ... | namespaced under `report.` (`report.job`, `report.experiments[].optimization_goal`, ...) |
| Links | `interpolate` filter + `report_links` from experiment.yaml via `--target-config-dir` | none in schema; templates build their own URLs (ADR-007 D7) |
| Template header | none required | first line MUST be `{#- smp-report-schema: 1 -#}` |
| Undefined vars | lax | strict (render fails) |
| Input schema | legacy `report.json` | versioned `report.v1.json` (checked via `$schema`) |

Decisions taken with Gregoire:

- **Output**: keep the **condensed PR-comment format** saluki produces today.
  Port it to the v1 context; structure the template after the final built-in
  `report.md.j2`.
- **Links**: hardcode saluki's URL macros in the template, copied verbatim from
  the deployed script's URL constants (metrics dashboard `4br-nxz-khi` with
  `adp-run-id` template var, profiles scoped `service:agent-data-plane`, logs
  with `run_id`; observation-window offsets 7200s/3600s).
- **Input**: `report.v1.json` only. No conversion in the wrapper.
- **CI**: update `.gitlab/benchmark.yml`; pin `SMP_VERSION=dev-pr4729-e720c69e2`.
- **Prototype-only additions**: keep the ⚠ warning lines, drop the
  "locally generated" footer.
- **Bounds observed-value formatting**: reuse the CLI's
  `check_format_observed_value` (accepted deviation, detailed below).

## Canonical reference

The deployed condensed renderer is **saluki `origin/main`'s
`ci/tooling/build-smp-report.py`**, last touched by `dcd681d7ae` ("gracefully
support missing analyses in SMP runs", Aug 13 evening). The prototype branch is
based on `0dee0a962b` (Aug 13 morning) and predates that fix — so the
`(no analysis)` / `⚠️ n/a` support visible in real PR comments is **not** in
this branch's history. The harness's `report.md` files (actual PR comments)
are the ground-truth reference output.

## Harness (enriched, verified)

`~/dev/greg-stuff/smp-experiments/report-parity-experiments/saluki-parity/`
now holds 20 jobs (`manifest.tsv`: pass / fail_bounds / improvement), each with:

- `report.json` — legacy server report
- `report.v1.json` — synthetic v1; verified byte-identical to
  `report_v1_from_json.py` output (spot-checked 2 jobs); all 20 render cleanly
  with the debug binary (`smp report render --builtin junit.xml`)
- `report.md` — the **actual GitHub PR comment** (synced from cached comments
  by `sync_report_md_from_comments.sh`)

Verified corpus coverage:

- every job: 5 experiments, all `memory` goal, one `memory_usage` bounds check
- 10 jobs with failed bounds, 10 passed; exactly one 🟢 improvement row
  (`ffdb2df2`, `quality_gates_rss_dsd_ultraheavy`, -6.08); **no 🔴 regression
  row in any job**; all headlines are "No significant changes detected"

> [!WARNING]
> Coverage gaps — these template sections can't be diffed against reference
> outputs, only smoke-tested for crash-freedom: the up-front regressions
> section, the `(no analysis)` / `⚠️ n/a` row rendering and its headline
> suffix, the ⚠ warning block, cpu/throughput goal labels, and
> `(ignored)`/`(erratic)` tags.

Flaws in the synthetic `report.v1.json` (heuristic KNOWN_GAPS; benign for
saluki, flagged as requested):

- `bounds_checks[].series` is a **placeholder** equal to the check name —
  happens to equal the true series here, but the template must not rely on
  `series`
- `unit` is guessed from the check name (correct here: `bytes` via override;
  could be wrong in general)
- `quantile_checks` and `analysis_errors` are always `[]` in fixtures, so the
  analysis-errors warning path can't be exercised with fixtures
- `trial_count` is derived as `trial_index + 1`

## Reference implementations

| Item | Path |
|---|---|
| Final CLI group (flags, stdout behavior) | `../smp-smp-ci-reporting/smp/src/bin/smp/cli/report_command.rs` |
| Render entry, schema-directive check, strict env | `../smp-smp-ci-reporting/smp/src/bin/smp/report_render.rs` |
| v1 context + filter wiring | `../smp-smp-ci-reporting/smp/src/bin/smp/report_render/v1.rs` |
| Filters: `build_unified_check_rows`, `bounds_checks_all_passed`, `group_failed_executions_by_categories` | `../smp-smp-ci-reporting/smp/src/bin/smp/report_render/v1/derive.rs` |
| Filters: `format_goal_label`, `format_percent`, `format_perf_symbol`, `check_format_*`, `shas_are_shas` | `../smp-smp-ci-reporting/smp/src/bin/smp/report_render/v1/format.rs` |
| Built-in `report.md.j2` (structural reference) | `../smp-smp-ci-reporting/smp/src/bin/smp/report_render/templates/report.md.j2` |
| v1 data contract (`Report`, `Job`, `Experiment`, `OptimizationGoalResult`, `BoundsCheck`) | `../smp-smp-ci-reporting/libs/report/src/v1.rs` |
| **Canonical condensed renderer** (deployed Python) | `origin/main:ci/tooling/build-smp-report.py` (commit `dcd681d7ae`) |
| Old->v1 heuristic (fixture provenance; fallback) | `~/dev/greg-stuff/smp-ih/2026-w40/report_v1_from_json.py` |
| Parity corpus + reference PR comments | `~/dev/greg-stuff/smp-experiments/report-parity-experiments/saluki-parity/` |

## Approach

### 1. Rewrite `ci/tooling/smp_condensed_report.md.j2` for the v1 context

Match the deployed `build_report()` output exactly (ground truth: the harness's
PR-comment `report.md` files), structured like the built-in `report.md.j2`.

- First line: `{#- smp-report-schema: 1 -#}` (required, version-checked by the CLI).
- No title: the `## Regression Detector (Agent Data Plane)` header comes from
  `pr-commenter --header`, not the report. The report starts at `**Run ID:**`.
- Header from `report.job`: `**Run ID:** {{ report.job.id }}`; short
  baseline/comparison SHAs joined with `&middot;`; GitLab-relative diff link
  `[diff](../../compare/{{ base }}..{{ comp }})` guarded by the `shas_are_shas`
  function.
- Headline: `## Optimization Goals: ❌ N regression(s) detected` or
  `✅ No significant changes detected`, plus the suffix
  ` &middot; ⚠️ N experiment(s) without analysis` when any experiment has an
  `optimization_goal` without a `result`.
- Optimization-goals table `| experiment | goal | Δ mean % | links |`:
  - iterate `report.experiments | selectattr("optimization_goal")`; experiments
    without `optimization_goal` at all are skipped from this table
    (deployed `partition_experiments` behavior);
  - goal label via a template-local token map reproducing `GOAL_INFO` short
    labels: `cpu -> cpu (down)`, `memory -> memory (down)`,
    `ingress_throughput -> throughput (up)`, `egress_throughput ->
    egress throughput (up)`;
  - `Δ mean %` cell: `{{ "%+.2f" | format(result.percent_change) }}` prefixed
    with the classification dot: 🟢 = `is_significant_change and
    is_improvement`, 🔴 = `is_significant_change and not is_improvement`, ⚪
    otherwise. Configured-erratic experiments (`experiment.erratic`) are
    force-neutral and tagged `**(ignored)**`; runtime `result.is_erratic` gets
    `**(erratic)**`;
  - **no-analysis rows**: experiment with `optimization_goal` but no `result`
    renders `| name **(no analysis)** | ? | ⚠️ n/a | links |` (deployed
    `dcd681d7ae` behavior);
  - sort worst-for-goal first, tie by name (verified against corpus comments:
    descending Δ for memory); regressions in an up-front section, everything
    else in a collapsed `<details>`.
- Links cell: `metrics_url` / `profiles_url` / `logs_url` macros built from
  `report.job.id`, experiment name, and `report.job.metrics_query_start/end`
  with the 7200/3600s offsets — copied verbatim from the deployed script's
  `METRICS_URL_TEMPLATE` / `PROFILES_URL_TEMPLATE` / `LOGS_URL_TEMPLATE`
  (byte-identical URLs to the corpus comments).
- Bounds checks (collapsed): `report.experiments | build_unified_check_rows |
  rejectattr("is_quantile")`, cells via `check_format_name`,
  `check_format_replicates_passed`, `check_format_observed_value` prefixed with
  `check_format_perf_symbol`, plus the saluki links macros; overall verdict via
  `bounds_checks_all_passed`; row order (experiment name, then check name)
  matches the corpus comments' alphabetical order.
- Explanation section: deployed wording, including the `(no analysis)` /
  `⚠️ n/a` sentence; effect size from
  `report.job.tolerances.effect_size | format_percent` (renders `**5.00%**`).
- Warning block (prototype addition, kept per decision): one ⚠ line each for
  experiments that lost all replicates / produced no goal data, retried
  replicates, and analysis errors (`report.analysis_errors`); nothing on a
  clean run. Note: the fixture corpus can't exercise it (see coverage gaps).
- Strict-mode discipline: guard every optional-field access with
  `selectattr` / conditionals the way the built-in does.

> [!NOTE]
> Accepted deviation (confirmed against corpus): bounds observed values render
> as `231.50MiB ≤ 250MiB` (CLI `check_format_observed_value`) where the
> reference PR comments show `232 MiB ≤ 250 MiB` (old `%.3g` formatting).
> Same underlying value (242749440 bytes), formatting only.

### 2. Update `ci/tooling/build-smp-report.py`

- Input flag: `--report-v1-json` (path to `report.v1.json`; `smp job sync` in
  the new CLI downloads it — `smp/src/bin/smp/cli/job_command/sync.rs`).
- Invocation: `<smp> report render --report <v1.json> --template-file
  ci/tooling/smp_condensed_report.md.j2`; keep `--smp-binary`.
- Capture stdout (rendered report), pass stderr through, write to
  `--output-report`.
- Fix the failure-placeholder path: current code reads `exc.stderr` from a
  `CalledProcessError` raised without `capture_output` (always `None`); use
  `capture_output=True` and include real stderr in the placeholder.
- Drop the stray `print(smp_binary.stat())` debug line.

### 3. Update `.gitlab/benchmark.yml`

- `SMP_VERSION=dev-pr4729-e720c69e2` (provided by Gregoire).
- `report-benchmarks-adp` / `report-benchmarks-adp-full`: pass
  `--report-v1-json outputs/report.v1.json`.
- S3 download path already uses un-prefixed `${SMP_VERSION}`, no change.

## Files to modify

- `ci/tooling/smp_condensed_report.md.j2` — full rewrite to v1 context
- `ci/tooling/build-smp-report.py` — new CLI flags, stdout capture, fixed failure path
- `.gitlab/benchmark.yml` — SMP_VERSION pin + wrapper args

Uncommitted scratch on the branch: the modified footer line in the template is
superseded by this rewrite; `nightly-job/` stays untracked as local fixture
data.

## Steps

- [x] Rewrite `smp_condensed_report.md.j2` (v1 context, schema header, saluki
      link macros, no-analysis rows, warning block kept, footer dropped)
- [x] Update `build-smp-report.py` (flags, stdout capture, fixed failure placeholder)
- [x] Update `.gitlab/benchmark.yml` (SMP_VERSION -> `dev-pr4729-e720c69e2`,
      wrapper args)
- [x] Fixture loop over all 20 corpus jobs: render the harness's
      `report.v1.json` with the debug binary, diff against the harness's
      `report.md` (expected diffs: bounds observed-value formatting only)
- [x] Test failure paths: missing `report.v1.json`, malformed JSON, strict-mode
      template error (undefined attribute)

## Verification

- For each of the 20 corpus jobs:
  `target/debug/smp report render --template-file ci/tooling/smp_condensed_report.md.j2 --report <corpus>/report.v1.json`
  diffed against `<corpus>/report.md` — the only accepted difference is the
  bounds observed-value formatting noted above; links, ordering, dots, and
  wording must match byte-for-byte.
- Sections not covered by the corpus (regressions section, no-analysis rows,
  warnings, erratic tags, cpu/throughput labels) get hand-built v1 fixtures to
  smoke-test rendering without crashing; content reviewed by eye against the
  deployed Python logic.
- Wrapper end-to-end: `python3 ci/tooling/build-smp-report.py --smp-binary <debug smp>
  --report-v1-json <fixture> --output-report /tmp/out.md`; with a bogus input it
  writes the placeholder instead of crashing.
- No Rust changes, so no `cargo check`; sanity-check the Python wrapper only.
