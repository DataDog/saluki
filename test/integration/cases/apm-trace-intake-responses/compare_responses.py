#!/opt/datadog-agent/embedded/bin/python3
"""Sends the same trace intake requests to ADP's native intake and to the trace-agent, and compares the answers.

ADP serves the trace endpoints on 8126 and the trace-agent, in the same container and with the same configuration,
listens on 8127. Every request goes to both, and their status, headers and body must match, except for:

- `Date`, which is the time of the answer.
- `Datadog-Agent-State`, the hash of the `/info` body: ADP serves `/info` itself, so it hashes its own body (see the
  `apm-info-endpoint` case).
- The value of `Datadog-Agent-Version`, which is only checked to be there (see `PRESENCE_ONLY_HEADERS`).
- The body of a `400` for a payload that doesn't decode, which is the decoder's own error message (Go's
  `httpDecodingError` writes `err.Error()`). Its status and headers but `Content-Length` are still compared. The
  fixed messages (`unexpected-eof`, `msgp: too few bytes left to read object`, `payload-too-large`) are compared.

Usage: compare_responses.py default|feature-429. `default` covers the error paths and the overload answers, with
`apm_config.features` unset; `feature-429` covers the overload answers with feature `429`. Writes `passed` to RESULT
when every answer matches.
"""

import http.client
import socket
import sys
import time
from pathlib import Path

ADP = ("127.0.0.1", 8126)
TRACE_AGENT = ("127.0.0.1", 8127)
RESULT = Path("/tmp/apm-trace-intake-responses-result")

# `DD_APM_MAX_PAYLOAD_SIZE` in the case's configuration.
MAX_PAYLOAD_SIZE = 4096

MSGPACK = {"Content-Type": "application/msgpack"}
JSON = {"Content-Type": "application/json"}
TRACER = {"Datadog-Meta-Lang": "python", "Datadog-Meta-Tracer-Version": "3.0.0", "X-Datadog-Trace-Count": "1"}

IGNORED_HEADERS = {"date", "datadog-agent-state"}

# Headers compared by presence only. ADP runs standalone here, as it can't otherwise take over the trace-agent's
# receiver, so it has no Core Agent to learn the Agent's version from, and answers with an empty
# `Datadog-Agent-Version`. Converged, it answers with the Core Agent's version (see the `apm-info-endpoint` case).
PRESENCE_ONLY_HEADERS = {"datadog-agent-version"}

# The smallest valid payload of each endpoint: no traces, so that the rates the trace-agent and ADP answer with stay
# the defaults.
EMPTY_PAYLOADS = [
    ("/v0.1/spans", JSON, b"[]"),
    ("/spans", JSON, b"[]"),
    ("/v0.2/traces", JSON, b"[]"),
    ("/v0.3/traces", MSGPACK, b"\x90"),
    ("/v0.3/traces", JSON, b"[]"),
    ("/v0.4/traces", MSGPACK, b"\x90"),
    ("/v0.4/traces", JSON, b"[]"),
    ("/v0.5/traces", MSGPACK, b"\x92\x90\x90"),
    ("/v0.7/traces", MSGPACK, b"\x80"),
    ("/v1.0/traces", MSGPACK, b"\x80"),
]


class Answer:
    def __init__(self, status, headers, body):
        self.status = status
        self.headers = headers
        self.body = body

    def __repr__(self):
        return f"{self.status} {self.headers} {self.body!r}"


def send(address, path, headers, body, chunked=False, timeout=10):
    """Sends one request on a fresh connection and reads the whole answer."""
    connection = http.client.HTTPConnection(*address, timeout=timeout)
    try:
        connection.putrequest("POST", path, skip_accept_encoding=True)
        for name, value in headers.items():
            connection.putheader(name, value)
        if chunked:
            connection.putheader("Transfer-Encoding", "chunked")
            connection.endheaders()
            for start in range(0, len(body), 1024):
                piece = body[start : start + 1024]
                connection.send(b"%x\r\n%s\r\n" % (len(piece), piece))
            connection.send(b"0\r\n\r\n")
        else:
            if "Content-Length" not in headers:
                connection.putheader("Content-Length", str(len(body)))
            connection.endheaders()
            connection.send(body)
        response = connection.getresponse()
        answer_headers = sorted(
            (name.lower(), "<present>" if name.lower() in PRESENCE_ONLY_HEADERS else value)
            for name, value in response.getheaders()
            if name.lower() not in IGNORED_HEADERS
        )
        return Answer(response.status, answer_headers, response.read())
    finally:
        connection.close()


