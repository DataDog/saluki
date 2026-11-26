#!/opt/datadog-agent/embedded/bin/python3
"""Send fixed requests to the OTLP/HTTP receiver of ADP or of the Agent, and check each answer against a spec.

The case runs this script once in each arm, with the spec in `probes.json` and the port of that arm's receiver. The
script uses the standard library only, because it runs in the Agent image.

Usage:

    otlp_http_probe.py --spec probes.json --side {adp,agent} --port N
        [--host 127.0.0.1] [--ready-timeout 30] [--request-timeout 10] [--dry-run]

The spec
--------

`probes.json` is an object with one key, `probes`: a list of probes. The script sends them in file order, so a spec
lists the probes that deliver telemetry first. Decoding is strict: an unknown key, a missing key or a wrong type is a
spec error, and the script exits with code 2 before it sends anything.

A probe has these keys:

- `id`: a name of lower-case letters, digits and `-`. It is unique. The metric of a probe is called
  `otlp.rcv.http.<id with "-" replaced by "_">.probe`, so the case selects the delivery of one probe by its prefix.
- `description`: what the probe shows.
- `request`: what the script sends.
  - `method`: `GET` or `POST`.
  - `path`: the path, which starts with `/`.
  - `headers`: the request headers, as an object. The script sends no other header than these, `Host` and
    `Content-Length`. In particular, it sends no `Accept-Encoding` unless the probe lists one. Optional.
  - `body`: `none`, `metrics-protobuf`, `metrics-json`, `traces-protobuf`, `traces-json` or `garbage`. The default is
    `none`. A body that is not `garbage` holds one gauge point or one span, with the resource attribute
    `service.name=otlp-rcv-http` and the time of the run. `garbage` is the byte `0x0f`, a protobuf tag with the
    invalid wire type 7. The protobuf bodies are encoded by hand. The JSON bodies use the OTLP JSON form: camel case
    names, hexadecimal identifiers and nanoseconds as strings.
  - `encode`: `gzip` compresses the body. Optional.
- `expect`: what the Agent answers. Every key but `status` is optional.
  - `status`: the HTTP status.
  - `headers`: an object of response headers. A name is case-insensitive and a value is exact. A `null` value means
    that the header is absent.
  - `body`: the exact body, as UTF-8 text.
  - `body_contains`: text that the raw body holds. It works for a protobuf `google.rpc.Status`, which holds its message
    as UTF-8 text. A probe sets at most one of `body` and `body_contains`. Without either, the body does not matter.
- `known`: the differences that exist today. Optional. An entry has these keys:
  - `side`: `adp` or `agent`. The script ignores an entry for the other side. A probe has at most one entry for each
    side.
  - `issue`: the GitHub issue that tracks the difference. It is a number, or a placeholder such as `ISSUE_NAME` for an
    issue that nobody has filed yet. The script only checks the form of a placeholder, and prints it as written.
  - `reason`: why the side answers differently.
  - `got`: the answer of the side. It has the keys of `expect`, and overrides only the keys that it names. A header
    that `got` names replaces the header of `expect`, and the other headers stay. `body` and `body_contains` replace
    the body check of `expect`, whichever it is. `got` names only what `expect` checks, and never gives the value that
    `expect` gives: both are spec errors.

The verdicts
------------

The script prints one line for each probe, with the status, the headers and the start of the body that the receiver
sent, and a last line with the totals.

- `PASS`: the probe has no known entry for this side, and the answer meets `expect`.
- `KNOWN <issue>`: the answer meets `expect` with the overrides of `got`.
- `STALE <issue>`: the answer meets `expect`, or a field that `got` overrides has the value that `expect` gives. The
  receiver changed, so remove or narrow the entry. A stale entry fails the run.
- `FAIL`: the answer meets neither.
- `ERROR`: the request ended with a refused or reset connection, a timeout or a protocol error. An error is never
  `KNOWN`, so a dead receiver cannot pass as a known difference. Only the first probe waits for a refused connection,
  for up to `--ready-timeout` seconds, because the receiver can need time to listen.

Exit codes:

- 0: every probe is `PASS` or `KNOWN`.
- 1: a probe is `FAIL` or `STALE`.
- 2: the spec has an error, or a probe is `ERROR`.

`--dry-run` checks the spec and prints the requests, and sends nothing.
"""

