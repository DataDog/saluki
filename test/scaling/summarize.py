#!/usr/bin/env python3
"""Summarizes the runs of `run-curves.sh`: medians per CPU count and image, the plan's ratios to legacy, scaling.

Reads runs.csv (cpus, round, label, image, seconds, decoded, cpu_usec, lading_bytes, memory_peak_bytes, and the series'
profile, transport, and connections) and prints the series' configuration, then a Markdown table per CPU count with,
for each label, the median over rounds of:

- throughput: metrics the DogStatsD source decoded per second;
- throughput per CPU;
- CPU ns per metric: the container's CPU time over the metrics decoded;
- the share of lading's bytes per decoded metric, a check that the generator kept up (it should not move with N);
- peak memory.

Then, for two labels `legacy` and `plan`, the ratios the gate checks (plan section 8.5: throughput at least 0.93 of
legacy, scaling `T(N) / T(N_min)` at least 0.93 of legacy's, CPU per metric at most 1.07 of legacy's), and the spread
of each label's throughput from p25 to p75 against its median. For an A/A series (labels `legacy-a` and `legacy-b`),
the spread is the noise the tolerance must cover: the plan's spread must fit inside it.

Standard library only: run it with `python3 -I summarize.py runs.csv`.
"""

import csv
import statistics
import sys
from collections import defaultdict


def quantile(values, at):
    """Returns the `at` quantile of `values` by linear interpolation."""
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = (len(ordered) - 1) * at
    low = int(position)
    high = min(low + 1, len(ordered) - 1)
    return ordered[low] + (ordered[high] - ordered[low]) * (position - low)


def main(path):
    runs = defaultdict(list)
    configurations = set()
    with open(path, newline="") as file:
        for row in csv.DictReader(file):
            configurations.add(
                f"profile {row.get('profile') or '?'}, {row.get('transport') or '?'}, "
                f"{row.get('connections') or '?'} connection(s)"
            )
            decoded = int(row["decoded"])
            seconds = float(row["seconds"])
            if decoded <= 0 or seconds <= 0:
                print(f"skipping a run that decoded nothing: {row}", file=sys.stderr)
                continue
            runs[(int(row["cpus"]), row["label"])].append(
                {
                    "throughput": decoded / seconds,
                    "cpu_ns_per_metric": int(row["cpu_usec"]) * 1_000 / decoded,
                    "bytes_per_metric": int(row["lading_bytes"]) / decoded,
                    "memory_mib": int(row["memory_peak_bytes"]) / 2**20,
                }
            )

    print("Configuration: " + "; ".join(sorted(configurations)))
    labels = sorted({label for _, label in runs})
    cpu_counts = sorted({cpus for cpus, _ in runs})
    medians = {
        key: {measure: statistics.median(run[measure] for run in values) for measure in values[0]}
        for key, values in runs.items()
    }

    for cpus in cpu_counts:
        print(f"\n### N = {cpus}\n")
        print("| Measure | " + " | ".join(labels) + " |")
        print("|---|" + "---|" * len(labels))
        for measure, name in [
            ("throughput", "throughput (metrics/s)"),
            ("cpu_ns_per_metric", "CPU ns/metric"),
            ("bytes_per_metric", "lading bytes/metric"),
            ("memory_mib", "peak memory (MiB)"),
        ]:
            cells = [f"{medians[(cpus, label)][measure]:.1f}" if (cpus, label) in medians else "-" for label in labels]
            print(f"| {name} | " + " | ".join(cells) + " |")
        cells = []
        for label in labels:
            values = [run["throughput"] for run in runs.get((cpus, label), [])]
            if values:
                spread = (quantile(values, 0.75) - quantile(values, 0.25)) / statistics.median(values)
                cells.append(f"{spread:.3f} over {len(values)} rounds")
            else:
                cells.append("-")
        print("| throughput spread, p25 to p75 over the median | " + " | ".join(cells) + " |")
        cells = [
            f"{medians[(cpus, label)]['throughput'] / cpus:.0f}" if (cpus, label) in medians else "-" for label in labels
        ]
        print("| throughput per CPU | " + " | ".join(cells) + " |")

    pairs = [("legacy", "plan"), ("legacy-a", "legacy-b")]
    for base, other in pairs:
        if base not in labels or other not in labels:
            continue
        print(f"\n### {other} against {base}\n")
        print("| N | throughput ratio | scaling ratio | CPU/metric ratio |")
        print("|---|---|---|---|")
        first = cpu_counts[0]
        for cpus in cpu_counts:
            if (cpus, base) not in medians or (cpus, other) not in medians:
                continue
            ratio = medians[(cpus, other)]["throughput"] / medians[(cpus, base)]["throughput"]
            scaling_other = medians[(cpus, other)]["throughput"] / medians[(first, other)]["throughput"]
            scaling_base = medians[(cpus, base)]["throughput"] / medians[(first, base)]["throughput"]
            cpu = medians[(cpus, other)]["cpu_ns_per_metric"] / medians[(cpus, base)]["cpu_ns_per_metric"]
            print(f"| {cpus} | {ratio:.3f} | {scaling_other / scaling_base:.3f} | {cpu:.3f} |")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: summarize.py runs.csv")
    main(sys.argv[1])
