#!/opt/datadog-agent/embedded/bin/python3

import contextlib
import gzip
import importlib.util
import io
import json
import socket
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

PROBE_PATH = Path(__file__).with_name("otlp_http_probe.py")
SPEC_PATH = Path(__file__).with_name("probes.json")
spec = importlib.util.spec_from_file_location("otlp_http_probe", PROBE_PATH)
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)

PB = "application/x-protobuf"
TP = "text/plain; charset=utf-8"


def spec_text(known=None, expect=None):
    """A spec with one probe. `expect` and `known` replace the defaults."""
    one = {
        "id": "one",
        "description": "One probe.",
        "request": {"method": "POST", "path": "/v1/metrics", "headers": {"Content-Type": PB}, "body": "metrics-protobuf"},
        "expect": expect or {"status": 200, "headers": {"Content-Type": PB, "Content-Encoding": None}, "body": ""},
    }
    if known is not None:
        one["known"] = known
    return json.dumps({"probes": [one]})


def known(**got):
    return {"side": "adp", "gap": "gap/unit", "issue": 9, "reason": "A reason.", "got": got}


def answer(status=200, body=b"", **headers):
    headers = {name.replace("_", "-"): value for name, value in headers.items()}
    return {"status": status, "headers": headers, "body": body}


def verdict(text, side, response):
    (only,) = probe.decode_spec(text)
    return probe.judge(only, side, response)


class DecodeTest(unittest.TestCase):
    def assert_rejected(self, text, message):
        with self.assertRaisesRegex(probe.SpecError, message):
            probe.decode_spec(text)

    def test_the_committed_spec_decodes(self):
        probes = probe.decode_spec(SPEC_PATH.read_text(encoding="utf-8"))
        self.assertEqual(len({one["id"] for one in probes}), len(probes))
        for one in probes:
            self.assertEqual(set(one["known"]) - {"adp", "agent"}, set())

    def test_a_known_entry_that_gives_the_expected_value_is_rejected(self):
        self.assert_rejected(spec_text([known(status=200)]), "got.status: has the value of expect")
        self.assert_rejected(spec_text([known(headers={"content-type": PB})]), "has the value of expect")
        self.assert_rejected(spec_text([known(body="")]), "got.body: has the value of expect")

    def test_a_known_entry_needs_a_gap_an_issue_and_a_reason(self):
        entry = known(status=500)
        del entry["gap"]
        self.assert_rejected(spec_text([entry]), "missing key gap")
        entry = known(status=500)
        del entry["issue"]
        self.assert_rejected(spec_text([entry]), "missing key issue")
        entry = known(status=500)
        del entry["reason"]
        self.assert_rejected(spec_text([entry]), "missing key reason")
        entry = known(status=500)

    def test_a_known_entry_takes_a_positive_issue_number(self):
        for issue in ("soon", "ISSUE_UNIT", 0, -1, 1.5, True, None):
            entry = known(status=500)
            entry["issue"] = issue
            self.assert_rejected(spec_text([entry]), "expected an issue number")
        entry = known(status=500)
        entry["issue"] = 3500
        self.assertEqual(probe.decode_spec(spec_text([entry]))[0]["known"]["adp"]["issue"], 3500)

    def test_a_known_entry_takes_a_gap_in_the_form_of_the_cases(self):
        for gap in ("otlp-http", "gap/", "gap/Upper", "gap/a_b", "gap/a--b", "", None, 3):
            entry = known(status=500)
            entry["gap"] = gap
            self.assert_rejected(spec_text([entry]), "expected a gap")
        entry = known(status=500)
        entry["gap"] = "gap/otlp-http-content-type"
        self.assertEqual(
            probe.decode_spec(spec_text([entry]))[0]["known"]["adp"]["gap"], "gap/otlp-http-content-type"
        )

    def test_unknown_keys_are_rejected(self):
        self.assert_rejected(spec_text([known(status=500, colour="red")]), "unknown key colour")
        self.assert_rejected(spec_text(expect={"status": 200, "bodies": ""}), "unknown key bodies")
        self.assert_rejected(json.dumps({"probes": [], "extra": 1}), "unknown key extra")

    def test_a_known_entry_names_only_what_expect_checks(self):
        self.assert_rejected(spec_text([known(headers={"x-other": "1"})]), "expect does not check this header")
        self.assert_rejected(
            spec_text([known(body="OK")], expect={"status": 200}), "expect does not check the body"
        )

    def test_a_probe_has_one_known_entry_for_each_side(self):
        self.assert_rejected(spec_text([known(status=500), known(status=502)]), "has an entry for adp already")

    def test_a_body_check_is_exact_or_a_substring(self):
        self.assert_rejected(spec_text(expect={"status": 200, "body": "a", "body_contains": "a"}), "both body")


