"""Check that a Panoramic run report shows exactly the selected cases passing.

Usage: check_panoramic_selection.py <log base directory> <comma-separated case names>

The log base directory is the one given to `panoramic run -l`. It holds one `panoramic-<timestamp>` directory for each
run, and the check reads the `run.json` of the newest run. A local log directory can hold reports from earlier runs.

`panoramic run -t` rejects a name that matches no case, but a case that no longer declares the subject OS is skipped
and the run still exits 0. This check turns that skip into a failure.
"""

import json
import sys
from pathlib import Path


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    reports = sorted(Path(sys.argv[1]).glob("panoramic-*/run.json"), key=lambda r: r.stat().st_mtime)
    if not reports:
        sys.exit(f"ERROR: found no run report under {sys.argv[1]}")
    report = json.loads(reports[-1].read_text(encoding="utf-8"))
    selected = sys.argv[2].split(",")

    problems = []
    if report.get("state") != "complete":
        problems.append(f"run state is {report.get('state')!r}, expected 'complete'")
    if len(set(selected)) != len(selected):
        problems.append("the selection names a case twice")

    outcomes = {test["name"]: test["outcome"] for test in report.get("tests", [])}
    for name in sorted(set(selected) | set(outcomes)):
        if name not in outcomes:
            problems.append(f"{name}: selected but absent from the report")
        elif name not in selected:
            problems.append(f"{name}: reported but not selected")
        elif outcomes[name] != "passed":
            problems.append(f"{name}: {outcomes[name]}")

    if problems:
        print(f"ERROR: the run does not show exactly the {len(selected)} selected cases passing:", file=sys.stderr)
        for problem in problems:
            print(f"  {problem}", file=sys.stderr)
        sys.exit(1)
    print(f"[*] All {len(selected)} selected cases passed.")


if __name__ == "__main__":
    main()
