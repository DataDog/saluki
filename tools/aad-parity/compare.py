#!/usr/bin/env python3
"""Go/Rust parity comparator for the anomaly-detection testbench.

Compares a Go headless observer artifact (``output.go`` schema) plus its
diagnostic scorer score-state dump against a Rust headless artifact
(``export.rs`` schema, which carries an additive ``score_timeline``).

The comparison follows README section 8.2:

* reject incompatible scenario / config / replay-mode inputs before comparing;
* match the score curve by exact data-time second (no intersection filling);
* report per-second EWMA MAE / RMSE / P95 / max absolute error and the longest
  sustained divergence, for the whole timeline and for the active window;
* report raw and delivered severity agreement;
* compare the pre-pipeline detector ledger and the correlation-episode set;
* walk the first-divergence ladder: input counts -> detector ledger -> score tick;
* exit non-zero when a gated case fails.

Modes
-----
``metricsonly`` is the deterministic gate: emissions (detector ledger),
severity, per-second counts/bins, and score ticks must match exactly, with the
score tick tolerance of ``1e-10`` absolute / ``1e-8`` relative. ``default`` is
the non-gated context run (all five detectors); it applies the README's
mixed-curve guardrails and reports, but does not gate, a detector-ledger
mismatch.

Usage
-----
    compare.py --root /tmp/aad-parity --out-json summary.json --out-md summary.md
    compare.py --self-test

The Go score-state artifact is located next to the Go observer artifact as
``<go>.scorestate.json`` (both are produced by the instrumented Go binary).
"""

from __future__ import annotations

import argparse
import json
import math
import os
import sys
from dataclasses import dataclass, field
from typing import Any

ABS_TOL = 1e-10
REL_TOL = 1e-8

# README section 8.3 mixed/log curve guardrails, used for the non-gated context runs.
CONTEXT_MAE_MAX = 0.002
CONTEXT_P95_MAX = 0.01
CONTEXT_MAX_MAX = 0.03
CONTEXT_SEVERITY_MIN = 99.0

LEVELS = ["very_low", "low", "medium", "high", "x_high"]


def ewma_tolerance(a: float, b: float) -> float:
    """Absolute tolerance allowed for a score-tick EWMA pair (abs + rel)."""
    return ABS_TOL + REL_TOL * max(abs(a), abs(b))


def percentile(values: list[float], pct: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    rank = (len(ordered) - 1) * pct
    low = math.floor(rank)
    high = math.ceil(rank)
    if low == high:
        return ordered[low]
    return ordered[low] + (ordered[high] - ordered[low]) * (rank - low)


@dataclass
class Divergence:
    """The first place two artifacts diverge, plus human-readable evidence."""

    layer: str
    detail: str

    def to_json(self) -> dict[str, str]:
        return {"layer": self.layer, "detail": self.detail}


@dataclass
class CaseResult:
    scenario: str
    mode: str
    ok: bool
    verdict: str
    errors: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)
    findings: list[str] = field(default_factory=list)
    first_divergence: Divergence | None = None
    metrics: dict[str, Any] = field(default_factory=dict)

    def to_json(self) -> dict[str, Any]:
        return {
            "scenario": self.scenario,
            "mode": self.mode,
            "verdict": self.verdict,
            "ok": self.ok,
            "errors": self.errors,
            "notes": self.notes,
            "findings": self.findings,
            "first_divergence": self.first_divergence.to_json() if self.first_divergence else None,
            "metrics": self.metrics,
        }


def load_json(path: str) -> Any:
    with open(path, "r", encoding="utf-8") as handle:
        return json.load(handle)


def as_set(values: list[Any]) -> set[Any]:
    return set(values or [])


def index_go_buckets(score_state: dict[str, Any]) -> dict[int, dict[str, Any]]:
    buckets = {}
    for bucket in score_state.get("buckets", []):
        buckets[int(bucket["second"])] = bucket
    return buckets


