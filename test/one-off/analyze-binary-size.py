#!/usr/bin/env python3
"""
Analyze binary size differences using bloaty and generate a markdown report.

This script compares two binaries (baseline vs comparison) using Google's bloaty
tool and generates a markdown report suitable for posting to PRs.

Usage:
    python3 analyze-binary-size.py \
        --baseline-binary <path> \
        --comparison-binary <path> \
        --baseline-sha <sha> \
        --comparison-sha <sha> \
        --bloaty-path <path> \
        --output-report <path> \
        --output-csv <path> \
        --output-txt <path> \
        --threshold <percent>

Exit codes:
    0 - Success, size change within threshold
    1 - Size increase exceeded threshold
    2 - Error during execution
"""

import argparse
import csv
import os
import subprocess
import sys
from pathlib import Path


def get_file_size(path: Path) -> int:
    """Get the size of a file in bytes."""
    return path.stat().st_size


def format_size(bytes_val: int) -> str:
    """Format a byte count as a human-readable string."""
    abs_bytes = abs(bytes_val)
    if abs_bytes >= 1024 * 1024:
        return f"{bytes_val / (1024 * 1024):.2f} MiB"
    elif abs_bytes >= 1024:
        return f"{bytes_val / 1024:.2f} KiB"
    else:
        return f"{bytes_val} B"


def format_size_change(bytes_val: int) -> str:
    """Format a size change with +/- prefix."""
    if bytes_val >= 0:
        return f"+{format_size(bytes_val)}"
    else:
        return f"-{format_size(abs(bytes_val))}"


def run_bloaty(
    bloaty_path: Path,
    comparison_binary: Path,
    baseline_binary: Path,
    csv_output: bool = False,
    limit: int = 20,
) -> str:
    """Run bloaty and return its output."""
    cmd = [str(bloaty_path), "-d", "symbols"]

    if csv_output:
        cmd.extend(["--csv", "-n", "0", "-w"])
    else:
        cmd.extend(["-n", str(limit)])

    cmd.extend([str(comparison_binary), "--", str(baseline_binary)])

    result = subprocess.run(cmd, capture_output=True, text=True, check=True)
    return result.stdout


def generate_report(
    baseline_sha: str,
    comparison_sha: str,
    baseline_size: int,
    comparison_size: int,
    threshold_percent: float,
    bloaty_txt_output: str,
) -> tuple[str, bool]:
    """
    Generate a markdown report and determine pass/fail status.

    Returns:
        Tuple of (markdown_report, passed)
    """
    size_diff = comparison_size - baseline_size

    if baseline_size != 0:
        change_percent = (size_diff / baseline_size) * 100
    else:
        change_percent = 0.0

    # Determine pass/fail
    passed = change_percent <= threshold_percent

    if passed:
        status = "PASSED"
        status_emoji = "✅"
    else:
        status = "FAILED"
        status_emoji = "❌"

    # Format values for display
    baseline_size_fmt = format_size(baseline_size)
    comparison_size_fmt = format_size(comparison_size)
    size_diff_fmt = format_size_change(size_diff)

    if change_percent >= 0:
        change_percent_fmt = f"+{change_percent:.2f}%"
    else:
        change_percent_fmt = f"{change_percent:.2f}%"

    report = f"""## Binary Size Analysis {status_emoji}

| Metric | Value |
|--------|-------|
| **Baseline SHA** | `{baseline_sha}` |
| **Comparison SHA** | `{comparison_sha}` |
| **Baseline Size** | {baseline_size_fmt} |
| **Comparison Size** | {comparison_size_fmt} |
| **Size Change** | {size_diff_fmt} ({change_percent_fmt}) |
| **Status** | {status} (threshold: {threshold_percent:.0f}%) |

<details>
<summary>Top 20 Symbol Changes</summary>

```
{bloaty_txt_output}
```

</details>
"""

    return report, passed


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Analyze binary size differences using bloaty"
    )
    parser.add_argument(
        "--baseline-binary", type=Path, required=True, help="Path to baseline binary"
    )
    parser.add_argument(
        "--comparison-binary",
        type=Path,
        required=True,
        help="Path to comparison binary",
    )
    parser.add_argument(
        "--baseline-sha", type=str, required=True, help="Git SHA of baseline"
    )
    parser.add_argument(
        "--comparison-sha", type=str, required=True, help="Git SHA of comparison"
    )
    parser.add_argument(
        "--bloaty-path", type=Path, required=True, help="Path to bloaty binary"
    )
    parser.add_argument(
        "--output-report", type=Path, required=True, help="Path for markdown report"
    )
    parser.add_argument(
        "--output-csv", type=Path, required=True, help="Path for CSV output"
    )
    parser.add_argument(
        "--output-txt", type=Path, required=True, help="Path for text output"
    )
    parser.add_argument(
        "--threshold",
        type=float,
        default=5.0,
        help="Failure threshold as percentage (default: 5.0)",
    )

    args = parser.parse_args()

    # Validate inputs exist
    if not args.baseline_binary.exists():
        print(f"Error: Baseline binary not found: {args.baseline_binary}")
        return 2

    if not args.comparison_binary.exists():
        print(f"Error: Comparison binary not found: {args.comparison_binary}")
        return 2

    if not args.bloaty_path.exists():
        print(f"Error: Bloaty binary not found: {args.bloaty_path}")
        return 2

    # Get file sizes
    baseline_size = get_file_size(args.baseline_binary)
    comparison_size = get_file_size(args.comparison_binary)

    print(f"Baseline binary size: {format_size(baseline_size)}")
    print(f"Comparison binary size: {format_size(comparison_size)}")

    # Run bloaty for CSV output (full symbol list)
    print("Running bloaty analysis (CSV)...")
    csv_output = run_bloaty(
        args.bloaty_path,
        args.comparison_binary,
        args.baseline_binary,
        csv_output=True,
    )
    args.output_csv.write_text(csv_output)

    # Run bloaty for human-readable output (top 20)
    print("Running bloaty analysis (text)...")
    txt_output = run_bloaty(
        args.bloaty_path,
        args.comparison_binary,
        args.baseline_binary,
        csv_output=False,
        limit=20,
    )
    args.output_txt.write_text(txt_output)

    # Generate report
    report, passed = generate_report(
        baseline_sha=args.baseline_sha,
        comparison_sha=args.comparison_sha,
        baseline_size=baseline_size,
        comparison_size=comparison_size,
        threshold_percent=args.threshold,
        bloaty_txt_output=txt_output,
    )
    args.output_report.write_text(report)

    # Print summary
    size_diff = comparison_size - baseline_size
    if baseline_size != 0:
        change_percent = (size_diff / baseline_size) * 100
    else:
        change_percent = 0.0

    print(f"\nSize change: {format_size_change(size_diff)} ({change_percent:+.2f}%)")
    print(f"Threshold: {args.threshold:.0f}%")
    print(f"Status: {'PASSED' if passed else 'FAILED'}")

    if not passed:
        print(
            f"\nError: Binary size increased by {change_percent:.2f}%, "
            f"which exceeds the {args.threshold:.0f}% threshold."
        )
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
