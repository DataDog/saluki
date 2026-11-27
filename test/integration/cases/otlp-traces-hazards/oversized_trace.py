#!/opt/datadog-agent/embedded/bin/python3
"""Send an OTLP trace that is too large for ADP to send in one request, and check what reaches the intake.

The case runs this script in the ADP container and in the Agent container. The script uses the standard library only,
because it runs in the Agent image. It builds the trace when it runs, so the case commits no payload of several
megabytes.

Usage:

    oversized_trace.py --port N [--host 127.0.0.1] [--ready-timeout 30] [--request-timeout 60]
    oversized_trace.py --check --intake-url URL [--settle 20] [--poll 1] [--request-timeout 10]

The first form sends one OTLP/HTTP request to the receiver on `--port` and requires the answer 200. Only the first
connection waits for a refused connection, for up to `--ready-timeout` seconds, because the receiver can need time to
listen. The second form reads the span requests that the intake recorded, and prints a verdict.

The request
-----------

The request has two `ResourceSpans`, so ADP makes two traces from it and puts them in one buffer:

- The trace under test has the service `otlp-traces-hazards-oversized`. It holds one `SERVER` root and 300 `INTERNAL`
  children. Each child has an attribute of 20,000 random base64 characters, which is below the limit of 25,000
  characters of a `meta` value. The random text does not compress, so the trace is about 6 MB before compression and
  about 4.5 MB after it. That is over the 3.2 MB that ADP sends in one trace request. A trace is the smallest unit that
  ADP puts in a request, so it cannot split this one.
- The control has the service `otlp-traces-hazards-oversized-control` and one span, in its own `ResourceSpans`. A
  receiver that handles the request must deliver it, whatever it does with the trace under test.

The check
---------

The check reads `/traces/dump_span_requests` of the intake for `--settle` seconds, once for each `--poll` seconds, and
counts the spans of each service. The case runs it after a step that saw the control arrive from the same sender, so a
dead or stuck pipeline cannot pass. The check requires the control as well, and prints one verdict:

- `KNOWN gap/otlp-traces-oversized-trace 2428`: no span of the trace under test arrived within the settle time, and the
  control arrived. ADP drops a single trace that is too large for one request, and it does not yet send it in parts.
  The exit code is 0.
- `STALE gap/otlp-traces-oversized-trace 2428`: all 301 spans arrived. ADP delivers the trace now, so remove the known
  difference. The exit code is 1.
- `FAIL`: some spans of the trace under test arrived but not 301, more than 301 arrived, or the control did not arrive.
  The exit code is 1.
- `ERROR`: the dump is not readable, or it does not have the expected shape. An error is never `KNOWN`. The exit code
  is 2.
"""

import argparse
import base64
import http.client
import json
import os
import socket
import struct
import sys
import time
import urllib.error
import urllib.request

SERVICE = "otlp-traces-hazards-oversized"
CONTROL_SERVICE = "otlp-traces-hazards-oversized-control"

# The children of the root, and the characters of the attribute of each child.
CHILDREN = 300
ATTRIBUTE_CHARS = 20_000

# The spans of the trace under test: the root and its children.
SPAN_COUNT = CHILDREN + 1

# The known difference that the check reports.
GAP = "gap/otlp-traces-oversized-trace"
ISSUE = 2428

# The seeds of the IDs of the two traces.
TRACE_SEED = 0x0F20
CONTROL_SEED = 0x0F21

SPAN_KIND_SERVER = 2
SPAN_KIND_INTERNAL = 1

# The `Content-Type` that both receivers accept for a protobuf body.
CONTENT_TYPE = "application/x-protobuf"


# --- The request ------------------------------------------------------------------------------------------------


def varint(value):
    out = bytearray()
    while True:
        low = value & 0x7F
        value >>= 7
        if not value:
            out.append(low)
            return bytes(out)
        out.append(low | 0x80)


def varint_field(field, value):
    return varint(field << 3) + varint(value)


