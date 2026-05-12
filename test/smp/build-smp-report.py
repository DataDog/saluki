#!/usr/bin/env python3
"""
Generate a condensed Markdown benchmark report from SMP's report.json.

Reads the report.json produced by SMP's `job sync` command and emits a
Markdown report suitable for posting as a PR comment.

This is a more compact alternative to SMP's canonical report.md:
- Drops the `Δ mean % CI` and `trials` columns.
- Merges the `perf` indicator into the `Δ mean %` cell using emoji colored
  by direction relative to each experiment's optimization goal: 🟢 = past
  the effect-size threshold in the improving direction, 🔴 = past it in the
  regressing direction, ⚪ = within the threshold. The threshold is read
  from `job_info.effect_size` in the report.

Usage:
    python3 build-smp-report.py \\
        --report-json outputs/report.json \\
        --output-report outputs/condensed-report.md

If --report-json points to a missing file (because SMP failed before it could
produce one), a "benchmarks did not produce a report" placeholder is written
so the dependent reporting job still posts something useful to the PR.
"""

import argparse
import json
import sys
import urllib.parse
from pathlib import Path

# URL templates for the per-experiment "report links". These mirror the
# `report_links` configuration in test/smp/regression/adp/experiments.yaml.
METRICS_URL_TEMPLATE = (
    "https://app.datadoghq.com/dashboard/4br-nxz-khi"
    "?fromUser=true&refresh_mode=paused"
    "&tpl_var_adp-run-id%5B0%5D={job_id}"
    "&tpl_var_experiment%5B0%5D={experiment}"
    "&view=spans&from_ts={start_time_ms}&to_ts={end_time_ms}&live=false"
)
PROFILES_URL_TEMPLATE = (
    "https://app.datadoghq.com/profiling/explorer"
    "?query=env%3Asingle-machine-performance%20service%3Aagent-data-plane"
    "%20job_id%3A{job_id}%20experiment%3A{experiment}"
    "&agg_m=count&agg_m_source=base&agg_t=count&fromUser=false&viz=stream"
    "&start={filter_start_ms}&end={filter_end_ms}&paused=true"
)
LOGS_URL_TEMPLATE = (
    "https://app.datadoghq.com/logs"
    "?query=experiment%3A{experiment}%20run_id%3A{job_id}"
    "&agg_m=count&agg_m_source=base&agg_q=%40span.url&agg_q_source=base"
    "&agg_t=count&fromUser=true"
    "&index=single-machine-performance-target-logs"
    "&messageDisplay=inline&refresh_mode=paused&storage=hot"
    "&stream_sort=time%2Cdesc&top_n=100&top_o=top&viz=stream&x_missing=true"
    "&from_ts={filter_start_ms}&to_ts={filter_end_ms}&live=false"
)

# SMP wraps the metrics query window with a wider observation window for the
# profiling and logs links. These offsets match SMP's defaults.
FILTER_START_OFFSET_SECONDS = 7200  # 2 hours before metrics_query_start
FILTER_END_OFFSET_SECONDS = 3600  # 1 hour after metrics_query_end

# Map SMP's optimization_goal value to (human label, direction-of-improvement).
# "down" means a negative percent_change is the improving direction; "up"
# means a positive percent_change is the improving direction.
GOAL_INFO = {
    "Cpu": ("cpu", "down"),
    "Memory": ("memory", "down"),
    "IngressThroughput": ("throughput", "up"),
}


