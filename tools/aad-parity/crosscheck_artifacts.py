#!/usr/bin/env python3
"""Cross-check a Rust headless artifact against the recorded engine-loop artifacts.

The recordings ship two side files next to the Parquet data:

* ``parquet/advances.jsonl`` — one ``{data_time, reason}`` record per engine advance;
* ``parquet/detect_digests.jsonl`` — one ``{detector, data_time, anomaly_count, input_hash,
  read_count, point_count}`` record per detector per advance.

This script overlays the Rust export (its per-second ``score_timeline`` gives the advanced seconds,
its ``detector_anomalies`` ledger gives per-detector per-second anomaly counts) against those files.

Caveats (important — this is an early-divergence signal, not a parity gate):

* the recordings were produced by a *production* observer configuration, so they may enable a
  different detector set with different warmups and an active baseline than the parity configs;
* the recordings applied dropped-observation filtering during live capture;
* ``input_hash`` is a Go-internal digest and is never compared to Rust state.

Only the overlap is reported; missing or extra advances are shown explicitly rather than filled.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from collections import Counter

# digest detector name -> canonical detector name used by the parity artifacts
DIGEST_NAME_MAP = {
    "bocpd_detector": "bocpd",
    "bocpd": "bocpd",
    "scanmw": "scanmw",
    "scanwelch": "scanwelch",
    "holt_residual": "holt_residual",
    "tukey_biweight": "tukey_biweight",
}


def read_jsonl(path: str):
    with open(path, "r", encoding="utf-8") as handle:
        for line in handle:
            line = line.strip()
            if line:
                yield json.loads(line)


def crosscheck(scenario: str, scenarios_dir: str, rust_path: str) -> dict:
    parquet_dir = os.path.join(scenarios_dir, scenario, "parquet")
    advances_path = os.path.join(parquet_dir, "advances.jsonl")
    digests_path = os.path.join(parquet_dir, "detect_digests.jsonl")
    rust = json.load(open(rust_path, "r", encoding="utf-8"))

    recorded_advances = [int(row["data_time"]) for row in read_jsonl(advances_path)]
    recorded_seconds = set(recorded_advances)

    # Rust-derived advances: the scorer emits one tick per advanced second.
    rust_seconds = {int(tick["second"]) for tick in rust.get("score_timeline", [])}

    recorded_counts: Counter = Counter()
    for row in read_jsonl(digests_path):
        detector = DIGEST_NAME_MAP.get(row.get("detector"), row.get("detector"))
        recorded_counts[detector] += int(row.get("anomaly_count", 0))

    rust_counts: Counter = Counter()
    for entry in rust.get("detector_anomalies") or []:
        rust_counts[entry.get("detector")] += 1

    overlap = sorted(recorded_seconds & rust_seconds)
    recorded_only = sorted(recorded_seconds - rust_seconds)
    rust_only = sorted(rust_seconds - recorded_seconds)

    detectors = sorted(set(recorded_counts) | set(rust_counts))
    first_advance_divergence = None
    if not overlap or recorded_seconds != rust_seconds:
        first_advance_divergence = {
            "recorded_min": min(recorded_seconds) if recorded_seconds else None,
            "recorded_max": max(recorded_seconds) if recorded_seconds else None,
            "rust_min": min(rust_seconds) if rust_seconds else None,
            "rust_max": max(rust_seconds) if rust_seconds else None,
            "recorded_only_count": len(recorded_only),
            "rust_only_count": len(rust_only),
            "first_recorded_only": recorded_only[:5],
            "first_rust_only": rust_only[:5],
        }

    return {
        "scenario": scenario,
        "parquet_dir": parquet_dir,
        "advances": {
            "recorded": len(recorded_seconds),
            "rust": len(rust_seconds),
            "overlap": len(overlap),
            "recorded_only": len(recorded_only),
            "rust_only": len(rust_only),
            "first_divergence": first_advance_divergence,
        },
        "detector_anomaly_counts": {
            detector: {"recorded": recorded_counts.get(detector, 0), "rust": rust_counts.get(detector, 0)}
            for detector in detectors
        },
        "caveats": [
            "recorded engine loop used a production observer config (detector set / warmups / baseline may differ)",
            "recorded side files applied live dropped-observation filtering",
            "input_hash digests are Go-internal and are not compared",
        ],
    }


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scenario", required=True)
    parser.add_argument("--scenarios-dir", required=True)
    parser.add_argument("--rust", required=True, help="Rust headless artifact to overlay")
    parser.add_argument("--out", default=None, help="optional JSON output path")
    args = parser.parse_args(argv)

    result = crosscheck(args.scenario, args.scenarios_dir, args.rust)
    text = json.dumps(result, indent=2)
    if args.out:
        with open(args.out, "w", encoding="utf-8") as handle:
            handle.write(text + "\n")
    print(text)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