import argparse
import gzip
import http.client
import json
import os
import re
import socket
import struct
import sys
import time

SERVICE = "otlp-rcv-http"
METRIC_PREFIX = "otlp.rcv.http."

BODIES = ("none", "metrics-protobuf", "metrics-json", "traces-protobuf", "traces-json", "garbage")
ENCODINGS = ("gzip",)
METHODS = ("GET", "POST")
SIDES = ("adp", "agent")

# Headers that the script sets itself.
RESERVED_HEADERS = ("host", "content-length")

ID_FORM = re.compile(r"[a-z0-9]+(-[a-z0-9]+)*")
PLACEHOLDER_FORM = re.compile(r"ISSUE_[A-Z][A-Z0-9_]*")

# The longest part of a response body that a verdict line shows.
SHOWN_BODY_BYTES = 100

# The headers a verdict line leaves out, because they differ on every run.
HIDDEN_HEADERS = ("date",)

# A protobuf tag with field number 1 and the invalid wire type 7.
GARBAGE = b"\x0f"


class SpecError(Exception):
    """The spec does not decode."""


# --- The spec ---------------------------------------------------------------------------------------------------


def keys_of(where, value, required=(), optional=()):
    """Check that `value` is an object whose keys are the required ones and some of the optional ones."""
    if not isinstance(value, dict):
        raise SpecError(f"{where}: expected an object")
    unknown = sorted(set(value) - set(required) - set(optional))
    if unknown:
        raise SpecError(f"{where}: unknown key {', '.join(unknown)}")
    missing = [key for key in required if key not in value]
    if missing:
        raise SpecError(f"{where}: missing key {', '.join(missing)}")
    return value


def text_of(where, value):
    if not isinstance(value, str) or not value:
        raise SpecError(f"{where}: expected a non-empty string")
    return value


def one_of(where, value, choices):
    if value not in choices:
        raise SpecError(f"{where}: expected one of {', '.join(choices)}, not {value!r}")
    return value


def headers_of(where, value, nullable):
    """Decode an object of headers into one with lower-case names."""
    if not isinstance(value, dict):
        raise SpecError(f"{where}: expected an object")
    headers = {}
    for name, header in value.items():
        text_of(f"{where} name", name)
        if header is not None or not nullable:
            if not isinstance(header, str):
                raise SpecError(f"{where}.{name}: expected a string" + (" or null" if nullable else ""))
        if name.lower() in headers:
            raise SpecError(f"{where}.{name}: the name appears twice")
        headers[name.lower()] = header
    return headers


def checks_of(where, value, status_required):
    """Decode `expect`, or the `got` of a known entry, into a dict of checks.

    The dict has the keys `status`, `headers` (lower-case names), `body` and `body_contains`, for those that the object
    names.
    """
    keys_of(where, value, ("status",) if status_required else (), ("status", "headers", "body", "body_contains"))
    checks = {}
    if "status" in value:
        status = value["status"]
        if isinstance(status, bool) or not isinstance(status, int) or not 100 <= status <= 599:
            raise SpecError(f"{where}.status: expected an HTTP status")
        checks["status"] = status
    if "headers" in value:
        checks["headers"] = headers_of(f"{where}.headers", value["headers"], nullable=True)
    if "body" in value and "body_contains" in value:
        raise SpecError(f"{where}: sets both body and body_contains")
    for key in ("body", "body_contains"):
        if key in value:
            if not isinstance(value[key], str):
                raise SpecError(f"{where}.{key}: expected a string")
            if key == "body_contains" and not value[key]:
                raise SpecError(f"{where}.{key}: expected a non-empty string")
            checks[key] = value[key]
    return checks