def classify_change(
    goal_data: dict,
    effect_size: float | None,
) -> str:
    """Classify a change as `improvement`, `regression`, or `neutral`.

    Experiments configured `erratic: true` (`reported_erratic`) are ignored
    outright and always classify as neutral. For the rest, a regression /
    improvement requires *both*:
    - `|percent_change| > effect_size` in the right direction for the
      optimization goal (a reduction in CPU/memory is an improvement; a
      reduction in ingress throughput is a regression), and
    - SMP's matching significance flag (`is_regression` / `is_improvement`).

    Note that `is_erratic` (runtime-detected) is informational about sample
    dispersion only; SMP already takes it into account when setting
    `is_regression` / `is_improvement`, so we don't gate on it separately.
    """
    if goal_data.get("reported_erratic"):
        return "neutral"
    if effect_size is None:
        return "neutral"
    percent_change = goal_data.get("percent_change") or 0.0
    if abs(percent_change) <= effect_size:
        return "neutral"
    optimization_goal = goal_data.get("optimization_goal")
    _label, improving = GOAL_INFO.get(optimization_goal, ("?", "down"))
    is_good = (improving == "down" and percent_change < 0) or (
        improving == "up" and percent_change > 0
    )
    if is_good and goal_data.get("is_improvement"):
        return "improvement"
    if not is_good and goal_data.get("is_regression"):
        return "regression"
    return "neutral"


def change_emoji(classification: str) -> str:
    """Map a `classify_change` result to a colored dot for the Δ mean % cell."""
    return {"improvement": "🟢", "regression": "🔴"}.get(classification, "⚪")


def format_percent(percent_change: float) -> str:
    """Format a percent change as e.g. `+1.50` or `-0.03`."""
    return f"{percent_change:+.2f}"


def goal_label(optimization_goal: str) -> str:
    """Return the human-readable label for an optimization goal."""
    label, _improving = GOAL_INFO.get(optimization_goal, (optimization_goal, "down"))
    return label


def build_links(experiment: str, job_id: str, time_range: dict) -> str:
    """Build the metrics / profiles / logs links cell for an experiment."""
    metrics_query_start = int(time_range["metrics_query_start"])
    metrics_query_end = int(time_range["metrics_query_end"])
    start_time_ms = metrics_query_start * 1000
    end_time_ms = metrics_query_end * 1000
    filter_start_ms = (metrics_query_start - FILTER_START_OFFSET_SECONDS) * 1000
    filter_end_ms = (metrics_query_end + FILTER_END_OFFSET_SECONDS) * 1000

    params = {
        "job_id": urllib.parse.quote(job_id, safe=""),
        "experiment": urllib.parse.quote(experiment, safe=""),
        "start_time_ms": start_time_ms,
        "end_time_ms": end_time_ms,
        "filter_start_ms": filter_start_ms,
        "filter_end_ms": filter_end_ms,
    }
    metrics_url = METRICS_URL_TEMPLATE.format(**params)
    profiles_url = PROFILES_URL_TEMPLATE.format(**params)
    logs_url = LOGS_URL_TEMPLATE.format(**params)
    return f"[metrics]({metrics_url}) [profiles]({profiles_url}) [logs]({logs_url})"


def render_experiment_row(
    name: str,
    experiment: dict,
    job_id: str,
    time_range: dict,
    effect_size: float | None,
) -> str:
    """Render one Markdown table row for an experiment's optimization-goal
    analysis."""
    goal_data = experiment["optimization_goal"]
    percent_change = goal_data["percent_change"]
    optimization_goal = goal_data["optimization_goal"]

    classification = classify_change(goal_data, effect_size)
    pct_cell = f"{change_emoji(classification)} {format_percent(percent_change)}"
    links_cell = build_links(name, job_id, time_range)
    display_name = name
    if goal_data.get("reported_erratic"):
        display_name = f"{name} **(ignored)**"
    elif goal_data.get("is_erratic"):
        display_name = f"{name} **(erratic)**"
    return (
        f"| {display_name} | {goal_label(optimization_goal)} | {pct_cell} | "
        f"{links_cell} |"
    )


def render_experiment_table(
    entries: list[tuple[str, dict]],
    job_id: str,
    time_range: dict,
    effect_size: float | None,
) -> str:
    """Render a Markdown table of experiments."""
    lines = [
        "| experiment | goal | Δ mean % | links |",
        "|---|---|---|---|",
    ]
    for name, experiment in entries:
        lines.append(
            render_experiment_row(name, experiment, job_id, time_range, effect_size)
        )
    return "\n".join(lines)


