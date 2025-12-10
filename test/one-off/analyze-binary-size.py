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
import json
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


def get_workspace_crates() -> set[str]:
    """Get the set of workspace crate names from cargo metadata."""
    result = subprocess.run(
        ["cargo", "metadata", "--no-deps", "--format-version", "1"],
        capture_output=True,
        text=True,
        check=True,
    )
    metadata = json.loads(result.stdout)

    workspace_crates = set()
    for package in metadata.get("packages", []):
        # Workspace members have source == null (they're local, not from a registry)
        if package.get("source") is None:
            # Normalize hyphen to underscore (Cargo uses hyphens, Rust symbols use underscores)
            crate_name = package["name"].replace("-", "_")
            workspace_crates.add(crate_name)

    return workspace_crates


def parse_bloaty_csv(csv_output: str) -> list[dict]:
    """Parse bloaty CSV output into a list of dicts."""
    import io

    reader = csv.DictReader(io.StringIO(csv_output))
    parsed = []
    for row in reader:
        parsed.append(
            {
                "symbol": row["symbols"],
                "vmsize": int(row["vmsize"]),
                "filesize": int(row["filesize"]),
            }
        )
    return parsed


def demangle_escape_sequences(text: str) -> str:
    """Replace Rust symbol escape sequences with their readable equivalents."""
    import re

    # Named escape sequences
    replacements = [
        ("$LT$", "<"),
        ("$GT$", ">"),
        ("$RF$", "&"),
        ("$BP$", "*"),
        ("$C$", ","),
        ("..", "::"),  # Trait syntax uses .. instead of ::
    ]

    result = text
    for old, new in replacements:
        result = result.replace(old, new)

    # Handle arbitrary Unicode escape sequences: $uXX$ where XX is hex. We only do this
    # for values which are printable ASCII characters (0x20-0x7E).
    def decode_unicode_escape(match: re.Match) -> str:
        hex_val = match.group(1)
        code_point = int(hex_val, 16)
        if 0x20 <= code_point <= 0x7E:
            return chr(code_point)
        return match.group(0)  # Keep original if not printable ASCII

    result = re.sub(r"\$u([0-9a-fA-F]{2,})\$", decode_unicode_escape, result)

    return result


def demangle_symbol(symbol: str) -> str:
    """Demangle a Rust symbol name to extract the module path.

    Handles trait implementation symbols like:
    _$LT$figment..value..de..ConfiguredValueDe$LT$I$GT$$u20$as$u20$serde_core..de..Deserializer$GT$::...

    Returns the normalized module path (e.g., figment::value::de::ConfiguredValueDe).
    """
    result = demangle_escape_sequences(symbol)

    # Handle leading underscore (common in mangled names) before checking for trait impls
    if result.startswith("_"):
        result = result[1:]

    # Handle trait implementations: <Type as Trait>::method
    # Extract either the "from" type or the "as" trait depending on which is a real crate
    if result.startswith("<"):
        # Find the matching > for the outer angle bracket
        depth = 0
        for i, char in enumerate(result):
            if char == "<":
                depth += 1
            elif char == ">":
                depth -= 1
                if depth == 0:
                    # Extract content between < and >
                    inner = result[1:i]

                    if " as " in inner:
                        from_part, as_part = inner.split(" as ", 1)

                        # Remove generic parameters for checking
                        from_clean = from_part
                        if "<" in from_clean:
                            from_clean = from_clean[: from_clean.index("<")]

                        as_clean = as_part
                        if "<" in as_clean:
                            as_clean = as_clean[: as_clean.index("<")]

                        # Check if "from" part is a real crate (has ::)
                        # If not, it's a blanket impl like <&T as Trait>, use the "as" part
                        if "::" in from_clean:
                            inner = from_clean
                        else:
                            inner = as_clean
                    else:
                        # No " as ", just use the inner content
                        if "<" in inner:
                            inner = inner[: inner.index("<")]

                    result = inner
                    break

    # Handle leading underscore (common in mangled names)
    if result.startswith("_"):
        result = result[1:]

    # Handle leading < from incomplete trait impl extraction
    if result.startswith("<"):
        result = result[1:]

    return result