def index_rust_ticks(observer: dict[str, Any]) -> dict[int, dict[str, Any]]:
    ticks = {}
    for tick in observer.get("score_timeline", []):
        ticks[int(tick["second"])] = tick
    return ticks


def ledger_key(entry: dict[str, Any]) -> tuple[Any, ...]:
    return (entry.get("detector"), int(entry.get("timestamp", 0)), entry.get("source"), entry.get("title"))


def ledger_counts(entries: list[dict[str, Any]]) -> dict[tuple[Any, ...], int]:
    counts: dict[tuple[Any, ...], int] = {}
    for entry in entries or []:
        key = ledger_key(entry)
        counts[key] = counts.get(key, 0) + 1
    return counts


def multiset_diff(a: dict[Any, int], b: dict[Any, int]) -> tuple[int, int]:
    """Returns (a-only, b-only) total counts."""
    a_only = sum(max(0, count - b.get(key, 0)) for key, count in a.items())
    b_only = sum(max(0, count - a.get(key, 0)) for key, count in b.items())
    return a_only, b_only


def episode_key(period: dict[str, Any]) -> tuple[Any, ...]:
    return (period.get("pattern"), int(period.get("period_start", 0)), int(period.get("period_end", 0)))


def compare_case(
    scenario: str, mode: str, go_observer: dict[str, Any], go_score: dict[str, Any], rust_observer: dict[str, Any]
) -> CaseResult:
    """Compares one (scenario, mode) pair. `mode` is `metricsonly` (gated) or `default`."""
    gated = mode == "metricsonly"
    result = CaseResult(scenario=scenario, mode=mode, ok=True, verdict="PASS")

    go_meta = go_observer.get("metadata", {})
    rust_meta = rust_observer.get("metadata", {})

    # ---- 1. Reject incompatible inputs before computing any similarity ----
    if go_meta.get("scenario") != scenario:
        result.errors.append(f"go artifact scenario {go_meta.get('scenario')!r} != {scenario!r}")
    if rust_meta.get("scenario") != scenario:
        result.errors.append(f"rust artifact scenario {rust_meta.get('scenario')!r} != {scenario!r}")
    if as_set(go_meta.get("detectors_enabled")) != as_set(rust_meta.get("detectors_enabled")):
        result.errors.append(
            f"detectors_enabled differ: go={go_meta.get('detectors_enabled')} rust={rust_meta.get('detectors_enabled')}"
        )
    if as_set(go_meta.get("correlators_enabled")) != as_set(rust_meta.get("correlators_enabled")):
        result.errors.append(
            f"correlators_enabled differ: go={go_meta.get('correlators_enabled')} "
            f"rust={rust_meta.get('correlators_enabled')}"
        )
    rust_unlinked = rust_meta.get("unlinked_detectors") or []
    if rust_unlinked:
        result.errors.append(f"rust ran with unlinked detectors: {rust_unlinked}")

    # ---- 2. Input counts (the first rung of the divergence ladder) ----
    go_stats = go_meta.get("stats", {})
    rust_stats = rust_meta.get("stats", {})
    count_fields = [
        "input_metrics_count",
        "input_metrics_cardinality",
        "input_logs_count",
        "input_anomalies_count",
    ]
    stats_match = True
    for field_name in count_fields:
        if go_stats.get(field_name) == rust_stats.get(field_name):
            continue
        stats_match = False
        message = f"input {field_name} differ: go={go_stats.get(field_name)} rust={rust_stats.get(field_name)}"
        # Input normalization counts (metrics/cardinality/logs) are fatal in every mode. The anomaly
        # count is just the ledger total, so in a context run it is reported with the ledger finding
        # rather than double-counted as an independent failure.
        if field_name == "input_anomalies_count" and not gated:
            result.notes.append(message)
            continue
        result.errors.append(message)
        if result.first_divergence is None:
            result.first_divergence = Divergence("input_counts", message)
    result.metrics["input_counts"] = {
        "go": {k: go_stats.get(k) for k in count_fields},
        "rust": {k: rust_stats.get(k) for k in count_fields},
        "match": stats_match,
    }

    # ---- 3. Detector ledger ----
    go_ledger = ledger_counts(go_observer.get("detector_anomalies") or [])
    rust_ledger = ledger_counts(rust_observer.get("detector_anomalies") or [])
    go_only, rust_only = multiset_diff(go_ledger, rust_ledger)
    ledger_exact = go_ledger == rust_ledger
    by_detector: dict[str, dict[str, int]] = {}
    for key in set(go_ledger) | set(rust_ledger):
        detector = key[0]
        entry = by_detector.setdefault(detector, {"go_only": 0, "rust_only": 0})
        entry["go_only"] += max(0, go_ledger.get(key, 0) - rust_ledger.get(key, 0))
        entry["rust_only"] += max(0, rust_ledger.get(key, 0) - go_ledger.get(key, 0))
    result.metrics["detector_ledger"] = {
        "go_total": sum(go_ledger.values()),
        "rust_total": sum(rust_ledger.values()),
        "go_only": go_only,
        "rust_only": rust_only,
        "exact": ledger_exact,
        "by_detector": {k: v for k, v in sorted(by_detector.items()) if v["go_only"] or v["rust_only"]},
    }
    if not ledger_exact:
        message = (
            f"detector ledger differs: go_total={sum(go_ledger.values())} rust_total={sum(rust_ledger.values())} "
            f"go_only={go_only} rust_only={rust_only} by_detector={result.metrics['detector_ledger']['by_detector']}"
        )
        if result.first_divergence is None:
            result.first_divergence = Divergence("detector_ledger", message)
        if gated:
            result.errors.append(message)
            result.findings.append(message)
            if result.first_divergence is None:
                result.first_divergence = Divergence("detector_ledger", message)
        else:
            result.notes.append(message)
            result.findings.append(message)
            if result.first_divergence is None:
                result.first_divergence = Divergence("detector_ledger", message)

    # ---- 4. Score ticks, matched by exact second ----
    go_ticks = index_go_buckets(go_score)
    rust_ticks = index_rust_ticks(rust_observer)
    go_seconds = set(go_ticks)
    rust_seconds = set(rust_ticks)
    common = sorted(go_seconds & rust_seconds)
    coverage = {
        "go_ticks": len(go_seconds),
        "rust_ticks": len(rust_seconds),
        "common_ticks": len(common),
        "go_only_seconds": sorted(go_seconds - rust_seconds)[:10],
        "rust_only_seconds": sorted(rust_seconds - go_seconds)[:10],
        "match": go_seconds == rust_seconds,
    }
    result.metrics["timeline_coverage"] = coverage
    if go_seconds != rust_seconds:
        message = (
            f"score-timeline coverage differs: go={len(go_seconds)} rust={len(rust_seconds)} "
            f"go_only={len(go_seconds - rust_seconds)} rust_only={len(rust_seconds - go_seconds)}"
        )
        if result.first_divergence is None:
            result.first_divergence = Divergence("score_timeline_coverage", message)
        result.errors.append(message)

    abs_errors: list[float] = []
    squared_errors: list[float] = []
    active_abs_errors: list[float] = []
    count_mismatches = 0
    bins_mismatches = 0
    weight_mismatches = 0
    raw_severity_mismatches = 0
    delivered_severity_mismatches = 0
    delivered_compared = 0
    first_tick_divergence: int | None = None
    first_tick_detail = ""
    longest_run = 0
    current_run = 0

    for second in common:
        go_tick = go_ticks[second]
        rust_tick = rust_ticks[second]
        go_ewma = float(go_tick.get("ewma", 0.0))
        rust_ewma = float(rust_tick.get("ewma", 0.0))
        difference = abs(go_ewma - rust_ewma)
        abs_errors.append(difference)
        squared_errors.append(difference * difference)
        active = (go_tick.get("count", 0) or rust_tick.get("count", 0)) and (
            go_ewma > 0.0 or rust_ewma > 0.0
        )
        if active:
            active_abs_errors.append(difference)
        diverged = difference > ewma_tolerance(go_ewma, rust_ewma)
        if diverged:
            current_run += 1
            longest_run = max(longest_run, current_run)
        else:
            current_run = 0

        tick_problem = False
        if int(go_tick.get("count", 0)) != int(rust_tick.get("count", 0)):
            count_mismatches += 1
            tick_problem = True
        if list(go_tick.get("bins", [])) != list(rust_tick.get("bins", [])):
            bins_mismatches += 1
            tick_problem = True
        if abs(float(go_tick.get("weight_sum", 0.0)) - float(rust_tick.get("weight_sum", 0.0))) > 1e-9:
            weight_mismatches += 1
            tick_problem = True
        go_raw = go_tick.get("raw_level")
        rust_raw = rust_tick.get("raw_severity")
        if go_raw != rust_raw:
            raw_severity_mismatches += 1
            tick_problem = True
        go_delivered = go_tick.get("delivered_level")
        rust_delivered = rust_tick.get("delivered_severity")
        if go_delivered is not None or rust_delivered is not None:
            delivered_compared += 1
            if go_delivered != rust_delivered:
                delivered_severity_mismatches += 1
                tick_problem = True
        if tick_problem and first_tick_divergence is None:
            first_tick_divergence = second
            first_tick_detail = (
                f"second={second} go{{count={go_tick.get('count')}, bins={go_tick.get('bins')}, "
                f"ewma={go_ewma:.12g}, raw={go_raw}}} rust{{count={rust_tick.get('count')}, "
                f"bins={rust_tick.get('bins')}, ewma={rust_ewma:.12g}, raw={rust_raw}}}"
            )

    mae = sum(abs_errors) / len(abs_errors) if abs_errors else 0.0
    rmse = math.sqrt(sum(squared_errors) / len(squared_errors)) if squared_errors else 0.0
    p95 = percentile(abs_errors, 0.95)
    p99 = percentile(abs_errors, 0.99)
    max_error = max(abs_errors) if abs_errors else 0.0
    active_mae = sum(active_abs_errors) / len(active_abs_errors) if active_abs_errors else 0.0

    severity_compared = len(common)
    raw_agreement = 100.0 * (severity_compared - raw_severity_mismatches) / severity_compared if severity_compared else 100.0
    delivered_agreement = (
        100.0 * (delivered_compared - delivered_severity_mismatches) / delivered_compared if delivered_compared else None
    )

    result.metrics["score_curve"] = {
        "compared_seconds": len(common),
        "ewma_mae": mae,
        "ewma_rmse": rmse,
        "ewma_p95_abs_error": p95,
        "ewma_p99_abs_error": p99,
        "ewma_max_abs_error": max_error,
        "ewma_active_window_mae": active_mae,
        "active_window_seconds": len(active_abs_errors),
        "longest_sustained_divergence_seconds": longest_run,
        "count_mismatches": count_mismatches,
        "bins_mismatches": bins_mismatches,
        "weight_sum_mismatches": weight_mismatches,
        "raw_severity_agreement_pct": raw_agreement,
        "raw_severity_mismatches": raw_severity_mismatches,
        "delivered_severity_agreement_pct": delivered_agreement,
        "delivered_severity_mismatches": delivered_severity_mismatches,
        "first_divergent_second": first_tick_divergence,
        "first_divergent_detail": first_tick_detail,
    }

    # Gate the deterministic run on exact emissions/severity and the score tolerance.
    if gated:
        if count_mismatches or bins_mismatches or weight_mismatches:
            result.errors.append(
                f"score tick state differs: count={count_mismatches} bins={bins_mismatches} "
                f"weight_sum={weight_mismatches}"
            )
        if raw_severity_mismatches:
            result.errors.append(f"raw severity differs on {raw_severity_mismatches} seconds")
        if delivered_severity_mismatches:
            result.errors.append(f"delivered severity differs on {delivered_severity_mismatches} seconds")
        if max_error > max(ewma_tolerance(0.0, 1.0), 1e-9):
            result.errors.append(f"EWMA max abs error {max_error:.3e} exceeds the 1e-10 abs tolerance")
        if first_tick_divergence is not None and result.first_divergence is None:
            result.first_divergence = Divergence("score_tick", first_tick_detail)
    else:
        if mae > CONTEXT_MAE_MAX:
            result.errors.append(f"context EWMA MAE {mae:.3e} exceeds {CONTEXT_MAE_MAX}")
        if p95 > CONTEXT_P95_MAX:
            result.errors.append(f"context EWMA P95 {p95:.3e} exceeds {CONTEXT_P95_MAX}")
        if max_error > CONTEXT_MAX_MAX:
            result.errors.append(f"context EWMA max {max_error:.3e} exceeds {CONTEXT_MAX_MAX}")
        if raw_agreement < CONTEXT_SEVERITY_MIN:
            result.errors.append(f"context raw severity agreement {raw_agreement:.2f}% below {CONTEXT_SEVERITY_MIN}%")
        if first_tick_divergence is not None:
            result.notes.append(f"first differing score tick (context): {first_tick_detail}")

    # ---- 5. Correlation-episode set ----
    go_episodes = {episode_key(p) for p in go_observer.get("anomaly_periods") or []}
    rust_episodes = {episode_key(p) for p in rust_observer.get("anomaly_periods") or []}
    result.metrics["anomaly_periods"] = {
        "go_count": len(go_episodes),
        "rust_count": len(rust_episodes),
        "go_only": sorted(go_episodes - rust_episodes, key=str)[:10],
        "rust_only": sorted(rust_episodes - go_episodes, key=str)[:10],
        "exact": go_episodes == rust_episodes,
    }
    if go_episodes != rust_episodes:
        message = (
            f"anomaly-period sets differ: go={len(go_episodes)} rust={len(rust_episodes)} "
            f"go_only={len(go_episodes - rust_episodes)} rust_only={len(rust_episodes - go_episodes)}"
        )
        if gated:
            result.errors.append(message)
        else:
            result.notes.append(message)

    result.ok = not result.errors
    result.verdict = "PASS" if result.ok else "FAIL"
    return result