def format_bytes(value: float) -> str:
    """Format a byte value as MiB with up to three significant figures.

    `%g` rounds to 3 sig figs and drops trailing zeros, so `140.00 MiB` renders
    as `140 MiB`, `122.99 MiB` as `123 MiB`, and `39.79 MiB` as `39.8 MiB`.
    """
    return f"{value / (1024 * 1024):.3g} MiB"


def render_bounds_checks(
    experiments: dict,
    job_id: str,
    time_range: dict,
) -> tuple[str, bool, int]:
    """Render the bounds checks section.

    Returns a tuple of (markdown, all_passed, total_checks).
    """
    rows = []
    all_passed = True
    total = 0

    for name in sorted(experiments.keys()):
        bounds_checks = experiments[name].get("bounds_checks") or {}
        for check_name, check in bounds_checks.items():
            total += 1
            results = check.get("results", {}).get("comparison", [])
            passed_count = sum(1 for r in results if r.get("passed"))
            total_count = len(results)
            max_observed = max(
                (r.get("max_observed", 0.0) for r in results), default=0.0
            )
            upper_bound = check.get("upper_bound")
            check_passed = passed_count == total_count
            if not check_passed:
                all_passed = False
            status = "✅" if check_passed else "❌"
            observed_value = (
                f"{format_bytes(max_observed)} ≤ {format_bytes(upper_bound)}"
                if upper_bound is not None
                else format_bytes(max_observed)
            )
            observed = f"{status} {observed_value}"
            links = build_links(name, job_id, time_range)
            rows.append(
                f"| {name} | {check_name} | {passed_count}/{total_count} "
                f"| {observed} | {links} |"
            )

    if total == 0:
        return "", True, 0

    table = "\n".join(
        [
            "| experiment | check | replicates | observed | links |",
            "|---|---|---|---|---|",
            *rows,
        ]
    )
    return table, all_passed, total


def short_sha(sha: str) -> str:
    return sha[:8] if sha else "(unknown)"


def diff_link(baseline_sha: str, comparison_sha: str) -> str:
    if not baseline_sha or not comparison_sha:
        return "(diff unavailable)"
    return (
        f"[Diff](https://github.com/DataDog/saluki/compare/"
        f"{baseline_sha}...{comparison_sha})"
    )


def partition_experiments(
    experiments: dict,
    effect_size: float | None,
) -> tuple[list, list]:
    """Group experiments into (regressions, others).

    An experiment is a regression when `|percent_change| > effect_size` in the
    regressing direction for its optimization goal AND it is not configured
    `erratic: true`. Erratic experiments always fall into `others` and get a
    `(erratic/ignored)` suffix on render. Both lists are sorted with the worst
    direction (for each experiment's goal) first; ties break by name.
    """
    regressions, others = [], []
    for name, experiment in experiments.items():
        goal_data = experiment.get("optimization_goal") or {}
        reported_erratic = goal_data.get("reported_erratic", False)
        classification = classify_change(goal_data, effect_size)
        if classification == "regression" and not reported_erratic:
            regressions.append((name, experiment))
        else:
            others.append((name, experiment))

    def sort_key(entry: tuple[str, dict]) -> tuple:
        name, experiment = entry
        goal_data = experiment.get("optimization_goal") or {}
        percent_change = goal_data.get("percent_change") or 0.0
        optimization_goal = goal_data.get("optimization_goal")
        # Order so the worst direction (for the goal) sorts first; ties break by name.
        _label, improving = GOAL_INFO.get(optimization_goal, ("?", "down"))
        signed = percent_change if improving == "down" else -percent_change
        return (-signed, name)

    return (
        sorted(regressions, key=sort_key),
        sorted(others, key=sort_key),
    )


