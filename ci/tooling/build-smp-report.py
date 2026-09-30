#!/usr/bin/env python3
"""
Generate a condensed Markdown benchmark report from SMP's report.v1.json.

Wraps the `smp report render` command with saluki's condensed template
(ci/tooling/smp_condensed_report.md.j2). If rendering fails (report file is
missing, malformed, or missing elements), a "benchmarks did not produce a
report" placeholder is written so the dependent reporting job still posts
something useful to the PR.

Usage:
    python3 build-smp-report.py \\
        --smp-binary ./smp \\
        --report-v1-json outputs/report.v1.json \\
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
        "--report-v1-json",
        type=Path,
        required=True,
        help="Path to the report.v1.json produced by `smp job sync`.",
    )
    parser.add_argument(
        "--output-report",
        type=Path,
        required=True,
        help="Path to write the generated Markdown report to.",
    )
    args = parser.parse_args()

    # Ensure the output directory exists so every write below — including the
    # failure-placeholder path — can't fail with FileNotFoundError when the
    # caller hasn't created it.
    args.output_report.parent.mkdir(parents=True, exist_ok=True)

    if not args.report_v1_json.is_file():
        args.output_report.write_text(
            "## Optimization Goals: ⚠️ Report unavailable\n\n"
            f"The benchmark run did not produce a usable report: `{args.report_v1_json}` is missing\n\n"
            "Check the benchmark job logs for details.\n"
        )
        logging.error("Report %s is missing", args.report_v1_json)
        return 0

    smp_binary = args.smp_binary.resolve()

    cmd = (
        smp_binary.as_posix(),
        "report",
        "render",
        "--report",
        args.report_v1_json.resolve().as_posix(),
        "--template-file",
        "ci/tooling/smp_condensed_report.md.j2",
    )
    logging.info("Running %s", cmd)
    try:
        # Rendered report goes to stdout; the smp banner and logs go to stderr.
        result = subprocess.run(
            cmd,
            check=True,
            capture_output=True,
            text=True,
        )
    except subprocess.CalledProcessError as exc:
        stderr = exc.stderr or ""
        failure_report = (
            "## Optimization Goals: ⚠️ Report unavailable\n\n"
            "The benchmark run did not produce a usable report:\n"
            f"Stderr: \n{stderr}\n\n"
            "Check the benchmark job logs for details.\n"
        )
        args.output_report.write_text(failure_report)
        logging.error("Report rendering failed:\n%s", stderr)
        return 0

    args.output_report.write_text(result.stdout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
