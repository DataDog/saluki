"""Convert the Panoramic run report of one CI attempt to JUnit.

Each case that completed keeps its verdict. Each case that was still running when the run stopped gets an
`interrupted` error. Cases that never started are left out. When the attempt has no complete run report, one
`panoramic.harness` test case records why, so a failed attempt is visible without a false product verdict.
"""

import argparse
import json
import os
import re
import sys
import time
import xml.etree.ElementTree as ET
from pathlib import Path

SCHEMA_VERSION = 2
RUN_STATES = ("running", "complete", "interrupted")
HARNESS_SUITE = "panoramic.harness"

# Bounds on what one test case carries. The GitLab artifacts hold the full logs and reports.
MESSAGE_CAP = 500
TEXT_CAP = 8000
LINE_CAP = 500
DETAIL_LINE_CAP = 20
DIAGNOSTIC_LINE_CAP = 10
TAG_VALUE_CAP = 200

# Assertion and harness messages can contain terminal control characters that XML 1.0 forbids.
INVALID_XML = re.compile(r"[^\t\n\r\x20-\ud7ff\ue000-\ufffd\U00010000-\U0010ffff]")


class UnreadableReport(Exception):
    """The run report, or one entry in it, does not match the expected schema."""


def clean(text, cap):
    text = str(text)
    if len(text) > cap:
        text = text[: cap - 1] + "…"
    return INVALID_XML.sub("\ufffd", text)


def field(entry, key, kind, required=True):
    value = entry.get(key) if isinstance(entry, dict) else None
    if value is None and not required:
        return None
    if not isinstance(value, kind):
        raise UnreadableReport(f"field '{key}' is {type(value).__name__}, expected {kind.__name__}")
    return value


class Report:
    """A schema-v2 run report and the location of its run directory."""

    def __init__(self, path, base):
        self.path = path
        self.base = base
        try:
            data = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, ValueError) as e:
            raise UnreadableReport(f"cannot read {path.name}: {e}") from e
        if not isinstance(data, dict):
            raise UnreadableReport("the report is not a JSON object")
        if data.get("schema_version") != SCHEMA_VERSION:
            raise UnreadableReport(f"unsupported schema version {data.get('schema_version')!r}")
        if data.get("state") not in RUN_STATES:
            raise UnreadableReport(f"unknown run state {data.get('state')!r}")
        self.state = data["state"]
        self.tests = field(data, "tests", list)
        self.running = [name for name in field(data, "running", list) if isinstance(name, str)]
        self.not_run = field(data, "not_run", list)
        self.interrupted_reason = field(data, "interrupted_reason", dict, required=False)
        duration = data.get("duration_ms")
        self.duration_ms = duration if isinstance(duration, (int, float)) else None
        self.run_dir = path.parent.name

    def settled(self):
        """Whether no case in the report can still change."""
        return self.state == "complete" or (self.state == "interrupted" and not self.running)

    def artifact(self, path):
        """Path of `path` in the job's artifacts, or `path` itself when it is outside the run directory."""
        path = str(path).replace("\\", "/")
        marker = f"/{self.run_dir}"
        index = path.find(marker + "/")
        if index < 0 and path.endswith(marker):
            index = len(path) - len(marker)
        if index < 0:
            return path
        return f"{self.base.as_posix()}{path[index:]}"


def find_report(base, wait):
    """Reads the attempt's only run report, waiting up to `wait` seconds for it to settle.

    Returns `None` when the attempt wrote no report.
    """
    deadline = time.monotonic() + wait
    while True:
        reports = sorted(base.glob("panoramic-*/run.json"))
        if not reports:
            return None
        if len(reports) > 1:
            raise UnreadableReport(f"found {len(reports)} run reports for one attempt")
        try:
            report = Report(reports[0], base)
        except UnreadableReport:
            if time.monotonic() >= deadline:
                raise
        else:
            if report.settled() or time.monotonic() >= deadline:
                return report
        time.sleep(1)


def step_label(entry):
    step = entry.get("step")
    member = entry.get("member")
    if not isinstance(step, int):
        return "?"
    return f"{step + 1}.{member + 1}" if isinstance(member, int) else f"{step + 1}"


def tag_value(value):
    return clean(re.sub(r"[^A-Za-z0-9_.:,/-]", "_", str(value)), TAG_VALUE_CAP)