def request_of(where, value):
    keys_of(where, value, ("method", "path"), ("headers", "body", "encode"))
    request = {
        "method": one_of(f"{where}.method", value["method"], METHODS),
        "path": text_of(f"{where}.path", value["path"]),
        "headers": headers_of(f"{where}.headers", value.get("headers", {}), nullable=False),
        "body": one_of(f"{where}.body", value.get("body", "none"), BODIES),
        "encode": None,
    }
    if not request["path"].startswith("/"):
        raise SpecError(f"{where}.path: expected a path that starts with /")
    for name in request["headers"]:
        if name in RESERVED_HEADERS:
            raise SpecError(f"{where}.headers.{name}: the script sets this header")
    if "encode" in value:
        request["encode"] = one_of(f"{where}.encode", value["encode"], ENCODINGS)
        if request["body"] == "none":
            raise SpecError(f"{where}.encode: the request has no body to encode")
    if request["method"] == "GET" and request["body"] != "none":
        raise SpecError(f"{where}.body: a GET has no body")
    return request


def issue_of(where, value):
    """An issue is a number, or a placeholder for an issue that nobody has filed yet."""
    if isinstance(value, int) and not isinstance(value, bool) and value > 0:
        return value
    if isinstance(value, str) and PLACEHOLDER_FORM.fullmatch(value):
        return value
    raise SpecError(f"{where}: expected an issue number, or a placeholder such as ISSUE_NAME")


def known_of(where, value, expect):
    keys_of(where, value, ("side", "issue", "reason", "got"))
    got = checks_of(f"{where}.got", value["got"], status_required=False)
    if not got:
        raise SpecError(f"{where}.got: names nothing")
    entry = {
        "side": one_of(f"{where}.side", value["side"], SIDES),
        "issue": issue_of(f"{where}.issue", value["issue"]),
        "reason": text_of(f"{where}.reason", value["reason"]),
        "got": got,
    }
    if "status" in got and got["status"] == expect["status"]:
        raise SpecError(f"{where}.got.status: has the value of expect")
    for name, header in got.get("headers", {}).items():
        if name not in expect.get("headers", {}):
            raise SpecError(f"{where}.got.headers.{name}: expect does not check this header")
        if header == expect["headers"][name]:
            raise SpecError(f"{where}.got.headers.{name}: has the value of expect")
    for key in ("body", "body_contains"):
        if key in got:
            if "body" not in expect and "body_contains" not in expect:
                raise SpecError(f"{where}.got.{key}: expect does not check the body")
            if got[key] == expect.get(key):
                raise SpecError(f"{where}.got.{key}: has the value of expect")
    return entry


def probe_of(where, value):
    keys_of(where, value, ("id", "description", "request", "expect"), ("known",))
    probe_id = text_of(f"{where}.id", value["id"])
    if not ID_FORM.fullmatch(probe_id):
        raise SpecError(f"{where}.id: expected lower-case letters, digits and -")
    where = f"probe {probe_id}"
    expect = checks_of(f"{where}.expect", value["expect"], status_required=True)
    probe = {
        "id": probe_id,
        "description": text_of(f"{where}.description", value["description"]),
        "request": request_of(f"{where}.request", value["request"]),
        "expect": expect,
        "known": {},
    }
    entries = value.get("known", [])
    if not isinstance(entries, list):
        raise SpecError(f"{where}.known: expected a list")
    for number, entry in enumerate(entries):
        entry = known_of(f"{where}.known[{number}]", entry, expect)
        if entry["side"] in probe["known"]:
            raise SpecError(f"{where}.known[{number}].side: the probe has an entry for {entry['side']} already")
        probe["known"][entry["side"]] = entry
    return probe