def wait_until_ready():
    deadline = time.monotonic() + 90
    for address in (ADP, TRACE_AGENT):
        while True:
            try:
                if send(address, "/v0.4/traces", MSGPACK, b"\x90", timeout=2).status == 200:
                    break
            except OSError:
                pass
            if time.monotonic() > deadline:
                raise RuntimeError(f"{address} never accepted an empty /v0.4/traces payload")
            time.sleep(1)


class Comparison:
    def __init__(self):
        self.failures = []
        self.count = 0

    def check(self, name, adp, trace_agent, expected_status, compare_body=True):
        self.count += 1
        problems = []
        if adp.status != trace_agent.status:
            problems.append("status differs")
        if adp.status != expected_status:
            problems.append(f"expected status {expected_status}")
        # The length of a body that isn't compared isn't either.
        headers = lambda answer: [h for h in answer.headers if compare_body or h[0] != "content-length"]
        if headers(adp) != headers(trace_agent):
            problems.append("headers differ")
        if compare_body and adp.body != trace_agent.body:
            problems.append("body differs")
        verdict = "FAIL " + ", ".join(problems) if problems else "ok"
        print(f"[{verdict}] {name}\n    adp:         {adp!r}\n    trace-agent: {trace_agent!r}", flush=True)
        if problems:
            self.failures.append(name)

    def same(self, name, path, headers, body, expected_status, chunked=False, compare_body=True):
        adp = send(ADP, path, headers, body, chunked)
        trace_agent = send(TRACE_AGENT, path, headers, body, chunked)
        self.check(name, adp, trace_agent, expected_status, compare_body)


