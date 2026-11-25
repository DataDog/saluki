import json
import os
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path


CONVERTER = Path(__file__).with_name("panoramic_junit.py")
RUN_DIR = "panoramic-20260101"
LOG_DIR = f"/builds/saluki/integration-logs/42/{RUN_DIR}"


def passed(name, **extra):
    return dict({"name": name, "outcome": "passed", "duration_ms": 21, "issues": [], "assertions": []}, **extra)


class PanoramicJUnitTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.base = Path(directory.name) / "42"
        self.run = self.base / RUN_DIR / "run.json"
        self.xml = self.base / "panoramic.xml"
        self.report = {
            "schema_version": 2, "state": "complete", "interrupted_reason": None, "duration_ms": 38622,
            "log_dir": LOG_DIR, "running": [], "not_run": [],
            "tests": [{"name": "otlp-logs", "outcome": "passed", "duration_ms": 38622, "issues": []}],
        }

    def write(self, report=None):
        self.run.parent.mkdir(parents=True, exist_ok=True)
        self.run.write_text(report if isinstance(report, str) else json.dumps(report or self.report))

    def convert(self, suite="integration", *args, status="failed"):
        env = dict(os.environ, CI_JOB_STATUS=status)
        result = subprocess.run(
            [sys.executable, CONVERTER, self.base, suite, *args], capture_output=True, text=True, env=env,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result

    def suites(self):
        return {suite.get("name"): suite for suite in ET.parse(self.xml).getroot().iter("testsuite")}

    def harness(self):
        suites = self.suites()
        self.assertIn("panoramic.harness", suites)
        case, = suites["panoramic.harness"].findall("testcase")
        self.assertEqual(case.get("classname"), "panoramic.harness")
        return case

    def tags(self, case):
        return {p.get("name"): p.get("value") for p in case.iter("property")}

    def test_complete_correctness(self):
        self.write()
        self.convert("correctness", "--case", "otlp-logs")
        self.assertEqual(list(self.suites()), ["panoramic.correctness"])
        suite = self.suites()["panoramic.correctness"]
        self.assertEqual(suite.attrib, {
            "name": "panoramic.correctness", "tests": "1", "failures": "0", "errors": "0", "skipped": "0",
            "time": "38.622",
        })
        case, = suite.findall("testcase")
        self.assertEqual(case.attrib, {
            "name": "otlp-logs", "classname": "panoramic.correctness", "time": "38.622",
            "file": "test/correctness/cases/otlp-logs/config.yaml",
        })
        self.assertEqual(list(case), [])

    def test_failed_run_keeps_verdicts_and_bounded_evidence(self):
        mismatch = 'expected <2> & got "3"\x1b' + "é" * 1024
        case_dir = f"{LOG_DIR}/integration/compare"
        self.report.update({
            "not_run": ["after-fail-fast"],
            "tests": [
                passed("startup", issues=[{"kind": "resource_leaked", "message": "container x remains"}],
                       log_dir=f"{LOG_DIR}/integration/startup"),
                {"name": "compare", "outcome": "failed", "duration_ms": 2700, "log_dir": case_dir, "issues": [],
                 "diagnostic_lines": [{"source": f"{case_dir}/adp.stderr.log", "line_number": i, "text": f"bad {i}"}
                                      for i in range(25)],
                 "diagnostic_lines_truncated": True,
                 "assertions": [
                     {"step": 0, "member": None, "kind": "log", "status": "passed", "message": "ok"},
                     {"step": 1, "member": None, "kind": "compare", "status": "failed", "message": mismatch,
                      "details": [f"line {i}" for i in range(100)], "details_truncated": True,
                      "details_path": f"{case_dir}/details/1-compare.txt"},
                     {"step": 2, "member": 0, "kind": "metric", "status": "failed", "message": "second", "details": []},
                     {"step": 3, "member": None, "kind": "log", "status": "not_run", "message": "not run"},
                 ]},
                {"name": "setup", "outcome": "errored", "duration_ms": 9, "issues": [],
                 "error": {"kind": "setup", "message": "no image"}},
                {"name": "slow", "outcome": "timed_out", "duration_ms": 30000, "issues": [],
                 "log_dir": "c:\\mnt\\integration-logs\\42\\" + RUN_DIR + "\\integration\\slow",
                 "error": {"kind": "timeout", "message": "Test timed out after 30s."},
                 "timeout": {"deadline": "test_deadline", "configured_ms": 30000, "test_deadline_ms": 30000,
                             "active_phase": "assertions", "grace_phase": None},
                 "cause": {"by": "case_deadline", "active": [
                     {"step": 2, "member": 1, "kind": "intake_has_metric", "elapsed_ms": 4900, "budget_ms": 30000},
                 ]}},
                {"name": "linux-only", "outcome": "skipped", "duration_ms": 0, "issues": [],
                 "skip_reason": "does not declare windows"},
            ],
        })
        self.write()
        self.convert()
        suites = self.suites()
        self.assertEqual(list(suites), ["panoramic.integration"])
        suite = suites["panoramic.integration"]
        self.assertEqual(suite.attrib, {
            "name": "panoramic.integration", "tests": "5", "errors": "2", "failures": "1", "skipped": "1",
            "time": "38.622",
        })
        startup, compare, setup, slow, skipped = suite.findall("testcase")
        for case, test in zip(suite, self.report["tests"]):
            self.assertEqual(case.get("classname"), "panoramic.integration")
            self.assertEqual(case.get("name"), test["name"])
            self.assertEqual(case.get("file"), f"test/integration/cases/{test['name']}/config.yaml")

        # A passing case keeps its verdict, and its issues become a tag and output.
        self.assertIsNone(startup.find("failure"))
        self.assertEqual(self.tags(startup), {"dd_tags[test.panoramic.issues]": "resource_leaked"})
        self.assertIn("issue resource_leaked: container x remains", startup.find("system-out").text)
        self.assertIn(f"logs: {self.base.as_posix()}/{RUN_DIR}/integration/startup", startup.find("system-out").text)

        # The message stays short. The text names every failed step, bounded detail and the artifacts.
        failure = compare.find("failure")
        self.assertEqual(len(failure.get("message")), 500)
        self.assertTrue(failure.get("message").startswith('expected <2> & got "3"\ufffd'))
        text = failure.text
        self.assertIn("step 2 compare failed", text)
        self.assertIn("step 3.1 metric failed: second", text)
        self.assertNotIn("not run", text)
        self.assertIn("    line 19", text)
        self.assertNotIn("line 20", text)
        artifacts = f"{self.base.as_posix()}/{RUN_DIR}/integration/compare"
        self.assertIn(f"more detail in {artifacts}/details/1-compare.txt", text)
        self.assertIn(f"diagnostic {artifacts}/adp.stderr.log:9: bad 9", text)
        self.assertNotIn("bad 10", text)
        self.assertLessEqual(len(text), 8000)

        self.assertEqual(setup.find("error").attrib, {"type": "setup", "message": "no image"})
        self.assertEqual(self.tags(setup), {"dd_tags[test.panoramic.error_kind]": "setup"})

        error = slow.find("error")
        self.assertEqual(error.attrib, {"type": "timeout", "message": "Test timed out after 30s."})
        self.assertIn("deadline test_deadline (30000 ms) fired in phase assertions", error.text)
        self.assertIn("running steps: 3.2 intake_has_metric (4900 of 30000 ms)", error.text)
        self.assertIn(f"logs: {self.base.as_posix()}/{RUN_DIR}/integration/slow", error.text)
        self.assertEqual(self.tags(slow), {
            "dd_tags[test.panoramic.error_kind]": "timeout",
            "dd_tags[test.panoramic.timeout_deadline]": "test_deadline",
            "dd_tags[test.panoramic.active_phase]": "assertions",
            "dd_tags[test.panoramic.stopped_by]": "case_deadline",
        })

        self.assertEqual(skipped.find("skipped").get("message"), "does not declare windows")

    def test_interrupted_run_reports_finished_and_unfinished_cases(self):
        self.report.update({
            "state": "interrupted", "interrupted_reason": {"kind": "signal", "source": "SIGTERM"},
            "running": ["hung"], "not_run": ["never-started"],
            "tests": [
                passed("done"),
                {"name": "cancelled", "outcome": "errored", "duration_ms": 31000, "issues": [],
                 "error": {"kind": "internal", "message": "Test was cancelled."},
                 "cause": {"by": "signal", "active": []}},
            ],
        })
        self.write()
        self.convert(status="timedout")
        suites = self.suites()
        suite = suites["panoramic.integration"]
        self.assertEqual((suite.get("tests"), suite.get("errors")), ("3", "2"))
        done, cancelled, hung = suite.findall("testcase")
        self.assertEqual(list(done), [])
        self.assertEqual(cancelled.find("error").get("type"), "interrupted")
        self.assertEqual(self.tags(cancelled)["dd_tags[test.panoramic.error_kind]"], "internal")
        self.assertEqual(hung.get("name"), "hung")
        self.assertEqual(hung.get("file"), "test/integration/cases/hung/config.yaml")
        self.assertIsNone(hung.get("time"))
        self.assertEqual(hung.find("error").get("type"), "interrupted")
        self.assertIn("SIGTERM", hung.find("error").get("message"))
        self.assertNotIn("never-started", [case.get("name") for case in self.xml_cases()])

        harness = self.harness()
        self.assertEqual(harness.get("name"), "integration")
        error = harness.find("error")
        self.assertEqual(error.get("type"), "interrupted")
        self.assertIn("2 case(s) completed, 1 did not finish, 1 never started.", error.text)
        self.assertIn("job status: timedout", error.text)

    def xml_cases(self):
        return list(ET.parse(self.xml).getroot().iter("testcase"))

    def test_stale_running_report_marks_unfinished_cases(self):
        self.report.update({"state": "running", "running": ["hung"], "tests": [passed("done")]})
        self.write()
        self.convert()
        suite = self.suites()["panoramic.integration"]
        self.assertEqual([case.get("name") for case in suite], ["done", "hung"])
        self.assertEqual(self.harness().find("error").get("type"), "incomplete")

    def test_runner_failure_without_running_cases(self):
        self.report.update({
            "state": "interrupted", "not_run": ["b"], "tests": [passed("a")],
            "interrupted_reason": {"kind": "runner_failed", "message": "Runner task failed: panic"},
        })
        self.write()
        self.convert()
        self.assertEqual([case.get("name") for case in self.suites()["panoramic.integration"]], ["a"])
        self.assertIn("Runner task failed: panic", self.harness().find("error").get("message"))

    def test_attempt_without_a_run_report(self):
        # Another attempt's report is never borrowed.
        previous = self.base.parent / "41" / RUN_DIR / "run.json"
        previous.parent.mkdir(parents=True)
        previous.write_text(json.dumps(self.report))
        self.convert("correctness", "--case", "otlp-logs")
        self.assertEqual(list(self.suites()), ["panoramic.harness"])
        harness = self.harness()
        self.assertEqual(harness.get("name"), "correctness/otlp-logs")
        self.assertIsNone(harness.get("file"))
        self.assertEqual(harness.find("error").get("type"), "no_report")
        self.assertIn("job status: failed", harness.find("error").text)

    def test_unreadable_reports(self):
        diagnostic = self.run.parent / "integration" / "startup" / "result.json"
        diagnostic.parent.mkdir(parents=True)
        diagnostic.write_text("{}")
        other = self.base / "panoramic-20260102" / "run.json"
        for scenario, content in (
            ("unsupported", dict(self.report, schema_version=1)),
            ("invalid", "{"),
            ("not an object", "[]"),
            ("unknown state", dict(self.report, state="paused")),
            ("bad tests", dict(self.report, tests=None)),
            ("ambiguous", self.report),
        ):
            with self.subTest(scenario=scenario):
                self.write(content)
                if scenario == "ambiguous":
                    other.parent.mkdir()
                    other.write_text(json.dumps(self.report))
                self.xml.write_text("stale XML")
                self.convert()
                self.assertEqual(list(self.suites()), ["panoramic.harness"])
                self.assertEqual(self.harness().find("error").get("type"), "unreadable_report")
                self.assertTrue(diagnostic.exists())

    def test_one_unreadable_result_does_not_drop_the_run(self):
        self.report["tests"] = [
            passed("good"),
            {"name": "bad-error", "outcome": "errored", "duration_ms": 5, "error": None},
            {"name": "bad-skip", "outcome": "skipped", "duration_ms": 0},
            {"name": "new-outcome", "outcome": "flaked", "duration_ms": "soon"},
            {"outcome": "passed"},
        ]
        self.write()
        self.convert()
        good, bad_error, bad_skip, new_outcome = self.suites()["panoramic.integration"].findall("testcase")
        self.assertEqual(list(good), [])
        for case in (bad_error, bad_skip, new_outcome):
            error, = list(case)
            self.assertEqual(error.get("type"), "unreadable_result")
        self.assertIsNone(new_outcome.get("time"))
        self.assertIn("1 result(s) in the run report have no case name.", self.harness().find("error").get("message"))

    def test_waits_for_a_stopped_run_to_settle(self):
        self.report.update({"state": "interrupted", "interrupted_reason": {"kind": "signal", "source": "SIGTERM"},
                            "running": ["slow"], "tests": [passed("done")]})
        self.write()
        settled = dict(self.report, running=[], tests=[passed("done"), passed("slow")])
        timer = threading.Timer(0.5, self.write, [settled])
        timer.start()
        self.addCleanup(timer.cancel)
        self.convert("integration", "--wait", "20")
        suite = self.suites()["panoramic.integration"]
        self.assertEqual([(case.get("name"), list(case)) for case in suite], [("done", []), ("slow", [])])

    def test_wait_is_bounded(self):
        self.report.update({"state": "running", "running": ["hung"], "tests": []})
        self.write()
        started = time.monotonic()
        result = self.convert("integration", "--wait", "1")
        self.assertLess(time.monotonic() - started, 10)
        self.assertIn("did not settle", result.stderr)
        self.assertEqual(self.suites()["panoramic.integration"].find("testcase/error").get("type"), "interrupted")

    def test_complete_report_does_not_wait(self):
        self.write()
        started = time.monotonic()
        self.convert("integration", "--wait", "60")
        self.assertLess(time.monotonic() - started, 10)
        self.assertNotIn("panoramic.harness", self.suites())


if __name__ == "__main__":
    unittest.main()
