# Adapt the local-report prototype to the final `smp report` CLI (ADR-007)

Status: DRAFT — exploring, open questions pending.

## Context

The branch `greg/smp-ci-reporting` (this checkout) prototypes locally-rendered SMP
reports in saluki CI (ADR-007). It was written against an **intermediate draft of
the `smp report render` CLI**:

- old CLI flags: `--report-json`, `--output-file`, `--target-config-dir`
- old flat render context: `optimization_goals`, `checks`, `experiments[name].report_links`,
  `job_id`, `baseline_sha`, `effect_size`, ... plus an `interpolate` filter

The final implementation lives in `../smp-smp-ci-reporting` (binary:
`target/debug/smp`, v0.29.0-alpha.0) and changed substantially:

- CLI: `smp report render --report <report.v1.json> <--template-file F | --builtin B> [--extra E]`
  - no `--output-file` (rendered text goes to **stdout**), no `--target-config-dir`
  - custom templates **require** first line `{#- smp-report-schema: 1 -#}`
- Context is namespaced: `report.job`, `report.experiments` (each with
  `optimization_goal`, `bounds_checks`, `quantile_checks`), `report.failed_replicates`,
  `report.analysis_errors`; `extra` only if `--extra` passed
- Rich helper set (minijinja filters/functions): `format_goal_label`,
  `format_percent`, `format_confidence_interval`, `pvalue_to_confidence`,
  `shas_are_shas`, `format_perf_symbol`, `check_format_*`,
  `build_unified_check_rows`, `bounds_checks_all_passed`,
  `group_failed_executions_by_categories`, `table`
- Strict undefined behavior
- Custom links are retired (ADR-007 D7): templates carry their own URLs
- `smp job sync` now downloads `report.v1.json` alongside the legacy `report.json`

Relevant sources:

| Item | Path |
|---|---|
| Final CLI group | `../smp-smp-ci-reporting/smp/src/bin/smp/cli/report_command.rs` |
| Render entry + template registry | `../smp-smp-ci-reporting/smp/src/bin/smp/report_render.rs` |
| v1 context + filters wiring | `../smp-smp-ci-reporting/smp/src/bin/smp/report_render/v1.rs` |
| Categorization filters | `../smp-smp-ci-reporting/smp/src/bin/smp/report_render/v1/derive.rs` |
| Format filters | `../smp-smp-ci-reporting/smp/src/bin/smp/report_render/v1/format.rs` |
| Built-in `report.md` template (reference for structure) | `../smp-smp-ci-reporting/smp/src/bin/smp/report_render/templates/report.md.j2` |
| v1 data contract | `../smp-smp-ci-reporting/libs/report/src/v1.rs` |
| Prototype wrapper script | `ci/tooling/build-smp-report.py` |
| Prototype condensed template | `ci/tooling/smp_condensed_report.md.j2` |
| Prototype CI wiring | `.gitlab/benchmark.yml` |
| Old->v1 heuristic converter | `~/dev/greg-stuff/smp-ih/2026-w40/report_v1_from_json.py` |
| Saluki PR report corpus (old-style `report.json` + server-rendered `report.md`) | `~/dev/greg-stuff/smp-experiments/report-parity-experiments/saluki-parity/` |

Prototype branch also has uncommitted scratch: a modified footer line in the
template (typo'd) and untracked `nightly-job/` job artifacts (old-style
`report.json` etc., useful as test fixtures).