def error_cases(comparison):
    for path, headers, body in EMPTY_PAYLOADS:
        comparison.same(f"accepted {path} {headers['Content-Type']}", path, {**TRACER, **headers}, body, 200)
    comparison.same(
        "accepted /v0.4/traces with a stale Datadog-Rates-Payload-Version",
        "/v0.4/traces",
        {**TRACER, **MSGPACK, "Datadog-Rates-Payload-Version": "stale"},
        b"\x90",
        200,
    )

    # 415: `handleWithVersion` rejects MessagePack below v0.3 (`httpFormatError`).
    for path in ("/v0.1/spans", "/spans", "/v0.2/traces"):
        comparison.same(f"415 msgpack on {path}", path, {**TRACER, **MSGPACK}, b"\x90", 415)
    comparison.same(
        "415 msgpack with parameters on /v0.2/traces",
        "/v0.2/traces",
        {**TRACER, "Content-Type": "application/msgpack; charset=utf-8"},
        b"\x90",
        415,
    )

    # 403: `handleWithVersion` rejects cross-site browser requests.
    for path, headers, body in EMPTY_PAYLOADS:
        comparison.same(
            f"403 cross-site on {path}", path, {**TRACER, **headers, "Sec-Fetch-Site": "cross-site"}, body, 403
        )
    comparison.same(
        "same-site is accepted", "/v0.4/traces", {**TRACER, **MSGPACK, "Sec-Fetch-Site": "same-site"}, b"\x90", 200
    )

    # 413: a `Content-Length` over the limit (`reserveBodySize`), and a body that reaches it (`LimitedReader`).
    big_json = b"[" + b",".join([b"[]"] * (MAX_PAYLOAD_SIZE // 2)) + b"]"
    big_msgpack = b"\xdc" + (MAX_PAYLOAD_SIZE * 2).to_bytes(2, "big") + b"\x90" * (MAX_PAYLOAD_SIZE * 2)
    for path, headers, body in [
        ("/v0.4/traces", MSGPACK, big_msgpack),
        ("/v0.4/traces", JSON, big_json),
        ("/v0.5/traces", MSGPACK, b"\x92\x90" + big_msgpack),
        ("/v0.7/traces", MSGPACK, big_msgpack),
        ("/v1.0/traces", MSGPACK, big_msgpack),
        ("/v0.1/spans", JSON, big_json),
    ]:
        name = f"{path} {headers['Content-Type']}"
        comparison.same(f"413 Content-Length on {name}", path, {**TRACER, **headers}, body, 413)
        comparison.same(
            f"413 chunked body on {name}",
            path,
            {**TRACER, **headers},
            body,
            413,
            chunked=True,
        )

    # 400 with a fixed message: the body ends early (`unexpected-eof`), or a MessagePack value is cut short
    # (`msgp.ErrShortBytes`).
    for name, path, headers, body in [
        ("unexpected-eof, truncated JSON", "/v0.4/traces", JSON, b'[[{"service":"a"'),
        ("unexpected-eof, empty JSON", "/v0.4/traces", JSON, b""),
        ("unexpected-eof, truncated v0.1 JSON", "/v0.1/spans", JSON, b'[{"service":"a"'),
        ("unexpected-eof, truncated v0.2 JSON", "/v0.2/traces", JSON, b"[["),
        ("msgp_short_bytes, v0.4", "/v0.4/traces", MSGPACK, b"\x91\xdc\x00"),
        ("msgp_short_bytes, empty v0.4", "/v0.4/traces", MSGPACK, b""),
        ("msgp_short_bytes, v0.3", "/v0.3/traces", MSGPACK, b"\x91\x91\x81\xa7servic"),
        ("msgp_short_bytes, v0.5", "/v0.5/traces", MSGPACK, b"\x92\x91\xa3abc"),
        ("msgp_short_bytes, v0.7", "/v0.7/traces", MSGPACK, b"\x81\xa6chunks\x91"),
        ("msgp_short_bytes, v1.0", "/v1.0/traces", MSGPACK, b"\x84\x03\xa2go"),
    ]:
        comparison.same(f"400 {name}", path, {**TRACER, **headers}, body, 400)

    # 400 for a payload that doesn't decode: only the status and headers, as the body is the decoder's message.
    for name, path, headers, body in [
        ("decoding-error, JSON object", "/v0.4/traces", JSON, b"{}"),
        ("decoding-error, invalid JSON", "/v0.4/traces", JSON, b"} invalid json"),
        ("decoding-error, JSON string span ID", "/v0.4/traces", JSON, b'[[{"span_id":"52"}]]'),
        ("decoding-error, v0.1 JSON object", "/v0.1/spans", JSON, b"{}"),
        ("decoding-error, msgpack bool", "/v0.4/traces", MSGPACK, b"\xc3"),
        ("decoding-error, v0.5 not an array", "/v0.5/traces", MSGPACK, b"\x80"),
        ("decoding-error, v0.5 dictionary reference out of range", "/v0.5/traces", MSGPACK,
         b"\x92\x90\x91\x91\x9c" + b"\x05" * 12),
        ("decoding-error, v0.7 not a map", "/v0.7/traces", MSGPACK, b"\x90"),
        ("decoding-error, v1.0 not a map", "/v1.0/traces", MSGPACK, b"\x90"),
        ("decoding-error, v1.0 ends where a string is due", "/v1.0/traces", MSGPACK, b"\x81\x02"),
    ]:
        comparison.same(f"400 {name}", path, {**TRACER, **headers}, body, 400, compare_body=False)


class DecoderHolder:
    """Holds the only decoder of a server with a request whose body never finishes arriving."""

    def __init__(self, address):
        self.socket = socket.create_connection(address)
        self.socket.sendall(
            b"POST /v0.4/traces HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/msgpack\r\n"
            b"Content-Length: 100\r\n\r\n\x90"
        )
        # Leave the server time to hand the request to its handler, which takes the decoder before reading the body.
        time.sleep(1)

    def close(self):
        self.socket.close()


def overload_cases(comparison, feature_429):
    refused_status = 429 if feature_429 else 200
    for path, headers, body in EMPTY_PAYLOADS:
        for name, extra, status in [
            ("", {}, refused_status),
            (" with Datadog-Send-Real-Http-Status", {"Datadog-Send-Real-Http-Status": "true"}, 429),
        ]:
            answers = []
            for address in (ADP, TRACE_AGENT):
                holder = DecoderHolder(address)
                try:
                    answers.append(send(address, path, {**TRACER, **headers, **extra}, body))
                finally:
                    holder.close()
                # Let the held request fail and release the decoder.
                time.sleep(0.5)
            comparison.check(f"refused {path} {headers['Content-Type']}{name}", *answers, status)


def main():
    mode = sys.argv[1]
    wait_until_ready()
    comparison = Comparison()
    if mode == "default":
        error_cases(comparison)
        overload_cases(comparison, feature_429=False)
    elif mode == "feature-429":
        overload_cases(comparison, feature_429=True)
    else:
        raise SystemExit(f"unknown mode {mode}")
    if comparison.failures:
        print(f"{len(comparison.failures)} of {comparison.count} answers differ: {comparison.failures}")
        return 1
    print(f"All {comparison.count} answers match.")
    RESULT.write_text("passed\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