class JudgeTest(unittest.TestCase):
    def test_an_answer_that_meets_expect_passes(self):
        self.assertEqual(verdict(spec_text(), "adp", answer(content_type=PB))[0], "PASS")

    def test_an_answer_that_misses_expect_fails(self):
        self.assertEqual(verdict(spec_text(), "adp", answer(500, content_type=PB))[0], "FAIL")
        self.assertEqual(verdict(spec_text(), "adp", answer(content_type=TP))[0], "FAIL")
        self.assertEqual(verdict(spec_text(), "adp", answer(content_type=PB, content_encoding="gzip"))[0], "FAIL")
        self.assertEqual(verdict(spec_text(), "adp", answer(body=b"OK", content_type=PB))[0], "FAIL")

    def test_a_known_entry_matches_its_own_answer(self):
        text = spec_text([known(headers={"Content-Type": TP}, body="OK")])
        verdict_, entry, _ = verdict(text, "adp", answer(body=b"OK", content_type=TP))
        self.assertEqual((verdict_, entry["gap"], entry["issue"]), ("KNOWN", "gap/unit", 9))

    def test_a_known_entry_does_not_apply_to_the_other_side(self):
        text = spec_text([known(headers={"Content-Type": TP}, body="OK")])
        self.assertEqual(verdict(text, "agent", answer(body=b"OK", content_type=TP))[0], "FAIL")
        self.assertEqual(verdict(text, "agent", answer(content_type=PB))[0], "PASS")

    def test_an_answer_that_meets_expect_makes_the_entry_stale(self):
        text = spec_text([known(headers={"Content-Type": TP}, body="OK")])
        verdict_, entry, _ = verdict(text, "adp", answer(content_type=PB))
        self.assertEqual((verdict_, entry["gap"], entry["issue"]), ("STALE", "gap/unit", 9))

    def test_a_partly_fixed_answer_makes_the_entry_stale(self):
        text = spec_text([known(status=500, headers={"Content-Type": TP})])
        verdict_, _, problems = verdict(text, "adp", answer(500, content_type=PB))
        self.assertEqual(verdict_, "STALE")
        self.assertIn("header content-type meets expect", problems)
        self.assertEqual(verdict(text, "adp", answer(200, content_type=TP))[0], "STALE")

    def test_an_answer_that_matches_neither_fails(self):
        text = spec_text([known(status=500, headers={"Content-Type": TP})])
        self.assertEqual(verdict(text, "adp", answer(502, content_type="text/html"))[0], "FAIL")
        self.assertEqual(verdict(text, "adp", answer(500, content_type="text/html"))[0], "FAIL")

    def test_a_substring_check_replaces_an_exact_one(self):
        text = spec_text(
            [known(status=200, body="OK")], expect={"status": 400, "body_contains": "unsupported Content-Encoding"}
        )
        self.assertEqual(verdict(text, "adp", answer(200, b"OK"))[0], "KNOWN")
        self.assertEqual(verdict(text, "agent", answer(400, b"error: unsupported Content-Encoding: br"))[0], "PASS")
        self.assertEqual(verdict(text, "agent", answer(400, b"bad"))[0], "FAIL")


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        self.rfile.read(int(self.headers["Content-Length"]))
        self.send_response(200)
        self.send_header("Content-Type", TP)
        self.send_header("Content-Length", "2")
        self.end_headers()
        self.wfile.write(b"OK")

    def log_message(self, *arguments):
        pass