def extract_module_prefix(
    symbol: str, workspace_crates: set[str], workspace_depth: int = 3
) -> str:
    """Extract module prefix from a symbol name.

    For workspace crates, uses workspace_depth (default 3) to provide detailed module info.
    For third-party crates, rolls up to just the crate name (depth 1).
    """
    # Handle special section cases
    if symbol.startswith("[section .debug"):
        return "[debug sections]"
    elif symbol.startswith("[section "):
        return "[sections]"
    elif symbol.startswith("[") and "Others" in symbol:
        return symbol  # Keep [N Others] as-is

    # Demangle trait implementation symbols
    working_symbol = symbol
    if symbol.startswith("_$LT$") or symbol.startswith("$LT$"):
        working_symbol = demangle_symbol(symbol)

    # For Rust symbols, split on :: and determine depth based on crate ownership
    parts = working_symbol.split("::")
    if not parts:
        return symbol

    # Extract the crate name (first component)
    crate_name = parts[0]

    # Determine depth: workspace crates get detailed view, third-party gets rolled up
    if crate_name in workspace_crates:
        depth = workspace_depth
    else:
        depth = 1  # Just the crate name for third-party

    if len(parts) <= depth:
        return working_symbol

    return "::".join(parts[:depth])


def aggregate_by_module(
    parsed_csv: list[dict], workspace_crates: set[str], workspace_depth: int = 3
) -> dict[str, dict]:
    """Aggregate size changes by module prefix."""
    aggregated = {}

    for item in parsed_csv:
        module = extract_module_prefix(
            item["symbol"], workspace_crates, workspace_depth
        )

        if module not in aggregated:
            aggregated[module] = {"vmsize": 0, "filesize": 0, "count": 0}

        aggregated[module]["vmsize"] += item["vmsize"]
        aggregated[module]["filesize"] += item["filesize"]
        aggregated[module]["count"] += 1

    return aggregated


def format_module_rollup(aggregated: dict[str, dict], limit: int = 20) -> str:
    """Format aggregated module data as a markdown table."""
    # Sort by absolute filesize (largest changes first)
    sorted_modules = sorted(
        aggregated.items(), key=lambda x: abs(x[1]["filesize"]), reverse=True
    )

    # Take top N modules
    top_modules = sorted_modules[:limit]

    # Generate markdown table
    lines = []
    lines.append("| Module | File Size | Symbols |")
    lines.append("|--------|-----------|---------|")

    for module, data in top_modules:
        filesize_str = format_size_change(data["filesize"])
        count_str = str(data["count"])
        lines.append(f"| {module} | {filesize_str} | {count_str} |")

    return "\n".join(lines)