def decode_spec(text):
    """Decode the text of a spec into a list of probes."""
    try:
        value = json.loads(text)
    except ValueError as error:
        raise SpecError(f"not JSON: {error}") from error
    keys_of("spec", value, ("probes",))
    if not isinstance(value["probes"], list) or not value["probes"]:
        raise SpecError("spec.probes: expected a list of probes")
    probes = [probe_of(f"probes[{number}]", probe) for number, probe in enumerate(value["probes"])]
    seen = set()
    for probe in probes:
        if probe["id"] in seen:
            raise SpecError(f"probe {probe['id']}: the id appears twice")
        seen.add(probe["id"])
    return probes


# --- The requests -----------------------------------------------------------------------------------------------


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


def double_field(field, value):
    return varint(field << 3 | 1) + struct.pack("<d", value)


def bytes_field(field, payload):
    return varint(field << 3 | 2) + varint(len(payload)) + payload


def resource_protobuf():
    value = bytes_field(1, SERVICE.encode())  # AnyValue.string_value
    attribute = bytes_field(1, b"service.name") + bytes_field(2, value)  # KeyValue
    return bytes_field(1, attribute)  # Resource.attributes


def metric_name(probe_id):
    return f"{METRIC_PREFIX}{probe_id.replace('-', '_')}.probe"


def metrics_protobuf(probe_id, now):
    point = fixed64_field(3, now) + double_field(4, 1.0)  # NumberDataPoint: time_unix_nano, as_double
    metric = bytes_field(1, metric_name(probe_id).encode()) + bytes_field(5, bytes_field(1, point))  # Metric: name, gauge
    resource_metrics = bytes_field(1, resource_protobuf()) + bytes_field(2, bytes_field(2, metric))
    return bytes_field(1, resource_metrics)


def traces_protobuf(probe_id, now, trace_id, span_id):
    span = (
        bytes_field(1, trace_id)
        + bytes_field(2, span_id)
        + bytes_field(5, f"{SERVICE}.{probe_id}".encode())
        + varint_field(6, 2)  # SPAN_KIND_SERVER
        + fixed64_field(7, now - 5_000_000)
        + fixed64_field(8, now)
    )
    resource_spans = bytes_field(1, resource_protobuf()) + bytes_field(2, bytes_field(2, span))
    return bytes_field(1, resource_spans)


def resource_json():
    return {"attributes": [{"key": "service.name", "value": {"stringValue": SERVICE}}]}


def metrics_json(probe_id, now):
    point = {"timeUnixNano": str(now), "asDouble": 1.0}
    metric = {"name": metric_name(probe_id), "gauge": {"dataPoints": [point]}}
    return {"resourceMetrics": [{"resource": resource_json(), "scopeMetrics": [{"metrics": [metric]}]}]}


def traces_json(probe_id, now, trace_id, span_id):
    span = {
        "traceId": trace_id.hex(),
        "spanId": span_id.hex(),
        "name": f"{SERVICE}.{probe_id}",
        "kind": 2,
        "startTimeUnixNano": str(now - 5_000_000),
        "endTimeUnixNano": str(now),
    }
    return {"resourceSpans": [{"resource": resource_json(), "scopeSpans": [{"spans": [span]}]}]}


def random_identifier(size):
    """Random bytes that are not all zero, which an OTLP identifier cannot be."""
    while True:
        identifier = os.urandom(size)
        if any(identifier):
            return identifier


def body_of(request, probe_id):
    """The bytes that the request sends."""
    kind = request["body"]
    now = time.time_ns()
    if kind == "none":
        return b""
    if kind == "garbage":
        body = GARBAGE
    elif kind == "metrics-protobuf":
        body = metrics_protobuf(probe_id, now)
    elif kind == "traces-protobuf":
        body = traces_protobuf(probe_id, now, random_identifier(16), random_identifier(8))
    elif kind == "metrics-json":
        body = json.dumps(metrics_json(probe_id, now)).encode()
    else:
        body = json.dumps(traces_json(probe_id, now, random_identifier(16), random_identifier(8))).encode()
    if request["encode"] == "gzip":
        body = gzip.compress(body)
    return body