class RunTest(unittest.TestCase):
    def setUp(self):
        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=self.server.serve_forever, args=(0.05,), daemon=True)
        thread.start()
        self.addCleanup(thread.join, 1)
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.port = self.server.server_address[1]

    def run_helper(self, text, side, port=None):
        """Run the probes of `text` against the test server, or the port. Return the exit code and the output."""
        out = io.StringIO()

        def send_probe(one, first):
            body = probe.body_of(one["request"], one["id"])
            return probe.send("127.0.0.1", port or self.port, one["request"], body, 2)

        return probe.run(probe.decode_spec(text), side, send_probe, out), out.getvalue()

    def test_a_known_difference_exits_zero(self):
        code, output = self.run_helper(spec_text([known(headers={"Content-Type": TP}, body="OK")]), "adp")
        self.assertEqual(code, 0, output)
        self.assertIn("KNOWN gap/unit 9 one", output)

    def test_a_stale_entry_exits_one(self):
        code, output = self.run_helper(spec_text([known(status=500)]), "adp")
        self.assertEqual(code, 1, output)
        self.assertIn("STALE gap/unit 9 one", output)

    def test_an_unexpected_answer_exits_one(self):
        code, output = self.run_helper(spec_text(), "adp")
        self.assertEqual(code, 1, output)
        self.assertIn("FAIL one", output)

    def test_a_closed_port_is_an_error_and_never_known(self):
        with socket.socket() as closed:
            closed.bind(("127.0.0.1", 0))
            port = closed.getsockname()[1]
        text = spec_text([known(headers={"Content-Type": TP}, body="OK")])
        code, output = self.run_helper(text, "adp", port)
        self.assertEqual(code, 2, output)
        self.assertIn("ERROR one: connection refused", output)
        self.assertNotIn("KNOWN gap/", output)

    def test_the_main_function_reports_a_spec_error_with_code_two(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "probes.json"
            path.write_text(spec_text([known(status=200)]), encoding="utf-8")
            with contextlib.redirect_stderr(io.StringIO()) as errors:
                code = probe.main(["--spec", str(path), "--side", "adp", "--port", str(self.port)])
        self.assertEqual(code, 2)
        self.assertIn("got.status: has the value of expect", errors.getvalue())


class BodyTest(unittest.TestCase):
    def test_the_protobuf_encoder_matches_known_bytes(self):
        self.assertEqual(probe.varint(1), b"\x01")
        self.assertEqual(probe.varint(300), b"\xac\x02")
        self.assertEqual(probe.bytes_field(1, b"ab"), b"\x0a\x02ab")
        self.assertEqual(probe.varint_field(6, 2), b"\x30\x02")
        self.assertEqual(probe.fixed64_field(3, 1), b"\x19\x01\x00\x00\x00\x00\x00\x00\x00")

    def test_the_json_bodies_use_the_otlp_form(self):
        request = {"body": "traces-json", "encode": None}
        body = json.loads(probe.body_of(request, "traces-json"))
        span = body["resourceSpans"][0]["scopeSpans"][0]["spans"][0]
        self.assertEqual(len(span["traceId"]), 32)
        self.assertEqual(len(span["spanId"]), 16)
        self.assertNotEqual(span["traceId"], "0" * 32)
        self.assertIsInstance(span["startTimeUnixNano"], str)

    def test_a_metric_is_named_after_its_probe(self):
        self.assertEqual(probe.metric_name("metrics-no-content-type"), "otlp.rcv.http.metrics_no_content_type.probe")
        body = probe.body_of({"body": "metrics-protobuf", "encode": None}, "metrics-gzip")
        self.assertIn(b"otlp.rcv.http.metrics_gzip.probe", body)

    def test_gzip_compresses_the_body(self):
        plain = probe.body_of({"body": "metrics-protobuf", "encode": None}, "one")
        packed = probe.body_of({"body": "metrics-protobuf", "encode": "gzip"}, "one")
        self.assertEqual(gzip.decompress(packed)[:5], plain[:5])
        self.assertEqual(probe.body_of({"body": "garbage", "encode": None}, "one"), b"\x0f")


if __name__ == "__main__":
    unittest.main()