def generate_report(
    baseline_sha: str,
    comparison_sha: str,
    baseline_size: int,
    comparison_size: int,
    threshold_percent: float,
    bloaty_txt_output: str,
    module_rollup: str,
) -> tuple[str, bool]:
    """
    Generate a Markdown report and determine pass/fail status.

    Returns:
        Tuple of (markdown_report, passed)
    """
    size_diff = comparison_size - baseline_size

    if baseline_size != 0:
        change_percent = (size_diff / baseline_size) * 100
    else:
        change_percent = 0.0

    passed = change_percent <= threshold_percent

    if passed:
        status = "PASSED"
        status_emoji = "✅"
    else:
        status = "FAILED"
        status_emoji = "❌"

    baseline_size_fmt = format_size(baseline_size)
    comparison_size_fmt = format_size(comparison_size)
    size_diff_fmt = format_size_change(size_diff)

    if change_percent >= 0:
        change_percent_fmt = f"+{change_percent:.2f}%"
    else:
        change_percent_fmt = f"{change_percent:.2f}%"

    report = f"""
**Target:** {baseline_sha} (baseline) vs {comparison_sha} (comparison) [diff](../../compare/{baseline_sha}..{comparison_sha})
**Baseline Size:** {baseline_size_fmt}
**Comparison Size:** {comparison_size_fmt}
**Size Change:** {size_diff_fmt} ({change_percent_fmt})
**Pass/Fail Threshold:** +{threshold_percent:.0f}%
**Result:** {status} {status_emoji}

<details>
<summary>

### Changes by Module

{module_rollup}

</details>

<details>
<summary>

### Detailed Symbol Changes

</summary>

```
{bloaty_txt_output}
```
</details>
"""

    return report, passed


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Analyze binary size differences using bloaty and generate a human-friendly report."
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

    # Validate our configuration.
    if not args.baseline_binary.exists():
        print(f"Error: Baseline binary not found: {args.baseline_binary}")
        return 2

    if not args.comparison_binary.exists():
        print(f"Error: Comparison binary not found: {args.comparison_binary}")
        return 2

    if not args.bloaty_path.exists():
        print(f"Error: Bloaty binary not found: {args.bloaty_path}")
        return 2

    # Get the basic file sizes of each binary.
    baseline_size = get_file_size(args.baseline_binary)
    comparison_size = get_file_size(args.comparison_binary)

    print(f"Baseline binary size: {format_size(baseline_size)}")
    print(f"Comparison binary size: {format_size(comparison_size)}")

    # Execute bloaty to get the analysis output of symbol size, what symbols are new, what symbols are old, the size
    # change for identical symbols, and so on.
    print("Running bloaty analysis (CSV)...")
    csv_output_raw = run_bloaty(
        args.bloaty_path,
        args.comparison_binary,
        args.baseline_binary,
        csv_output=True,
    )

    # Demangle symbols in CSV output (only the first column, preserving CSV structure)
    csv_lines = csv_output_raw.strip().split("\n")
    demangled_csv_lines = [csv_lines[0]]  # Keep header as-is
    for line in csv_lines[1:]:
        # Split on comma (only last 2 are numeric), demangle first field (symbol), rejoin
        # The symbol is everything before the last two comma-separated values
        parts = line.rsplit(",", 2)
        if len(parts) == 3:
            symbol = demangle_escape_sequences(parts[0])
            # Quote symbol if it contains comma or quotes
            if "," in symbol or '"' in symbol:
                symbol = '"' + symbol.replace('"', '""') + '"'
            demangled_csv_lines.append(f"{symbol},{parts[1]},{parts[2]}")
        else:
            demangled_csv_lines.append(line)
    csv_output = "\n".join(demangled_csv_lines)
    args.output_csv.write_text(csv_output)

    # Get workspace crates for module rollup differentiation
    print("Fetching workspace crate list...")
    workspace_crates = get_workspace_crates()

    # Parse CSV and generate module-level rollup
    print("Generating module-level rollup...")
    parsed_csv = parse_bloaty_csv(csv_output)
    module_aggregated = aggregate_by_module(
        parsed_csv, workspace_crates, workspace_depth=3
    )
    module_rollup_table = format_module_rollup(module_aggregated, limit=20)

    # Run bloaty for human-readable output (top 20)
    print("Running bloaty analysis (text)...")
    txt_output = run_bloaty(
        args.bloaty_path,
        args.comparison_binary,
        args.baseline_binary,
        csv_output=False,
        limit=20,
    )
    txt_output = demangle_escape_sequences(txt_output)
    args.output_txt.write_text(txt_output)

    report, passed = generate_report(
        baseline_sha=args.baseline_sha,
        comparison_sha=args.comparison_sha,
        baseline_size=baseline_size,
        comparison_size=comparison_size,
        threshold_percent=args.threshold,
        bloaty_txt_output=txt_output,
        module_rollup=module_rollup_table,
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
            f"\nError: Binary size increased by {change_percent:.2f}%, which exceeds the {args.threshold:.0f}% threshold."
        )
        return 1

    return 0


if __name__ == "__main__":
    sys.exit(main())