# --- The answers ------------------------------------------------------------------------------------------------


def unmet(checks, answer):
    """The checks that the answer does not meet, as text. An empty list means that it meets them all."""
    problems = []
    if "status" in checks and answer["status"] != checks["status"]:
        problems.append(f"status {answer['status']}, not {checks['status']}")
    for name, value in checks.get("headers", {}).items():
        actual = answer["headers"].get(name)
        if actual != value:
            problems.append(f"header {name} {show_header(actual)}, not {show_header(value)}")
    if "body" in checks and answer["body"] != checks["body"].encode():
        problems.append(f"body is not {checks['body']!r}")
    if "body_contains" in checks and checks["body_contains"].encode() not in answer["body"]:
        problems.append(f"body does not hold {checks['body_contains']!r}")
    return problems


def show_header(value):
    return "absent" if value is None else json.dumps(value)


def overlay(expect, got):
    """`expect`, with the keys that `got` names replaced."""
    merged = dict(expect)
    if "status" in got:
        merged["status"] = got["status"]
    if "headers" in got:
        merged["headers"] = {**expect.get("headers", {}), **got["headers"]}
    if "body" in got or "body_contains" in got:
        merged.pop("body", None)
        merged.pop("body_contains", None)
        merged.update({key: got[key] for key in ("body", "body_contains") if key in got})
    return merged


def overridden(expect, got):
    """The checks of `expect` that `got` overrides, one by one: a list of `(label, checks)`."""
    items = []
    if "status" in got:
        items.append(("status", {"status": expect["status"]}))
    for name in got.get("headers", {}):
        items.append((f"header {name}", {"headers": {name: expect["headers"][name]}}))
    if "body" in got or "body_contains" in got:
        items.append(("body", {key: expect[key] for key in ("body", "body_contains") if key in expect}))
    return items


def judge(probe, side, answer):
    """The verdict on an answer: `(verdict, issue, problems)`.

    `issue` is the issue of the known entry for `side`, or `None`. `problems` says what the answer misses, for a `FAIL`,
    or what changed, for a `STALE`.
    """
    expect = probe["expect"]
    entry = probe["known"].get(side)
    failed = unmet(expect, answer)
    if entry is None:
        return ("FAIL", None, failed) if failed else ("PASS", None, [])
    if not failed:
        return "STALE", entry["issue"], ["the answer meets expect"]
    # A field that `got` overrides and that has the value of `expect` again is fixed, even when others are not.
    fixed = [f"{label} meets expect" for label, checks in overridden(expect, entry["got"]) if not unmet(checks, answer)]
    if fixed:
        return "STALE", entry["issue"], fixed
    problems = unmet(overlay(expect, entry["got"]), answer)
    if problems:
        return "FAIL", entry["issue"], problems + [f"the known entry {entry['issue']} does not match either"]
    return "KNOWN", entry["issue"], []


def show_answer(answer):
    headers = " ".join(
        f"{name}={json.dumps(value)}" for name, value in answer["headers"].items() if name not in HIDDEN_HEADERS
    )
    body = answer["body"][:SHOWN_BODY_BYTES]
    ellipsis = "..." if len(answer["body"]) > SHOWN_BODY_BYTES else ""
    return f"{answer['status']} {headers} body={body!r}{ellipsis}"


# --- Sending ----------------------------------------------------------------------------------------------------


def send(host, port, request, body, timeout):
    """Send one request on a new connection, and return the answer."""
    connection = http.client.HTTPConnection(host, port, timeout=timeout)
    try:
        connection.putrequest(request["method"], request["path"], skip_accept_encoding=True)
        for name, value in request["headers"].items():
            connection.putheader(name, value)
        if request["method"] == "POST":
            connection.putheader("Content-Length", str(len(body)))
        connection.endheaders(body if request["method"] == "POST" else None)
        response = connection.getresponse()
        content = response.read()
        headers = {}
        for name, value in response.getheaders():
            name = name.lower()
            headers[name] = f"{headers[name]}, {value}" if name in headers else value
        return {"status": response.status, "headers": headers, "body": content}
    finally:
        connection.close()


