#!/opt/datadog-agent/embedded/bin/python3

import contextlib
import importlib.util
import io
import socket
import threading
import unittest
import zlib
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

SCRIPT_PATH = Path(__file__).with_name("oversized_trace.py")
spec = importlib.util.spec_from_file_location("oversized_trace", SCRIPT_PATH)
script = importlib.util.module_from_spec(spec)
spec.loader.exec_module(script)

# The limit of a compressed trace request of ADP, in bytes.
ADP_COMPRESSED_LIMIT = 3_200_000

# The longest value of a `meta` tag, in characters.
META_VALUE_LIMIT = 25_000


def read_varint(data, offset):
    value = shift = 0
    while True:
        byte = data[offset]
        offset += 1
        value |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return value, offset
        shift += 7


def fields(data):
    """The fields of a protobuf message, as `(number, value)`: bytes for a length-delimited field, an int otherwise."""
    offset = 0
    while offset < len(data):
        key, offset = read_varint(data, offset)
        number, wire = key >> 3, key & 7
        if wire == 0:
            value, offset = read_varint(data, offset)
        elif wire == 1:
            value, offset = int.from_bytes(data[offset : offset + 8], "little"), offset + 8
        elif wire == 2:
            size, offset = read_varint(data, offset)
            value, offset = data[offset : offset + size], offset + size
        else:
            raise AssertionError(f"wire type {wire}")
        yield number, value


def decoded(body):
    """Each `ResourceSpans` of the request as `(service, spans)`, with each span as a dict of its fields by number."""
    result = []
    for number, resource_spans in fields(body):
        assert number == 1
        parts = {}
        for part, value in fields(resource_spans):
            parts.setdefault(part, []).append(value)
        (resource,) = parts[1]
        (attribute,) = [value for field, value in fields(resource) if field == 1]
        key, value = [value for _, value in fields(attribute)]
        assert key == b"service.name"
        service = [text for _, text in fields(value)][0].decode()
        (scope_spans,) = parts[2]
        spans = []
        for kind, span in fields(scope_spans):
            assert kind == 2
            by_number = {}
            for field, value in fields(span):
                by_number.setdefault(field, []).append(value)
            spans.append(by_number)
        result.append((service, spans))
    return result


class RequestTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.body = script.request_body(1_700_000_000_000_000_000)
        cls.resources = decoded(cls.body)

    def test_the_request_holds_the_trace_and_a_control_in_two_resources(self):
        self.assertEqual([service for service, _ in self.resources], [script.SERVICE, script.CONTROL_SERVICE])
        trace, control = (spans for _, spans in self.resources)
        self.assertEqual(len(trace), 301)
        self.assertEqual(len(control), 1)

    def test_the_trace_is_a_server_root_with_internal_children(self):
        spans = self.resources[0][1]
        root, children = spans[0], spans[1:]
        self.assertEqual(root[6], [script.SPAN_KIND_SERVER])
        self.assertNotIn(4, root)
        self.assertEqual(len(children), 300)
        for child in children:
            self.assertEqual(child[6], [script.SPAN_KIND_INTERNAL])
            self.assertEqual(child[4], root[2])
        self.assertEqual(len({span[1][0] for span in spans}), 1, "one trace")
        self.assertEqual(len({span[2][0] for span in spans}), 301, "span IDs differ")

    def test_the_control_is_a_different_trace(self):
        trace, control = (spans for _, spans in self.resources)
        self.assertNotEqual(trace[0][1], control[0][1])
        self.assertEqual(len(control[0][1][0]), 16)
        self.assertEqual(len(control[0][2][0]), 8)

    def test_each_child_holds_an_attribute_below_the_meta_limit(self):
        for child in self.resources[0][1][1:]:
            (attribute,) = child[9]
            key, value = [value for _, value in fields(attribute)]
            self.assertEqual(key, b"hazards.filler")
            (text,) = [text for _, text in fields(value)]
            self.assertEqual(len(text), script.ATTRIBUTE_CHARS)
            self.assertLess(len(text.decode()), META_VALUE_LIMIT)

    def test_the_trace_is_over_the_limit_of_adp_after_compression(self):
        self.assertGreater(len(zlib.compress(self.body, 9)), ADP_COMPRESSED_LIMIT)

    def test_two_requests_differ_in_their_text(self):
        self.assertNotEqual(script.filler(), script.filler())


def counts(trace=0, control=1):
    return {script.SERVICE: trace, script.CONTROL_SERVICE: control}


class JudgeTest(unittest.TestCase):
    def test_a_missing_trace_with_its_control_is_known(self):
        self.assertEqual(script.judge(counts(0))[0], "KNOWN")

    def test_a_delivered_trace_makes_the_known_difference_stale(self):
        self.assertEqual(script.judge(counts(301))[0], "STALE")

    def test_a_partial_or_doubled_trace_fails(self):
        self.assertEqual(script.judge(counts(5))[0], "FAIL")
        self.assertEqual(script.judge(counts(302))[0], "FAIL")

    def test_a_missing_control_fails_whatever_the_trace_is(self):
        for trace in (0, 5, 301):
            self.assertEqual(script.judge(counts(trace, 0))[0], "FAIL")
        self.assertEqual(script.judge({})[0], "FAIL")
        self.assertEqual(script.judge(counts(0, 2))[0], "FAIL")

    def test_the_spans_of_other_services_do_not_count(self):
        self.assertEqual(script.judge({**counts(0), "other": 301})[0], "KNOWN")