def build_report(report: dict) -> str:
    """Build the condensed Markdown report from a parsed report.json."""
    job_id = report.get("job_id", "(unknown)")
    job_info = report.get("job_info") or {}
    baseline_sha = job_info.get("baseline_sha", "")
    comparison_sha = job_info.get("comparison_sha", "")
    effect_size = job_info.get("effect_size")
    time_range = report.get("time_range") or {
        "metrics_query_start": 0,
        "metrics_query_end": 0,
    }
    experiments = report.get("experiments") or {}

    regressions, others = partition_experiments(experiments, effect_size)

    if regressions:
        headline = (
            f"## Optimization Goals: ❌ {len(regressions)} regression"
            f"{'s' if len(regressions) != 1 else ''} detected"
        )
    else:
        headline = "## Optimization Goals: ✅ No significant changes detected"

    sections = [
        f"**Run ID:** `{job_id}`",
        f"**Baseline:** `{short_sha(baseline_sha)}` &middot; "
        f"**Comparison:** `{short_sha(comparison_sha)}` &middot; "
        f"{diff_link(baseline_sha, comparison_sha)}",
        "",
        headline,
        "",
    ]

    if regressions:
        sections += [
            render_experiment_table(regressions, job_id, time_range, effect_size),
            "",
        ]

    if others:
        sections += [
            "<details><summary><b>Fine details of change detection per "
            f"experiment</b> ({len(others)})</summary>",
            "",
            "Experiments configured `erratic: true` are tagged "
            "<code>(ignored)</code> and skipped when determining which "
            "experiments regressed or improved. Experiments which are "
            "_detected_ as erratic at runtime are tagged <code>(erratic)</code> "
            "to flag that the run's sample dispersion was high, but their "
            "regression / improvement signal still counts.",
            "",
            render_experiment_table(others, job_id, time_range, effect_size),
            "",
            "</details>",
            "",
        ]

    bounds_table, bounds_passed, bounds_total = render_bounds_checks(
        experiments, job_id, time_range
    )
    if bounds_total > 0:
        bounds_status = "✅ Passed" if bounds_passed else "❌ Failed"
        sections += [
            f"<details><summary><b>Bounds Checks:</b> {bounds_status} "
            f"({bounds_total})</summary>",
            "",
            bounds_table,
            "",
            "</details>",
            "",
        ]

    threshold_clause = (
        f"|Δ mean %| > **{effect_size:.2f}%**"
        if effect_size is not None
        else "the change exceeds the configured effect-size threshold"
    )
    explanation = (
        f"A change is flagged as a regression when {threshold_clause} in the "
        "regressing direction for its optimization goal AND SMP marks the experiment "
        "as a regression (`is_regression: true`). Improvements use the matching "
        "criteria for the improving direction. Experiments configured `erratic: true` "
        "(tagged <code>(ignored)</code>) are skipped outright; experiments detected "
        "as erratic at runtime (tagged <code>(erratic)</code>) still count, since "
        "that flag describes sample dispersion rather than directional certainty. "
        "The Δ mean % cell is colored accordingly: 🟢 = improvement, 🔴 = regression, "
        "⚪ = neutral. Reduction in CPU or memory is an improvement; reduction in "
        "ingress throughput is a regression."
    )
    sections += [
        "<details><summary><b>Explanation</b></summary>",
        "",
        explanation,
        "",
        "</details>",
    ]

    return "\n".join(sections).rstrip() + "\n"


def build_failure_report(reason: str) -> str:
    """Build a placeholder report for when report.json is missing or invalid."""
    return (
        "## Optimization Goals: ⚠️ Report unavailable\n\n"
        f"The benchmark run did not produce a usable report: {reason}\n\n"
        "Check the `run-benchmarks-adp` job logs for details.\n"
    )


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Generate a condensed Markdown SMP benchmark report.",
    )
    parser.add_argument(
        "--report-json",
        type=Path,
        required=True,
        help="Path to SMP report.json produced by `smp job sync`.",
    )
    parser.add_argument(
        "--output-report",
        type=Path,
        required=True,
        help="Path to write the generated Markdown report to.",
    )
    args = parser.parse_args()

    if not args.report_json.exists():
        args.output_report.write_text(
            build_failure_report(f"`{args.report_json}` is missing")
        )
        return 0

    try:
        with args.report_json.open() as handle:
            report = json.load(handle)
    except (OSError, json.JSONDecodeError) as exc:
        args.output_report.write_text(
            build_failure_report(f"could not parse `{args.report_json}`: {exc}")
        )
        return 0

    args.output_report.write_text(build_report(report))
    return 0


if __name__ == "__main__":
    sys.exit(main())