def send_when_ready(host, port, request, body, timeout, ready_timeout):
    """Like `send`, and on a refused connection tries again for `ready_timeout` seconds."""
    deadline = time.monotonic() + ready_timeout
    while True:
        try:
            return send(host, port, request, body, timeout)
        except ConnectionRefusedError:
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.5)


def problem_of(error):
    if isinstance(error, ConnectionRefusedError):
        return "connection refused"
    if isinstance(error, (socket.timeout, TimeoutError)):
        return "timeout"
    if isinstance(error, ConnectionError):
        return f"connection broken: {error}"
    if isinstance(error, http.client.HTTPException):
        return f"protocol error: {type(error).__name__}: {error}"
    return f"{type(error).__name__}: {error}"


def run(probes, side, send_probe, out=sys.stdout):
    """Run the probes through `send_probe(probe, first) -> answer`, print the verdicts, and return the exit code."""
    counts = {verdict: 0 for verdict in ("PASS", "KNOWN", "STALE", "FAIL", "ERROR")}
    for number, probe in enumerate(probes):
        try:
            answer = send_probe(probe, number == 0)
        except (OSError, http.client.HTTPException) as error:
            counts["ERROR"] += 1
            print(f"ERROR {probe['id']}: {problem_of(error)}", file=out, flush=True)
            continue
        verdict, issue, problems = judge(probe, side, answer)
        counts[verdict] += 1
        label = f"{verdict} {issue}" if verdict in ("KNOWN", "STALE") else verdict
        line = f"{label} {probe['id']}: {show_answer(answer)}"
        if problems:
            line += f" -- {'; '.join(problems)}"
        print(line, file=out, flush=True)
    totals = ", ".join(f"{count} {verdict}" for verdict, count in counts.items())
    print(f"otlp_http_probe side={side}: {len(probes)} probes: {totals}", file=out, flush=True)
    if counts["ERROR"]:
        return 2
    if counts["FAIL"] or counts["STALE"]:
        return 1
    return 0


def show_request(probe, body):
    request = probe["request"]
    headers = " ".join(f"{name}={json.dumps(value)}" for name, value in request["headers"].items())
    encoding = f" encode={request['encode']}" if request["encode"] else ""
    return f"{request['method']} {request['path']} {headers} body={request['body']}{encoding} ({len(body)} bytes)"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--spec", required=True, help="the probes.json file")
    parser.add_argument("--side", required=True, choices=SIDES, help="the receiver, which picks the known entries")
    parser.add_argument("--port", type=int, help="the port of the receiver")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--ready-timeout", type=float, default=30, help="how long the first probe waits for the port")
    parser.add_argument("--request-timeout", type=float, default=10)
    parser.add_argument("--dry-run", action="store_true", help="check the spec and print the requests")
    args = parser.parse_args(argv)
    if args.port is None and not args.dry_run:
        parser.error("--port is required")

    try:
        with open(args.spec, encoding="utf-8") as spec:
            probes = decode_spec(spec.read())
    except (OSError, SpecError) as error:
        print(f"spec error: {args.spec}: {error}", file=sys.stderr)
        return 2

    if args.dry_run:
        for probe in probes:
            print(f"{probe['id']}: {show_request(probe, body_of(probe['request'], probe['id']))}")
        print(f"otlp_http_probe side={args.side}: {len(probes)} probes, spec ok")
        return 0

    def send_probe(probe, first):
        request = probe["request"]
        body = body_of(request, probe["id"])
        if first:
            return send_when_ready(args.host, args.port, request, body, args.request_timeout, args.ready_timeout)
        return send(args.host, args.port, request, body, args.request_timeout)

    try:
        return run(probes, args.side, send_probe)
    except Exception as error:  # A crash of the script must not read as a failed probe, which exits with code 1.
        print(f"error: {type(error).__name__}: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