def discover_cases(root: str) -> list[tuple[str, str, str, str, str]]:
    """Returns (scenario, mode, go_path, go_score_path, rust_path) tuples."""
    go_dir = os.path.join(root, "go")
    rust_dir = os.path.join(root, "rust")
    cases = []
    if not os.path.isdir(go_dir) or not os.path.isdir(rust_dir):
        return cases
    for name in sorted(os.listdir(go_dir)):
        if not name.endswith(".json") or name.endswith(".scorestate.json"):
            continue
        stem = name[: -len(".json")]
        for mode in ("metricsonly", "default"):
            suffix = f"-{mode}"
            if not stem.endswith(suffix):
                continue
            scenario = stem[: -len(suffix)]
            rust_path = os.path.join(rust_dir, name)
            go_score = os.path.join(go_dir, f"{stem}-scorestate.json")
            if os.path.exists(rust_path) and os.path.exists(go_score):
                cases.append((scenario, mode, os.path.join(go_dir, name), go_score, rust_path))
    return cases


def build_report(cases: list[CaseResult], root: str) -> tuple[dict[str, Any], str]:
    total = len(cases)
    failed = [case for case in cases if not case.ok]
    findings = [f"{case.scenario}/{case.mode}: {finding}" for case in cases for finding in case.findings]
    summary = {
        "root": root,
        "cases": [case.to_json() for case in cases],
        "totals": {"cases": total, "passed": total - len(failed), "failed": len(failed), "findings": len(findings)},
        "findings": findings,
        "verdict": "PASS" if not failed else "FAIL",
    }

    lines = ["# Go/Rust parity comparison", ""]
    lines.append(f"Root: `{root}`")
    lines.append("")
    lines.append(f"**Overall verdict: {summary['verdict']}** ({summary['totals']['passed']}/{total} cases passed)")
    lines.append("")
    header = (
        "| scenario | mode | verdict | EWMA MAE | EWMA P95 | EWMA max | raw sev % | delivered sev % | ticks | "
        "ledger go/rust | ledger exact | input counts |"
    )
    lines.append(header)
    lines.append("|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for case in cases:
        curve = case.metrics.get("score_curve", {})
        ledger = case.metrics.get("detector_ledger", {})
        counts = case.metrics.get("input_counts", {})
        delivered = curve.get("delivered_severity_agreement_pct")
        lines.append(
            "| {scenario} | {mode} | {verdict} | {mae:.3e} | {p95:.3e} | {maxe:.3e} | {raw:.3f}% | {deliv} | "
            "{ticks} | {gt}/{rt} | {lexact} | {cmatch} |".format(
                scenario=case.scenario,
                mode=case.mode,
                verdict=case.verdict,
                mae=curve.get("ewma_mae", 0.0),
                p95=curve.get("ewma_p95_abs_error", 0.0),
                maxe=curve.get("ewma_max_abs_error", 0.0),
                raw=curve.get("raw_severity_agreement_pct", 0.0),
                deliv="n/a" if delivered is None else f"{delivered:.3f}%",
                ticks=curve.get("compared_seconds", 0),
                gt=ledger.get("go_total", 0),
                rt=ledger.get("rust_total", 0),
                lexact="yes" if ledger.get("exact") else "NO",
                cmatch="yes" if counts.get("match") else "NO",
            )
        )
    lines.append("")
    if summary["findings"]:
        lines.append("## Open findings")
        lines.append("")
        for finding in summary["findings"]:
            lines.append(f"- {finding}")
        lines.append("")
    lines.append("## First divergence and notes")
    for case in cases:
        lines.append("")
        lines.append(f"### {case.scenario} / {case.mode}")
        if case.first_divergence:
            lines.append(f"- First divergence: **{case.first_divergence.layer}** — {case.first_divergence.detail}")
        else:
            lines.append("- First divergence: none")
        for note in case.notes:
            lines.append(f"- Note: {note}")
        for error in case.errors:
            lines.append(f"- FAIL: {error}")
    lines.append("")
    lines.append("## Method")
    lines.append("")
    lines.append(
        "- Score ticks are matched by exact data-time second; missing seconds are reported, never filled with zero."
    )
    lines.append(
        "- EWMA error is `abs(go - rust)`; the gate is `1e-10` absolute plus `1e-8` relative per tick (README 8.3)."
    )
    lines.append(
        "- Detector ledger is a multiset over `(detector, timestamp, source, title)`, taken verbatim from both "
        "artifacts (never recomputed)."
    )
    lines.append(
        "- Severity agreement is computed from exported levels only: Go `raw_level`/`delivered_level`, Rust "
        "`raw_severity`/`delivered_severity`."
    )
    lines.append("- `metricsonly` is gated (deterministic exact match); `default` is the all-detector context run.")
    lines.append("")
    return summary, "\n".join(lines)


def run_self_test() -> int:
    """Exercises the comparator on tiny synthetic artifacts."""
    go_observer = {
        "metadata": {
            "scenario": "s",
            "detectors_enabled": ["bocpd"],
            "correlators_enabled": ["anomaly_scorer"],
            "stats": {
                "input_metrics_count": 10,
                "input_metrics_cardinality": 1,
                "input_logs_count": 0,
                "input_anomalies_count": 1,
            },
        },
        "detector_anomalies": [{"detector": "bocpd", "timestamp": 5, "source": "x", "title": "t"}],
        "anomaly_periods": [{"pattern": "p", "period_start": 5, "period_end": 5}],
    }
    rust_observer = {
        "metadata": {
            "scenario": "s",
            "detectors_enabled": ["bocpd"],
            "correlators_enabled": ["anomaly_scorer"],
            "stats": go_observer["metadata"]["stats"],
        },
        "detector_anomalies": [
            {"detector": "bocpd", "timestamp": 5, "source": "x", "title": "t", "score": None}
        ],
        "anomaly_periods": [{"pattern": "p", "period_start": 5, "period_end": 5}],
        "score_timeline": [
            {"second": 4, "bins": [0, 0, 0, 0, 0], "count": 0, "weight_sum": 0.0, "input": 0.0,
             "ewma": 0.0, "raw_severity": "low", "delivered_severity": "low"},
            {"second": 5, "bins": [0, 1, 0, 0, 0], "count": 1, "weight_sum": 1.0, "input": 0.18,
             "ewma": 0.00252, "raw_severity": "low", "delivered_severity": "low"},
        ],
    }
    go_score = {
        "buckets": [
            {"second": 4, "bins": [0, 0, 0, 0, 0], "count": 0, "weight_sum": 0.0, "input": 0.0,
             "ewma": 0.0, "raw_level": "low", "delivered_level": "low"},
            {"second": 5, "bins": [0, 1, 0, 0, 0], "count": 1, "weight_sum": 1.0, "input": 0.18,
             "ewma": 0.00252, "raw_level": "low", "delivered_level": "low"},
        ]
    }
    passing = compare_case("s", "metricsonly", go_observer, go_score, rust_observer)

    # A one-count divergence must fail the gate and report the first divergent second.
    broken_observer = json.loads(json.dumps(rust_observer))
    broken_observer["score_timeline"][1]["count"] = 2
    failing = compare_case("s", "metricsonly", go_observer, go_score, broken_observer)

    # A missing tick must be reported as a coverage mismatch, not silently filled.
    broken_coverage = json.loads(json.dumps(rust_observer))
    broken_coverage["score_timeline"] = broken_coverage["score_timeline"][:1]
    coverage = compare_case("s", "metricsonly", go_observer, go_score, broken_coverage)

    checks = [
        ("identical artifacts pass", passing.ok and passing.verdict == "PASS"),
        ("count divergence fails", not failing.ok and failing.first_divergence.layer == "score_tick"),
        ("coverage divergence fails", not coverage.ok),
    ]
    for name, ok in checks:
        print(f"[{'ok' if ok else 'FAIL'}] {name}")
    return 0 if all(ok for _, ok in checks) else 1


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default="/tmp/aad-parity")
    parser.add_argument("--out-json", default=None)
    parser.add_argument("--out-md", default=None)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--allow-empty", action="store_true", help="do not fail when no cases are discovered")
    args = parser.parse_args(argv)

    if args.self_test:
        return run_self_test()

    cases = discover_cases(args.root)
    if not cases and not args.allow_empty:
        print(f"compare.py: no comparable cases found under {args.root}", file=sys.stderr)
        return 2

    results = []
    for scenario, mode, go_path, go_score_path, rust_path in cases:
        results.append(
            compare_case(
                scenario,
                mode,
                load_json(go_path),
                load_json(go_score_path),
                load_json(rust_path),
            )
        )

    summary, report = build_report(results, args.root)
    if args.out_json:
        with open(args.out_json, "w", encoding="utf-8") as handle:
            json.dump(summary, handle, indent=2)
            handle.write("\n")
    if args.out_md:
        with open(args.out_md, "w", encoding="utf-8") as handle:
            handle.write(report)
            handle.write("\n")
    if not args.out_md:
        print(report)

    return 0 if summary["verdict"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
