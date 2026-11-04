#!/usr/bin/env python3
"""Summarizes the runs of `run-limits.sh`: per tier and CPU limit, the medians of each image, and plan against main.

Reads runs.csv and each run's per-second series (in the same directory), and prints a Markdown table per tier with,
for each CPU limit and label, the median over rounds of:

- send: lading's rate over the tier's target; below 0.97 ADP did not keep up, since a Unix datagram socket blocks the
  sender rather than drop (in saturate mode, the rate lading reached, in MiB/s);
- decoded: metrics the DogStatsD source decoded per second;
- CPU: cores used, and its share of the limit;
- CPU ns/metric: the container's CPU time over the metrics decoded;
- throttled: the share of CFS periods throttled;
- worst and peak second: lading's slowest and fastest one-second rates over the target, and the seconds below half
  the target (paced mode only). A stall shorter than a second, such as during an aggregation flush, does not show in
  the worst second, but lading catches up after it, so a run that keeps up with a peak well above 1 stalled;
- pool: the most datagram buffers in use at a sample;
- memory: peak (`memory.peak`, which counts page cache) and the most anonymous memory at a sample, in MiB, against the
  SMP case's bound;
- drops: metrics aggregation dropped and contexts the source failed to resolve, summed over rounds;
- with `perf stat` counts (SCALING_PERF=1): instructions per decoded metric, instructions per cycle, L1D and L2
  refills per metric, and the share of cycles stalled in the back end (the refills and stalls are scaled estimates).

Then the ratios of each label to the reference (decoded, CPU per metric, anonymous memory, and instructions per metric
when counted), and the spread from p25 to p75 over the median of decoded, CPU per metric, and instructions per metric,
for each label. The reference is the `reference` column `run-limits.sh` writes (the first of SCALING_VARIANTS), or
main for main and plan, or main-a for an A/A series, whose spreads are the noise.

Standard library only: run it with `python3 -I summarize_limits.py runs.csv`.
"""

import csv
import os
import statistics
import sys
from collections import defaultdict

TIER_ORDER = ["low", "medium", "heavy", "ultraheavy"]
PERF_HEADER = " instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |"


def quantile(values, at):
    """Returns the `at` quantile of `values` by linear interpolation."""
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * at
    low = int(position)
    high = min(low + 1, len(ordered) - 1)
    return ordered[low] + (ordered[high] - ordered[low]) * (position - low)


def spread(values):
    """Returns the spread from p25 to p75 of `values` over their median."""
    median = statistics.median(values)
    return (quantile(values, 0.75) - quantile(values, 0.25)) / median if median else 0.0


def read_series(path):
    """Returns the per-second series of a run: the rows, with `-` as None."""
    rows = []
    if not os.path.exists(path):
        return rows
    with open(path, newline="") as file:
        for row in csv.DictReader(file):
            rows.append({key: (None if value in ("-", "") else float(value)) for key, value in row.items()})
    return rows


def series_measures(rows, target):
    """Returns the worst and peak one-second send rates over `target`, the seconds below half of it, the most buffers
    in use, and the most anonymous memory (bytes), from a run's series."""
    worst, peak, stalled = None, None, 0
    for before, after in zip(rows, rows[1:]):
        if before["lading_bytes"] is None or after["lading_bytes"] is None:
            continue
        elapsed = after["t"] - before["t"]
        if elapsed <= 0:
            continue
        ratio = (after["lading_bytes"] - before["lading_bytes"]) / elapsed / target
        worst = ratio if worst is None else min(worst, ratio)
        peak = ratio if peak is None else max(peak, ratio)
        if ratio < 0.5:
            stalled += 1
    pool = max((row["pool_in_use"] for row in rows if row["pool_in_use"] is not None), default=None)
    anon = max((row["anon_bytes"] for row in rows if row["anon_bytes"] is not None), default=None)
    return worst, peak, stalled, pool, anon


