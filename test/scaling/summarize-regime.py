#!/usr/bin/env python3
"""Summarizes a series of `run-regime.sh`: per CPU limit, plan against native in the steady window and the burst.

Reads `<dir>/runs.csv` (the readings of ADP's telemetry and lading's counters at the phase boundaries) and each run's
`<dir>/runs/<cpus>-<round>-<label>/timeline.csv` (the sampler's 100 ms rows, see `regime_sampler.py`), derives the
measures below per run, writes them to `<dir>/derived.csv`, and prints Markdown tables per limit: each image's median
over its runs (a label's prefix before `-` names its image: `native-a` and `native-b` pool into `native`, and likewise
`plan` and `candidate`), the ratios plan / native, candidate / plan, and candidate / native, each image's A/A ratio (the
median of its `-a` runs over the median of its `-b` runs), and each image's spread (p25 to p75 over the median, over all
its runs): the A/A ratio and the spread are the noise this host adds at that limit.

The steady window runs from the `s0` to the `s1` reading; the burst window from `b0` (the burst's start) to `b1` (its
end); the burst with its recovery from `b0` to `b2`.

- `decoded/s`: metrics the DogStatsD source decoded per second (ADP's `dsd_in` counter).
- `CPU ns/metric`: the cgroup's `usage_usec` over the metrics decoded.
- `CPU (cores)`: the cgroup's CPU time per second of the window.
- `throttled periods`: `nr_throttled` over `nr_periods`; `throttled ms/s`: `throttled_usec` per second.
- `shortfall`: the share of what the generators were configured to send that they did not write. Over a Unix datagram
  socket a sender blocks while the socket is full, and lading does not carry a second's unsent bytes over, so this is
  what a non-blocking client would have dropped.
- `longest stall (ms)`: in the burst window, the longest stretch of sampler rows over which neither generator's bytes
  written grew, while overloaded: ADP read nothing from the socket (throttled, or not admitting).
- `admission after / before the flush`: the generators' bytes written in the 2 s after the flush the burst is timed
  across, over the 2 s before it. Both are inside the overloaded burst, where a generator always has bytes to send, so
  this is how much ADP's reading from the socket slowed while it flushed.
- `anon (MiB)`: the cgroup's anonymous memory, median over the steady window, and its peak over the burst and recovery.
- `memory.peak (MiB)`: the cgroup's peak memory over the whole run.
- `pool capacity`, `pool created`: the datagram buffer pool (`dsd_packet_bufs`) at the end of the burst, and the
  buffers it created during the burst.
- `flushes in burst`: aggregation flushes between the burst's start and end (should be 1: the burst is timed across a
  flush).

Standard library only: `python3 -I summarize-regime.py <dir>`.
"""

import csv
import math
import os
import re
import statistics
import sys
from collections import defaultdict

MEASURES = [
    ("steady_decoded_per_s", "steady: decoded/s", "{:.0f}"),
    ("steady_cpu_ns_per_metric", "steady: CPU ns/metric", "{:.0f}"),
    ("steady_cores", "steady: CPU (cores)", "{:.3f}"),
    ("steady_throttled_share", "steady: throttled periods", "{:.3f}"),
    ("steady_throttled_ms_per_s", "steady: throttled ms/s", "{:.0f}"),
    ("steady_shortfall", "steady: shortfall", "{:.3f}"),
    ("steady_anon_mib", "steady: anon (MiB)", "{:.1f}"),
    ("burst_decoded_per_s", "burst: decoded/s", "{:.0f}"),
    ("burst_cpu_ns_per_metric", "burst+recovery: CPU ns/metric", "{:.0f}"),
    ("burst_throttled_share", "burst: throttled periods", "{:.3f}"),
    ("burst_throttled_ms_per_s", "burst: throttled ms/s", "{:.0f}"),
    ("burst_shortfall", "burst: shortfall", "{:.3f}"),
    ("burst_longest_stall_ms", "burst: longest stall (ms)", "{:.0f}"),
    ("flush_admission_ratio", "burst: admission after / before the flush", "{:.3f}"),
    ("burst_anon_peak_mib", "burst+recovery: anon peak (MiB)", "{:.1f}"),
    ("burst_pool_capacity", "burst end: pool capacity", "{:.0f}"),
    ("burst_pool_created", "burst: pool created", "{:.0f}"),
    ("memory_peak_mib", "run: memory.peak (MiB)", "{:.1f}"),
    ("flushes_in_burst", "flushes in burst", "{:.0f}"),
]


# The images a label can name (its prefix before `-`), and the ratios printed when both ran.
IMAGES = ["native", "plan", "candidate"]
COMPARISONS = [("plan", "native"), ("candidate", "plan"), ("candidate", "native")]