Canonical reference for the condensed format: `git show 45c414a41d^:ci/tooling/build-smp-report.py`
(the pre-prototype Python implementation that produces today's PR comments).

### 1. Rewrite `ci/tooling/smp_condensed_report.md.j2` for the v1 context

Keep the rendered output as close as possible to today's condensed PR comment
(`build_report()` in the pre-prototype script), while structuring the template
like the built-in `report.md.j2` (front-matter `set` block, macros for links
and tables, CLI helpers).

- First line: `{#- smp-report-schema: 1 -#}` (required, version-checked by the CLI).
- Header from `report.job`: `**Run ID:** {{ report.job.id }}`; short
  baseline/comparison SHAs joined with `&middot;`; GitLab-relative diff link
  `[diff](../../compare/{{ base }}..{{ comp }})` guarded by the `shas_are_shas`
  function (matches current saluki output, unlike the built-in's non-relative
  link).
- Optimization-goals table `| experiment | goal | Δ mean % | links |`:
  - iterate `report.experiments | selectattr("optimization_goal.result")`;
  - goal label via a template-local token map reproducing the old `GOAL_INFO`
    short labels: `cpu -> cpu (down)`, `memory -> memory (down)`,
    `ingress_throughput -> throughput (up)`, `egress_throughput ->
    egress throughput (up)` — not the built-in's `format_goal_label`
    ("memory utilization");
  - `Δ mean %` cell: `{{ "%+.2f" | format(result.percent_change) }}` prefixed
    with the classification dot: green = `is_significant_change and
    is_improvement`, red = `is_significant_change and not is_improvement`,
    neutral otherwise. Configured-erratic experiments (`experiment.erratic`)
    are force-neutral and tagged `**(ignored)**`; runtime `result.is_erratic`
    gets `**(erratic)**`. This reproduces `classify_change()` on v1 flags (the
    heuristic maps `is_regression` -> `is_significant_change`, so Python's
    extra `|Δ| > effect_size` conjunction is subsumed);
  - sort worst-for-goal first, tie by name (old `sort_key`);
  - regressions in an up-front section, others in a collapsed `<details>`.
- Links cell: `metrics_url` / `profiles_url` / `logs_url` macros built from
  `report.job.id`, experiment name, and `report.job.metrics_query_start/end`
  with the 7200/3600s offsets. Copied verbatim from the pre-prototype script's
  `METRICS_URL_TEMPLATE` / `PROFILES_URL_TEMPLATE` / `LOGS_URL_TEMPLATE`.
- Bounds checks (collapsed): `report.experiments | build_unified_check_rows |
  rejectattr("is_quantile")`, cells via the CLI filters `check_format_name`,
  `check_format_replicates_passed`, `check_format_perf_symbol`,
  `check_format_observed_value`, plus the saluki links macros; overall verdict
  via `bounds_checks_all_passed`.
- Explanation section: same wording as today, with
  `report.job.tolerances.effect_size | format_percent` and
  `report.job.tolerances.p_value | pvalue_to_confidence | format_percent`.
- Strict-mode discipline: guard every optional-field access (`optimization_goal`,
  `result`) with `selectattr` / conditionals the way the built-in does.

> [!NOTE]
> Known deviation: the CLI's `check_format_observed_value` renders
> `231.50MiB ≤ 250MiB` (compact) where the old Python script rendered
> `232 MiB ≤ 250 MiB` (`%.3g`, spaced). Plan reuses the CLI filter (supported
> path, matches the full server-rendered report). Flag if exact old formatting
> is preferred.

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

- `SMP_VERSION` -> `dev-pr4729-e720c69e2` (provided by Gregoire).
- `report-benchmarks-adp` / `report-benchmarks-adp-full`: pass
  `--report-v1-json outputs/report.v1.json`.
- S3 download path already uses un-prefixed `${SMP_VERSION}`, no change.

## Files to modify

- `ci/tooling/smp_condensed_report.md.j2` — full rewrite to v1 context
- `ci/tooling/build-smp-report.py` — new CLI flags, stdout capture, fixed failure path
- `.gitlab/benchmark.yml` — SMP_VERSION pin + wrapper args

Uncommitted scratch on the branch: the modified footer line in the template is
superseded by this rewrite; `nightly-job/` stays untracked as local fixture data.

## Steps

- [ ] Rewrite `smp_condensed_report.md.j2` (v1 context, schema header, saluki link macros)
- [ ] Update `build-smp-report.py` (flags, stdout capture, fixed failure placeholder)
- [ ] Update `.gitlab/benchmark.yml` once SMP_VERSION is provided
- [ ] Fixture loop: convert every parity-corpus `report.json` with the
      heuristic, render with the debug binary, eyeball each output against the
      corpus `report.md` and the reference condensed format
- [ ] Test failure paths: missing `report.v1.json`, malformed JSON, strict-mode
      template error (undefined attribute)

## Verification

- For each parity-corpus job: heuristic-convert, render with
  `target/debug/smp report render --template-file ci/tooling/smp_condensed_report.md.j2 --report ...`,
  diff against the expected condensed format (Gregoire enriches the harness
  with reference outputs)
- Exercise the failed-bounds path with the `fail_bounds` jobs from `manifest.tsv`
- Wrapper end-to-end: `python3 ci/tooling/build-smp-report.py --smp-binary <debug smp>
  --report-v1-json <fixture> --output-report /tmp/out.md`; with a bogus input it
  writes the placeholder instead of crashing
- No Rust changes, so no `cargo check`; sanity-check the Python wrapper only

## Open items

- SMP_VERSION string to pin in `.gitlab/benchmark.yml` (Gregoire provides)
- Keep or drop the prototype-only template additions (warning lines,
  "locally generated" footer) — answer pending
- Bounds observed-value formatting: CLI `check_format_observed_value`
  (recommended) vs exact old-Python `%.3g MiB` formatting