class Case:
    """One JUnit test case, built from a run report entry."""

    def __init__(self, parent, suite, name, file, duration_ms):
        attributes = {"classname": suite, "name": clean(name, LINE_CAP)}
        if file:
            attributes["file"] = file
        if isinstance(duration_ms, (int, float)):
            attributes["time"] = f"{duration_ms / 1000:.3f}"
        self.element = ET.SubElement(parent, "testcase", attributes)
        self.tags = {}
        self.lines = []
        self.verdict_element = None

    def verdict(self, tag, message, **attributes):
        element = ET.SubElement(self.element, tag, attributes)
        element.set("message", clean(message, MESSAGE_CAP))
        self.verdict_element = element
        return element

    def finish(self):
        """Adds the tags, and the evidence lines as the text of the verdict, or as output when there is none."""
        if self.tags:
            properties = ET.Element("properties")
            for key, value in sorted(self.tags.items()):
                if value is None:
                    continue
                ET.SubElement(properties, "property", name=f"dd_tags[test.panoramic.{key}]", value=tag_value(value))
            self.element.insert(0, properties)
        text = "\n".join(clean(line, LINE_CAP) for line in self.lines)
        if len(text) > TEXT_CAP:
            text = text[: TEXT_CAP - 30] + "\n… truncated; see the artifacts."
        if not text:
            return
        if self.verdict_element is None:
            ET.SubElement(self.element, "system-out").text = text
        else:
            self.verdict_element.text = text


def add_completed(case, test, report):
    """Adds the verdict and bounded evidence of a completed test."""
    outcome = field(test, "outcome", str)
    if outcome == "passed":
        pass
    elif outcome == "failed":
        failed = [a for a in field(test, "assertions", list) if isinstance(a, dict) and a.get("status") == "failed"]
        message = str(failed[0].get("message")) if failed else "Assertion failed"
        case.verdict("failure", message)
    elif outcome in ("errored", "timed_out"):
        error = field(test, "error", dict)
        kind = field(error, "kind", str)
        cause = field(test, "cause", dict, required=False)
        stopped_by = cause.get("by") if cause else None
        case.tags["error_kind"] = kind
        # Panoramic records a case that a signal cancelled as an internal error. Report it as interrupted, like a case
        # that did not finish.
        case.verdict("error", field(error, "message", str), type="interrupted" if stopped_by == "signal" else kind)
        case.lines.append(f"{kind}: {error['message']}")
        add_timeout(case, field(test, "timeout", dict, required=False))
        if cause:
            case.tags["stopped_by"] = stopped_by
            active = [
                f"{step_label(s)} {s.get('kind')} ({s.get('elapsed_ms')} of {s.get('budget_ms')} ms)"
                for s in cause.get("active") or []
                if isinstance(s, dict)
            ]
            case.lines.append(f"stopped by {stopped_by}; running steps: {', '.join(active) or 'none'}")
    elif outcome == "skipped":
        case.verdict("skipped", field(test, "skip_reason", str))
        case.lines.append(test["skip_reason"])
    else:
        raise UnreadableReport(f"unknown test outcome {outcome!r}")

    if outcome != "skipped":
        add_assertions(case, test, report)
    issues = [i for i in test.get("issues") or [] if isinstance(i, dict)]
    if issues:
        case.tags["issues"] = ",".join(sorted({str(i.get("kind")) for i in issues}))
        case.lines.extend(f"issue {i.get('kind')}: {i.get('message')}" for i in issues)
    if outcome not in ("passed", "skipped"):
        add_diagnostics(case, test, report)
    if (issues or outcome not in ("passed", "skipped")) and isinstance(test.get("log_dir"), str):
        case.lines.append(f"logs: {report.artifact(test['log_dir'])}")
    case.finish()


def add_timeout(case, timeout):
    if not timeout:
        return
    case.tags["timeout_deadline"] = timeout.get("deadline")
    case.tags["active_phase"] = timeout.get("active_phase")
    line = f"deadline {timeout.get('deadline')} ({timeout.get('configured_ms')} ms)"
    line += f" fired in phase {timeout.get('active_phase')}"
    if timeout.get("grace_phase"):
        line += f"; grace expired in phase {timeout['grace_phase']}"
    case.lines.append(line)


def add_assertions(case, test, report):
    """Lists every step that ended without passing, with the start of its detail."""
    for entry in test.get("assertions") or []:
        if not isinstance(entry, dict) or entry.get("status") in ("passed", "not_run"):
            continue
        case.lines.append(f"step {step_label(entry)} {entry.get('kind')} {entry.get('status')}: {entry.get('message')}")
        details = [line for line in entry.get("details") or [] if isinstance(line, str)]
        case.lines.extend(f"    {line}" for line in details[:DETAIL_LINE_CAP])
        if len(details) > DETAIL_LINE_CAP or entry.get("details_truncated"):
            path = entry.get("details_path")
            case.lines.append(f"    … more detail in {report.artifact(path) if path else 'the artifacts'}")


def add_diagnostics(case, test, report):
    lines = [line for line in test.get("diagnostic_lines") or [] if isinstance(line, dict)]
    for line in lines[:DIAGNOSTIC_LINE_CAP]:
        source = f"{report.artifact(line.get('source'))}:{line.get('line_number')}"
        case.lines.append(f"diagnostic {source}: {line.get('text')}")
    if len(lines) > DIAGNOSTIC_LINE_CAP or test.get("diagnostic_lines_truncated"):
        case.lines.append("… more diagnostic lines in the run report")


