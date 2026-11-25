import json
import subprocess
import sys
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path


CONVERTER = Path(__file__).with_name("panoramic_junit.py")


class PanoramicJUnitTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.base = Path(directory.name) / "42"
        self.run = self.base / "panoramic-20260101" / "run.json"
        self.run.parent.mkdir(parents=True)
        self.xml = self.base / "panoramic.xml"
        self.report = {
            "schema_version": 2, "state": "complete", "duration_ms": 38622,
            "totals": {"passed": 1, "failed": 0, "errored": 0, "timed_out": 0, "skipped": 0, "not_run": 0},
            "tests": [{"name": "otlp-logs", "outcome": "passed", "duration_ms": 38622}],
        }

    def convert(self, suite):
        return subprocess.run([sys.executable, CONVERTER, self.base, suite], capture_output=True, text=True)

    def test_complete_correctness(self):
        self.run.write_text(json.dumps(self.report))
        self.convert("correctness").check_returncode()
        suite = ET.parse(self.xml).find("testsuite")
        self.assertEqual(suite.get("name"), "panoramic.correctness")
        case, = suite.findall("testcase")
        self.assertEqual(case.attrib, {
            "name": "otlp-logs", "classname": "panoramic.correctness", "time": "38.622",
            "file": "test/correctness/cases/otlp-logs/config.yaml",
        })
        self.assertEqual(list(case), [])

    def test_failed_run_keeps_verdicts_but_not_unstarted_cases(self):
        mismatch = 'expected <2> & got "3"\x1b' + "é" * 1024
        self.report.update({
            "not_run": ["after-fail-fast"],
            "totals": {"passed": 1, "failed": 1, "errored": 2, "timed_out": 1, "skipped": 1, "not_run": 1},
            "tests": [
                {"name": "startup", "outcome": "passed", "duration_ms": 21, "issues": [{"kind": "resource_leaked"}]},
                {"name": "compare", "outcome": "failed", "duration_ms": 2700, "assertions": [
                    {"status": "passed", "message": "ok"}, {"status": "failed", "message": mismatch},
                    {"status": "not_run", "message": "not run"},
                ]},
                {"name": "setup", "outcome": "errored", "duration_ms": 9,
                 "error": {"kind": "setup", "message": "no image"}},
                {"name": "slow", "outcome": "timed_out", "duration_ms": 30000,
                 "error": {"kind": "timeout", "message": "case budget expired"}},
                {"name": "linux-only", "outcome": "skipped", "duration_ms": 0,
                 "skip_reason": "does not declare windows"},
            ],
        })
        self.run.write_text(json.dumps(self.report))
        self.convert("integration").check_returncode()
        suite = ET.parse(self.xml).find("testsuite")
        self.assertEqual(suite.attrib, {
            "name": "panoramic.integration", "tests": "5", "errors": "2", "failures": "1", "skipped": "1", "time": "38.622",
        })
        passed, failed, errored, timed_out, skipped = suite.findall("testcase")
        for case, test in zip(suite, self.report["tests"]):
            self.assertEqual(case.get("classname"), "panoramic.integration")
            self.assertEqual(case.get("name"), test["name"])
            self.assertEqual(case.get("file"), f"test/integration/cases/{test['name']}/config.yaml")
        self.assertEqual(list(passed), [])
        self.assertEqual(failed.find("failure").get("message"), mismatch[:1024].replace("\x1b", "\ufffd"))
        self.assertEqual(errored.find("error").attrib, {"type": "setup", "message": "no image"})
        self.assertEqual(timed_out.find("error").attrib, {"type": "timeout", "message": "case budget expired"})
        self.assertEqual(skipped.find("skipped").get("message"), "does not declare windows")

    def test_no_xml_for_partial_or_ambiguous_input(self):
        previous = self.base.parent / "41" / self.run.parent.name / "run.json"
        previous.parent.mkdir(parents=True)
        previous.write_text(json.dumps(self.report))
        diagnostic = self.run.parent / "integration" / "startup" / "result.json"
        diagnostic.parent.mkdir(parents=True)
        diagnostic.write_text("{}")
        for scenario in ("missing", "running", "interrupted", "unsupported", "invalid", "ambiguous"):
            with self.subTest(scenario=scenario):
                if scenario in ("running", "interrupted"):
                    self.run.write_text(json.dumps(dict(self.report, state=scenario)))
                elif scenario == "unsupported":
                    self.run.write_text(json.dumps(dict(self.report, schema_version=1)))
                elif scenario == "invalid":
                    self.run.write_text("{")
                elif scenario == "ambiguous":
                    self.run.write_text(json.dumps(self.report))
                    other = self.base / "panoramic-20260102" / "run.json"
                    other.parent.mkdir()
                    other.write_text(json.dumps(self.report))
                self.xml.write_text("stale XML")
                result = self.convert("integration")
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("no Panoramic JUnit report", result.stderr)
                self.assertFalse(self.xml.exists())
                self.assertTrue(diagnostic.exists())
