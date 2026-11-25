"""Convert one CI attempt's completed Panoramic run to JUnit."""

import argparse
import json
import re
import sys
import xml.etree.ElementTree as ET
from pathlib import Path


# Assertion and harness messages can contain terminal control characters that XML 1.0 forbids.
INVALID_XML = re.compile(r"[^\t\n\r\x20-\ud7ff\ue000-\ufffd\U00010000-\U0010ffff]")


def add_verdict(case, test):
    outcome = test["outcome"]
    if outcome == "passed":
        return
    if outcome == "failed":
        element = ET.SubElement(case, "failure")
        failures = (step["message"] for step in test["assertions"] if step["status"] == "failed")
        message = next(failures, "Assertion failed")
    elif outcome in ("errored", "timed_out"):
        error = test["error"]
        element = ET.SubElement(case, "error", type=error["kind"])
        message = error["message"]
    elif outcome == "skipped":
        element = ET.SubElement(case, "skipped")
        message = test["skip_reason"]
    else:
        raise ValueError(f"unknown test outcome: {outcome}")
    message = INVALID_XML.sub("\ufffd", message[:1024])
    element.set("message", message)
    element.text = message


def convert(base, suite):
    output = base / "panoramic.xml"
    output.unlink(missing_ok=True)
    reports = list(base.glob("panoramic-*/run.json"))
    if len(reports) != 1:
        raise ValueError(f"expected one run report under {base}, found {len(reports)}")
    report = json.loads(reports[0].read_text(encoding="utf-8"))
    if report["schema_version"] != 2 or report["state"] != "complete":
        raise ValueError(f"expected a complete schema-v2 report, got v{report['schema_version']} {report['state']}")

    name = f"panoramic.{suite}"
    root = ET.Element("testsuites")
    totals = report["totals"]
    cases = ET.SubElement(
        root, "testsuite", name=name, tests=str(len(report["tests"])),
        failures=str(totals["failed"]), errors=str(totals["errored"]), skipped=str(totals["skipped"]),
        time=f"{report['duration_ms'] / 1000:.3f}",
    )
    # Only `tests` holds completed cases. The case name is its directory under the suite's `cases` directory.
    for test in report["tests"]:
        case = ET.SubElement(
            cases, "testcase", classname=name, name=test["name"],
            file=f"test/{suite}/cases/{test['name']}/config.yaml", time=f"{test['duration_ms'] / 1000:.3f}",
        )
        add_verdict(case, test)

    ET.indent(root)
    temporary = output.with_suffix(".tmp")
    ET.ElementTree(root).write(temporary, encoding="utf-8", xml_declaration=True)
    temporary.replace(output)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("base", type=Path)
    parser.add_argument("suite", choices=("integration", "correctness"))
    args = parser.parse_args()
    try:
        convert(args.base, args.suite)
    except (ValueError, KeyError, OSError, TypeError) as error:
        sys.exit(f"WARNING: no Panoramic JUnit report: {error}")