def interruption(report):
    """Describes why the run did not complete."""
    reason = report.interrupted_reason or {}
    if report.state == "running":
        return "incomplete", "The run report still says running: Panoramic stopped before it finished the run."
    if reason.get("kind") == "signal":
        return "interrupted", f"Panoramic received {reason.get('source')} and stopped the run."
    detail = reason.get("message") or reason.get("kind") or "no reason given"
    return "interrupted", f"Panoramic stopped the run: {detail}"


def convert(base, suite, case_name, wait, job_status):
    # The directory is missing when the job failed before Panoramic started.
    base.mkdir(parents=True, exist_ok=True)
    output = base / "panoramic.xml"
    output.unlink(missing_ok=True)
    root = ET.Element("testsuites")
    harness = []

    try:
        report = find_report(base, wait)
    except UnreadableReport as e:
        report = None
        harness.append(("unreadable_report", f"Panoramic's run report is unreadable: {e}"))
    else:
        if report is None:
            message = "No Panoramic run report: the attempt stopped before Panoramic started a case."
            harness.append(("no_report", message))

    if report is not None:
        name = f"panoramic.{suite}"
        cases = ET.SubElement(root, "testsuite", name=name)
        completed = set()
        unnamed = 0
        for test in report.tests:
            test_name = test.get("name") if isinstance(test, dict) else None
            if not isinstance(test_name, str) or not test_name:
                unnamed += 1
                continue
            completed.add(test_name)
            case = Case(cases, name, test_name, f"test/{suite}/cases/{test_name}/config.yaml", test.get("duration_ms"))
            try:
                add_completed(case, test, report)
            except (UnreadableReport, AttributeError, KeyError, TypeError, ValueError) as e:
                for child in list(case.element):
                    case.element.remove(child)
                message = clean(f"unreadable result: {e}", MESSAGE_CAP)
                ET.SubElement(case.element, "error", type="unreadable_result", message=message)
        if unnamed:
            harness.append(("unreadable_report", f"{unnamed} result(s) in the run report have no case name."))

        if not report.settled():
            # Some cases may still finish after this snapshot. They are reported as they stand now.
            print(f"WARNING: the Panoramic run did not settle in {wait}s; reporting it as it stands.", file=sys.stderr)
        if report.state != "complete":
            kind, message = interruption(report)
            harness.insert(0, (kind, message))
            for running in report.running:
                if running in completed:
                    continue
                case = Case(cases, name, running, f"test/{suite}/cases/{running}/config.yaml", None)
                case.verdict("error", f"The case did not finish. {message}", type="interrupted")
                case.tags["stopped_by"] = "unfinished_run"
                case.finish()
            harness.insert(1, (kind, f"{len(completed)} case(s) completed, {len(report.running)} did not finish, "
                                  f"{len(report.not_run)} never started."))
        count(cases, report.duration_ms)

    if harness:
        harness_name = f"{suite}/{case_name}" if case_name else suite
        cases = ET.SubElement(root, "testsuite", name=HARNESS_SUITE)
        case = Case(cases, HARNESS_SUITE, harness_name, None, None)
        case.verdict("error", harness[0][1], type=harness[0][0])
        case.lines = [message for _, message in harness]
        if job_status:
            case.lines.append(f"job status: {job_status}")
        case.finish()
        count(cases, None)
        print(f"WARNING: reporting the Panoramic attempt as a harness error: {harness[0][1]}", file=sys.stderr)

    ET.indent(root)
    temporary = output.with_suffix(".tmp")
    ET.ElementTree(root).write(temporary, encoding="utf-8", xml_declaration=True)
    temporary.replace(output)


def count(suite, duration_ms):
    cases = suite.findall("testcase")
    suite.set("tests", str(len(cases)))
    for attribute, tag in (("failures", "failure"), ("errors", "error"), ("skipped", "skipped")):
        suite.set(attribute, str(sum(1 for case in cases if case.find(tag) is not None)))
    if duration_ms is not None:
        suite.set("time", f"{duration_ms / 1000:.3f}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("base", type=Path, help="the attempt's Panoramic log directory, as passed to `-l`")
    parser.add_argument("suite", choices=("integration", "correctness"))
    parser.add_argument("--case", help="the case a correctness job runs, to name its harness result")
    parser.add_argument("--wait", type=float, default=0, help="seconds to wait for a running report to settle")
    args = parser.parse_args()
    try:
        convert(args.base, args.suite, args.case, args.wait, os.environ.get("CI_JOB_STATUS"))
    except OSError as error:
        sys.exit(f"WARNING: no Panoramic JUnit report: {error}")