def number(value):
    try:
        return float(value)
    except (TypeError, ValueError):
        return math.nan


def parse_size(size):
    match = re.fullmatch(r"\s*([0-9.]+)\s*([KMG]?)(i?)([Bb])\s*", size)
    number_, prefix, binary, unit = match.groups()
    power = {"": 0, "K": 1, "M": 2, "G": 3}[prefix]
    return float(number_) * (1024 if binary else 1000) ** power / (8 if unit == "b" else 1)


def series_rates(directory):
    """Returns the steady rate per CPU (bytes/s) and the burst factor from series.txt."""
    text = open(os.path.join(directory, "series.txt")).read()
    match = re.search(r"steady_rate_per_cpu: (.+?); burst_factor: ([0-9.]+)", text)
    return parse_size(match.group(1)), float(match.group(2))


def at(rows, t, field):
    """The value of `field` in the last timeline row at or before `t` (the first row if none)."""
    chosen = rows[0]
    for row in rows:
        if float(row["t"]) > t:
            break
        chosen = row
    return number(chosen.get(field))


def between(rows, t0, t1):
    return [row for row in rows if t0 <= float(row["t"]) <= t1]


def delta(rows, t0, t1, field):
    return at(rows, t1, field) - at(rows, t0, field)


def longest_stall(rows):
    """Seconds of the longest stretch of rows over which the generators' bytes written did not grow."""
    longest = current = 0.0
    previous = None
    for row in rows:
        written = number(row.get("steady_written")) + number(row.get("burst_written"))
        if math.isnan(written):
            continue
        t = float(row["t"])
        if previous is not None:
            if written <= previous[1]:
                current += t - previous[0]
                longest = max(longest, current)
            else:
                current = 0.0
        previous = (t, written)
    return longest


def derive(directory, run, per_cpu, factor):
    cpus = float(run["cpus"])
    path = os.path.join(directory, "runs", f"{run['cpus']}-{run['round']}-{run['label']}", "timeline.csv")
    rows = [row for row in csv.DictReader(open(path)) if row.get("usage_usec")]
    r = {key: number(value) for key, value in run.items() if key not in ("label", "image")}
    s0, s1, b0, b1, b2 = r["s0_t"], r["s1_t"], r["b0_t"], r["b1_t"], r["b2_t"]
    steady_rate = number(run.get("steady_rate")) if run.get("steady_rate") else per_cpu * cpus
    burst_rate = number(run.get("burst_rate")) if run.get("burst_rate") else per_cpu * cpus * (factor - 1)
    out = {"cpus": run["cpus"], "round": run["round"], "label": run["label"]}

    decoded = r["s1_decoded"] - r["s0_decoded"]
    seconds = s1 - s0
    usage = delta(rows, s0, s1, "usage_usec")
    out["steady_decoded_per_s"] = decoded / seconds
    out["steady_cpu_ns_per_metric"] = usage * 1000 / decoded if decoded > 0 else math.nan
    out["steady_cores"] = usage / 1e6 / seconds
    periods = delta(rows, s0, s1, "nr_periods")
    out["steady_throttled_share"] = delta(rows, s0, s1, "nr_throttled") / periods if periods > 0 else 0.0
    out["steady_throttled_ms_per_s"] = delta(rows, s0, s1, "throttled_usec") / 1000 / seconds
    sent = r["s1_steady_sent"] - r["s0_steady_sent"]
    out["steady_shortfall"] = max(0.0, 1 - sent / (steady_rate * seconds))
    anon = [number(row["anon"]) for row in between(rows, s0, s1)]
    out["steady_anon_mib"] = statistics.median(anon) / 2**20 if anon else math.nan

    burst_seconds = b1 - b0
    burst_decoded = r["b1_decoded"] - r["b0_decoded"]
    out["burst_decoded_per_s"] = burst_decoded / burst_seconds
    total_decoded = r["b2_decoded"] - r["b0_decoded"]
    out["burst_cpu_ns_per_metric"] = delta(rows, b0, b2, "usage_usec") * 1000 / total_decoded
    periods = delta(rows, b0, b1, "nr_periods")
    out["burst_throttled_share"] = delta(rows, b0, b1, "nr_throttled") / periods if periods > 0 else 0.0
    out["burst_throttled_ms_per_s"] = delta(rows, b0, b1, "throttled_usec") / 1000 / burst_seconds
    burst_sent = number(run.get("burst_sent1")) - number(run.get("burst_sent0"))
    steady_sent = r["b1_steady_sent"] - r["b0_steady_sent"]
    # The burst generator ran from its unpause to its pause, about the configured burst length inside b0..b1.
    burst_on = number(run.get("burst_t1")) - number(run.get("burst_t0"))
    configured = steady_rate * burst_seconds + burst_rate * (burst_on if not math.isnan(burst_on) else 8)
    out["burst_shortfall"] = max(0.0, 1 - (steady_sent + burst_sent) / configured)
    window = between(rows, b0, b1)
    out["burst_longest_stall_ms"] = longest_stall(window) * 1000 if window else math.nan
    flush = r["flush"]
    before = delta(rows, flush - 2, flush, "steady_written") + delta(rows, flush - 2, flush, "burst_written")
    after = delta(rows, flush, flush + 2, "steady_written") + delta(rows, flush, flush + 2, "burst_written")
    out["flush_admission_ratio"] = after / before if before > 0 else math.nan
    anon = [number(row["anon"]) for row in between(rows, b0, b2)]
    out["burst_anon_peak_mib"] = max(anon) / 2**20 if anon else math.nan
    out["burst_pool_capacity"] = r["b1_pool_capacity"]
    out["burst_pool_created"] = r["b1_pool_created"] - r["b0_pool_created"]
    out["memory_peak_mib"] = number(rows[-1]["memory_peak"]) / 2**20
    out["flushes_in_burst"] = r["b1_flushes"] - r["b0_flushes"]
    return out