def fixed64_field(field, value):
    return varint(field << 3 | 1) + struct.pack("<Q", value)


def bytes_field(field, payload):
    return varint(field << 3 | 2) + varint(len(payload)) + payload


def trace_id(seed):
    return seed.to_bytes(16, "big")


def span_id(seed, number):
    return ((seed << 32) | number).to_bytes(8, "big")


def resource_protobuf(service):
    value = bytes_field(1, service.encode())  # AnyValue.string_value
    attribute = bytes_field(1, b"service.name") + bytes_field(2, value)  # KeyValue
    return bytes_field(1, attribute)  # Resource.attributes


def filler():
    """Random text that does not compress: base64 of random bytes."""
    return base64.b64encode(os.urandom(ATTRIBUTE_CHARS * 3 // 4)).decode()


def span_protobuf(seed, number, name, kind, now, parent=None, text=None):
    span = (
        bytes_field(1, trace_id(seed))
        + bytes_field(2, span_id(seed, number))
        + (bytes_field(4, span_id(seed, parent)) if parent else b"")
        + bytes_field(5, name.encode())
        + varint_field(6, kind)
        + fixed64_field(7, now - 5_000_000)
        + fixed64_field(8, now)
    )
    if text is not None:
        value = bytes_field(1, text.encode())  # AnyValue.string_value
        span += bytes_field(9, bytes_field(1, b"hazards.filler") + bytes_field(2, value))  # Span.attributes
    return span


def resource_spans(service, spans):
    scope_spans = b"".join(bytes_field(2, span) for span in spans)  # ScopeSpans.spans
    return bytes_field(1, resource_protobuf(service)) + bytes_field(2, scope_spans)  # ResourceSpans.scope_spans


def request_body(now):
    """The ExportTraceServiceRequest: the trace under test, then the control."""
    spans = [span_protobuf(TRACE_SEED, 1, "otlpprobe.hazards.oversized.root", SPAN_KIND_SERVER, now)]
    for number in range(2, SPAN_COUNT + 1):
        spans.append(
            span_protobuf(
                TRACE_SEED, number, "otlpprobe.hazards.oversized.child", SPAN_KIND_INTERNAL, now, parent=1, text=filler()
            )
        )
    control = [span_protobuf(CONTROL_SEED, 1, "otlpprobe.hazards.oversized.control", SPAN_KIND_SERVER, now)]
    return bytes_field(1, resource_spans(SERVICE, spans)) + bytes_field(1, resource_spans(CONTROL_SERVICE, control))


# --- Sending ----------------------------------------------------------------------------------------------------


def post(host, port, body, timeout):
    connection = http.client.HTTPConnection(host, port, timeout=timeout)
    try:
        connection.request("POST", "/v1/traces", body, {"Content-Type": CONTENT_TYPE})
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def post_when_ready(host, port, body, timeout, ready_timeout):
    """Like `post`, and on a refused connection tries again for `ready_timeout` seconds."""
    deadline = time.monotonic() + ready_timeout
    while True:
        try:
            return post(host, port, body, timeout)
        except ConnectionRefusedError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.5)


def send(args, out=None):
    out = out or sys.stdout
    body = request_body(time.time_ns())
    print(f"sending {len(body)} bytes to {args.host}:{args.port}", file=out, flush=True)
    try:
        status, answer = post_when_ready(args.host, args.port, body, args.request_timeout, args.ready_timeout)
    except (OSError, http.client.HTTPException) as error:
        print(f"ERROR the request ended with {type(error).__name__}: {error}", file=out, flush=True)
        return 2
    print(f"answer {status} body={answer[:100]!r}", file=out, flush=True)
    if status != 200:
        print(f"FAIL the receiver answered {status}, not 200", file=out, flush=True)
        return 1
    return 0


# --- The check --------------------------------------------------------------------------------------------------


class DumpError(Exception):
    """The dump of the intake is not readable, or does not have the expected shape."""


def read_dump(url, timeout):
    """Read the span requests of the intake, and return the number of spans for each service."""
    try:
        with urllib.request.urlopen(f"{url.rstrip('/')}/traces/dump_span_requests", timeout=timeout) as response:
            if response.status != 200:
                raise DumpError(f"the intake answered {response.status}")
            dump = json.load(response)
    except (OSError, http.client.HTTPException, urllib.error.URLError, socket.timeout) as error:
        raise DumpError(f"the dump is not readable: {type(error).__name__}: {error}") from error
    except ValueError as error:
        raise DumpError(f"the dump is not JSON: {error}") from error
    return count_spans(dump)


def count_spans(dump):
    """The number of spans for each service in a dump of span requests."""
    if not isinstance(dump, list):
        raise DumpError("the dump is not a list of requests")
    counts = {}
    for request in dump:
        items = request.get("items") if isinstance(request, dict) else None
        if not isinstance(items, list):
            raise DumpError("a request of the dump has no list of items")
        for span in items:
            if not isinstance(span, dict) or not isinstance(span.get("service"), str):
                raise DumpError("a span of the dump has no service")
            counts[span["service"]] = counts.get(span["service"], 0) + 1
    return counts


def judge(counts):
    """The verdict on the counts at the end of the settle time: `(verdict, message)`."""
    seen = counts.get(SERVICE, 0)
    control = counts.get(CONTROL_SERVICE, 0)
    if control != 1:
        return "FAIL", f"the control has {control} spans, not 1, so the check cannot say that the pipeline is healthy"
    if seen == SPAN_COUNT:
        return "STALE", f"all {SPAN_COUNT} spans of the trace arrived, so the known difference is gone"
    if seen:
        return "FAIL", f"{seen} spans of the trace arrived, and {SPAN_COUNT} are expected or none"
    return "KNOWN", f"none of the {SPAN_COUNT} spans arrived, and the control arrived"


def check(read, settle, poll, out=None, sleep=time.sleep, clock=time.monotonic):
    """Read the dump until `settle` seconds pass, print the verdict, and return the exit code.

    `read()` returns the number of spans for each service, or raises `DumpError`. The check ends early when the trace
    has all its spans, or has too many, because a later read cannot change that verdict.
    """
    out = out or sys.stdout
    deadline = clock() + settle
    while True:
        try:
            counts = read()
        except DumpError as error:
            print(f"ERROR {error}", file=out, flush=True)
            return 2
        seen = counts.get(SERVICE, 0)
        if seen >= SPAN_COUNT or clock() >= deadline:
            break
        sleep(poll)
    verdict, message = judge(counts)
    label = f"{verdict} {GAP} {ISSUE}" if verdict in ("KNOWN", "STALE") else verdict
    print(f"{label} {SERVICE}: {message}", file=out, flush=True)
    return 0 if verdict == "KNOWN" else 1


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true", help="read the intake, not send the request")
    parser.add_argument("--port", type=int, help="the port of the receiver")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--ready-timeout", type=float, default=30, help="how long the request waits for the port")
    parser.add_argument("--intake-url", help="the URL of the intake, such as http://intake-adp:2049")
    parser.add_argument("--settle", type=float, default=20, help="how long the check watches for the trace")
    parser.add_argument("--poll", type=float, default=1)
    parser.add_argument("--request-timeout", type=float)
    args = parser.parse_args(argv)
    if args.check:
        if not args.intake_url:
            parser.error("--check needs --intake-url")
        timeout = args.request_timeout or 10
        try:
            return check(lambda: read_dump(args.intake_url, timeout), args.settle, args.poll)
        except Exception as error:  # A crash of the script must not read as a failed check, which exits with code 1.
            print(f"error: {type(error).__name__}: {error}", file=sys.stderr)
            return 2
    if args.port is None:
        parser.error("--port is required")
    args.request_timeout = args.request_timeout or 60
    try:
        return send(args)
    except Exception as error:
        print(f"error: {type(error).__name__}: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