class CountTest(unittest.TestCase):
    def test_spans_are_counted_for_each_service_over_all_requests(self):
        dump = [{"items": [{"service": "a"}, {"service": "b"}]}, {"items": [{"service": "a"}]}]
        self.assertEqual(script.count_spans(dump), {"a": 2, "b": 1})
        self.assertEqual(script.count_spans([]), {})

    def test_a_dump_of_the_wrong_shape_is_an_error(self):
        for dump in ({}, [1], [{}], [{"items": {}}], [{"items": [1]}], [{"items": [{"name": "x"}]}]):
            with self.assertRaises(script.DumpError, msg=repr(dump)):
                script.count_spans(dump)


class Clock:
    """A clock that moves only when the check sleeps."""

    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class CheckTest(unittest.TestCase):
    def run_check(self, reads, settle=20):
        clock = Clock()
        out = io.StringIO()
        remaining = iter(reads)

        def read():
            item = next(remaining)
            if isinstance(item, Exception):
                raise item
            return item

        code = script.check(read, settle, 1, out, clock.sleep, clock)
        return code, out.getvalue(), clock.now

    def test_a_trace_that_never_arrives_is_known_after_the_settle_time(self):
        code, output, elapsed = self.run_check([counts(0)] * 100)
        self.assertEqual(code, 0, output)
        self.assertIn("KNOWN gap/otlp-traces-oversized-trace 2428", output)
        self.assertGreaterEqual(elapsed, 20)

    def test_a_late_trace_is_seen_before_the_settle_time_ends(self):
        code, output, _ = self.run_check([counts(0)] * 10 + [counts(301)])
        self.assertEqual(code, 1, output)
        self.assertIn("STALE gap/otlp-traces-oversized-trace 2428", output)

    def test_a_partial_trace_at_the_end_fails(self):
        code, output, _ = self.run_check([counts(0)] * 5 + [counts(7)] * 100)
        self.assertEqual(code, 1, output)
        self.assertIn("FAIL", output)
        self.assertNotIn("KNOWN", output)

    def test_an_unreadable_dump_is_an_error_and_never_known(self):
        code, output, _ = self.run_check([counts(0), script.DumpError("the dump is not readable")])
        self.assertEqual(code, 2, output)
        self.assertIn("ERROR the dump is not readable", output)
        self.assertNotIn("KNOWN", output)

    def test_the_main_function_fails_on_a_closed_intake(self):
        with socket.socket() as closed:
            closed.bind(("127.0.0.1", 0))
            port = closed.getsockname()[1]
        with contextlib.redirect_stdout(io.StringIO()) as out:
            code = script.main(["--check", "--intake-url", f"http://127.0.0.1:{port}", "--settle", "0.1"])
        self.assertEqual(code, 2, out.getvalue())
        self.assertIn("ERROR the dump is not readable", out.getvalue())


class Handler(BaseHTTPRequestHandler):
    answer = 200
    received = []

    def do_POST(self):
        Handler.received.append((self.path, self.headers["Content-Type"], self.rfile.read(int(self.headers["Content-Length"]))))
        self.send_response(Handler.answer)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        body = b'[{"items": [{"service": "otlp-traces-hazards-oversized-control"}]}]'
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *arguments):
        pass


class ServerTest(unittest.TestCase):
    def setUp(self):
        Handler.answer = 200
        Handler.received = []
        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=self.server.serve_forever, args=(0.05,), daemon=True)
        thread.start()
        self.addCleanup(thread.join, 1)
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.port = self.server.server_address[1]

    def test_the_request_is_one_protobuf_post_to_the_traces_path(self):
        with contextlib.redirect_stdout(io.StringIO()) as out:
            code = script.main(["--port", str(self.port)])
        self.assertEqual(code, 0, out.getvalue())
        ((path, content_type, body),) = Handler.received
        self.assertEqual((path, content_type), ("/v1/traces", "application/x-protobuf"))
        self.assertEqual([service for service, _ in decoded(body)], [script.SERVICE, script.CONTROL_SERVICE])

    def test_an_answer_other_than_200_fails(self):
        Handler.answer = 500
        with contextlib.redirect_stdout(io.StringIO()) as out:
            code = script.main(["--port", str(self.port)])
        self.assertEqual(code, 1, out.getvalue())
        self.assertIn("FAIL the receiver answered 500", out.getvalue())

    def test_a_closed_port_is_an_error(self):
        with socket.socket() as closed:
            closed.bind(("127.0.0.1", 0))
            port = closed.getsockname()[1]
        with contextlib.redirect_stdout(io.StringIO()) as out:
            code = script.main(["--port", str(port), "--ready-timeout", "0"])
        self.assertEqual(code, 2, out.getvalue())

    def test_the_dump_of_the_intake_is_read_over_http(self):
        self.assertEqual(
            script.read_dump(f"http://127.0.0.1:{self.port}/", 2), {"otlp-traces-hazards-oversized-control": 1}
        )


if __name__ == "__main__":
    unittest.main()