def main(path):
    directory = os.path.dirname(os.path.abspath(path))
    runs = defaultdict(list)
    images = {}
    mode = "paced"
    reference = None

    def count(row, key):
        value = row.get(key)
        return float(value) if value not in (None, "", "-") else None

    with open(path, newline="") as file:
        for row in csv.DictReader(file):
            decoded = int(row["decoded"])
            seconds = float(row["seconds"])
            if seconds <= 0:
                continue
            mode = row["mode"]
            target = int(row["target_bytes_per_second"])
            worst, peak, stalled, pool, anon = series_measures(read_series(os.path.join(directory, row["series"])), target)
            periods = int(row["periods"])
            reference = row.get("reference") or reference
            instructions, cycles = count(row, "instructions"), count(row, "cycles")
            l1d, l2d, stalls = count(row, "l1d_refills"), count(row, "l2d_refills"), count(row, "stall_backend")
            runs[(row["tier"], float(row["limit"]), row["label"])].append(
                {
                    "send": int(row["lading_bytes"]) / seconds / target,
                    "send_mib": int(row["lading_bytes"]) / seconds / 2**20,
                    "decoded": decoded / seconds,
                    "cores": int(row["cpu_usec"]) / seconds / 1e6,
                    "cpu_ns_per_metric": int(row["cpu_usec"]) * 1_000 / decoded if decoded else float("nan"),
                    "throttled": int(row["throttled"]) / periods if periods else 0.0,
                    "worst": worst,
                    "peak": peak,
                    "stalled": stalled,
                    "pool": pool,
                    "peak_mib": int(row["memory_peak_bytes"]) / 2**20,
                    "anon_mib": anon / 2**20 if anon is not None else None,
                    "bound_mib": int(row["memory_bound_mib"]),
                    "agg_dropped": int(row["agg_dropped"]),
                    "unresolved": int(row["unresolved"]),
                    "instructions_per_metric": instructions / decoded if instructions and decoded else None,
                    "ipc": instructions / cycles if instructions and cycles else None,
                    "l1d_per_metric": l1d / decoded if l1d is not None and decoded else None,
                    "l2d_per_metric": l2d / decoded if l2d is not None and decoded else None,
                    "stalled_share": stalls / cycles if stalls is not None and cycles else None,
                }
            )
            images[row["label"]] = f"{row['image']} ({row['git_hash'] or '?'})"

    print(f"Mode: {mode}. Images: " + "; ".join(f"{label} = {image}" for label, image in sorted(images.items())))

    def median(values, key):
        present = [value[key] for value in values if value[key] is not None]
        return statistics.median(present) if present else None

    def fmt(value, spec):
        return "-" if value is None else format(value, spec)

    tiers = sorted({tier for tier, _, _ in runs}, key=lambda t: TIER_ORDER.index(t) if t in TIER_ORDER else 99)
    labels = sorted({label for _, _, label in runs})
    counted = any(v["instructions_per_metric"] is not None for values in runs.values() for v in values)
    for tier in tiers:
        print(f"\n### {tier}\n")
        if mode == "saturate":
            print("| CPUs | label | sent MiB/s | decoded/s | cores (share) | CPU ns/metric | throttled | pool | "
                  "peak / anon MiB (bound) | drops: agg, unresolved | rounds |" + PERF_HEADER * counted)
            print("|---|---|---|---|---|---|---|---|---|---|---|" + "---|" * (4 * counted))
        else:
            print("| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak "
                  "second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds |"
                  + PERF_HEADER * counted)
            print("|---|---|---|---|---|---|---|---|---|---|---|---|" + "---|" * (4 * counted))
        for limit in sorted({limit for t, limit, _ in runs if t == tier}):
            for label in labels:
                values = runs.get((tier, limit, label))
                if not values:
                    continue
                cores = median(values, "cores")
                send = f"{median(values, 'send_mib'):.1f}" if mode == "saturate" else f"{median(values, 'send'):.3f}"
                cells = [
                    f"{limit:g}",
                    label,
                    send,
                    f"{median(values, 'decoded'):,.0f}",
                    f"{cores:.3f} ({cores / limit:.2f})",
                    f"{median(values, 'cpu_ns_per_metric'):,.0f}",
                    f"{median(values, 'throttled'):.2f}",
                ]
                if mode != "saturate":
                    cells.append(
                        f"{fmt(median(values, 'worst'), '.2f')} / {fmt(median(values, 'peak'), '.2f')} "
                        f"({median(values, 'stalled'):g})"
                    )
                cells += [
                    fmt(median(values, "pool"), ".0f"),
                    f"{median(values, 'peak_mib'):.0f} / {fmt(median(values, 'anon_mib'), '.0f')} "
                    f"({values[0]['bound_mib']})",
                    f"{sum(v['agg_dropped'] for v in values):,}, {sum(v['unresolved'] for v in values):,}",
                    str(len(values)),
                ]
                if counted:
                    cells += [
                        fmt(median(values, "instructions_per_metric"), ",.0f"),
                        fmt(median(values, "ipc"), ".2f"),
                        f"{fmt(median(values, 'l1d_per_metric'), '.1f')} / {fmt(median(values, 'l2d_per_metric'), '.1f')}",
                        fmt(median(values, "stalled_share"), ".2f"),
                    ]
                print("| " + " | ".join(cells) + " |")

    if reference not in labels:
        reference = "main" if "main" in labels else "main-a" if "main-a" in labels else labels[0]
    for base, other in [(reference, label) for label in labels if label != reference]:
        print(f"\n### {other} against {base}\n")
        print("| tier | CPUs | decoded ratio | CPU/metric ratio | anon memory ratio | decoded spread "
              f"({base}, {other}) | CPU/metric spread ({base}, {other}) |"
              + f" instructions/metric ratio | instructions/metric spread ({base}, {other}) |" * counted)
        print("|---|---|---|---|---|---|---|" + "---|---|" * counted)
        for tier in tiers:
            for limit in sorted({limit for t, limit, _ in runs if t == tier}):
                a, b = runs.get((tier, limit, base)), runs.get((tier, limit, other))
                if not a or not b:
                    continue
                anon_a, anon_b = median(a, "anon_mib"), median(b, "anon_mib")
                cells = [
                    tier,
                    f"{limit:g}",
                    f"{median(b, 'decoded') / median(a, 'decoded'):.3f}",
                    f"{median(b, 'cpu_ns_per_metric') / median(a, 'cpu_ns_per_metric'):.3f}",
                    f"{anon_b / anon_a:.3f}" if anon_a and anon_b else "-",
                    f"{spread([v['decoded'] for v in a]):.3f}, {spread([v['decoded'] for v in b]):.3f}",
                    f"{spread([v['cpu_ns_per_metric'] for v in a]):.3f}, "
                    f"{spread([v['cpu_ns_per_metric'] for v in b]):.3f}",
                ]
                ia, ib = median(a, "instructions_per_metric"), median(b, "instructions_per_metric")
                if counted:
                    present = lambda runs_: [v["instructions_per_metric"] for v in runs_ if v["instructions_per_metric"]]
                    cells += [
                        f"{ib / ia:.3f}" if ia and ib else "-",
                        f"{spread(present(a)):.3f}, {spread(present(b)):.3f}" if present(a) and present(b) else "-",
                    ]
                print("| " + " | ".join(cells) + " |")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: summarize_limits.py runs.csv")
    main(sys.argv[1])
