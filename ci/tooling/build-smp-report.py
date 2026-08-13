#!/usr/bin/env python3
"""
Generate a condensed Markdown benchmark report from SMP's report.json.

Wraps the `SMP report render` command.
If it fails ( report.json file is non-existenc, malformed, or missing elements),
a "benchmarks did not produce a report" placeholder is written
so the dependent reporting job still posts something useful to the PR.

Usage:
    python3 build-smp-report.py \\
        --smp-binary ./smp \\
        --report-json outputs/report.json \\
        --output-report outputs/condensed-report.md
"""

import argparse
import logging
import subprocess
import sys
from pathlib import Path


def main() -> int:
    logging.basicConfig(level=logging.INFO)
    parser = argparse.ArgumentParser(
        description="Generate a condensed Markdown SMP benchmark report.",
    )
    parser.add_argument(
        "--smp-binary",
        type=Path,
        required=True,
        help="Path to SMP binary",
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

    # Ensure the output directory exists so every write below — including the
    # failure-placeholder paths — can't fail with FileNotFoundError when the caller hasn't
    # created it.
    args.output_report.parent.mkdir(parents=True, exist_ok=True)

    smp_binary: Path = args.smp_binary.resolve()
    print(smp_binary.stat())

    cmd = (
        smp_binary.as_posix(),
        "report",
        "render",
        "--report-json",
        args.report_json,
        "--output-file",
        args.output_report,
        "--target-config-dir",
        "test/smp/regression/adp/full/",
        "--template-file",
        "ci/tooling/smp_condensed_report.md.j2",
    )
    logging.info("Running %s", cmd)
    try:
        subprocess.run(
            cmd,
            check=True,
        )
    except subprocess.CalledProcessError as exc:
        failure_report = (
            "## Optimization Goals: ⚠️ Report unavailable\n\n"
            "The benchmark run did not produce a usable report:\n"
            f"Stderr: \n{exc.stderr}\n\n"
            "Check the benchmark job logs for details.\n"
        )
        args.output_report.write_text(failure_report)
        logging.exception("Report rendering failed")

    return 0


if __name__ == "__main__":
    sys.exit(main())