def quantile(ordered, at):
    """The `at` quantile of the sorted `ordered`, by linear interpolation."""
    position = (len(ordered) - 1) * at
    low = int(position)
    high = min(low + 1, len(ordered) - 1)
    return ordered[low] + (ordered[high] - ordered[low]) * (position - low)


def median(values):
    values = [v for v in values if not math.isnan(v)]
    return statistics.median(values) if values else math.nan


def ratio(a, b):
    if math.isnan(a) or math.isnan(b) or b == 0:
        return "-"
    return f"{a / b:.3f}"


def main(directory):
    per_cpu, factor = series_rates(directory)
    runs = list(csv.DictReader(open(os.path.join(directory, "runs.csv"))))
    derived = [derive(directory, run, per_cpu, factor) for run in runs]
    fields = list(derived[0])
    with open(os.path.join(directory, "derived.csv"), "w", newline="") as file:
        writer = csv.DictWriter(file, fieldnames=fields)
        writer.writeheader()
        for row in derived:
            writer.writerow({k: (f"{v:.4f}" if isinstance(v, float) else v) for k, v in row.items()})

    print(open(os.path.join(directory, "series.txt")).read().strip().replace("\n", "; "))
    groups = defaultdict(list)
    for row in derived:
        groups[(row["cpus"], row["label"])].append(row)
    for cpus in sorted({row["cpus"] for row in derived}, key=float):
        labels = sorted({label for c, label in groups if c == cpus})

        def pooled(image, key):
            return median(
                [row[key] for label in labels if label.split("-")[0] == image for row in groups[(cpus, label)]]
            )

        def spread(cpus, image, key):
            values = sorted(
                row[key]
                for label in labels
                if label.split("-")[0] == image
                for row in groups[(cpus, label)]
                if not math.isnan(row[key])
            )
            middle = median(values)
            if len(values) < 2 or not middle:
                return "-"
            return f"{(quantile(values, 0.75) - quantile(values, 0.25)) / abs(middle):.3f}"

        def single(label, key):
            return median([row[key] for row in groups.get((cpus, label), [])])

        images = [image for image in IMAGES if any(label.split("-")[0] == image for label in labels)]
        pairs = [(a, b) for a, b in COMPARISONS if a in images and b in images]
        count = {image: sum(len(groups[(cpus, l)]) for l in labels if l.split("-")[0] == image) for image in images}
        columns = [f"{image} ({count[image]})" for image in images]
        columns += [f"{a} / {b}" for a, b in pairs] + [f"A/A {image}" for image in images]
        columns += [f"spread {image}" for image in images]
        print(f"\n### {cpus} CPUs\n")
        print("| Measure | " + " | ".join(columns) + " |")
        print("|---|" + "---|" * len(columns))
        for key, name, fmt in MEASURES:
            values = {image: pooled(image, key) for image in images}
            cells = ["-" if math.isnan(values[image]) else fmt.format(values[image]) for image in images]
            cells += [ratio(values[a], values[b]) for a, b in pairs]
            cells += [ratio(single(f"{image}-a", key), single(f"{image}-b", key)) for image in images]
            cells += [spread(cpus, image, key) for image in images]
            print(f"| {name} | " + " | ".join(cells) + " |")

if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: summarize-regime.py <series directory>")
    main(sys.argv[1])
