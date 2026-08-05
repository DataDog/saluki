#!/opt/datadog-agent/embedded/bin/python3

"""Exercises ADP's DogStatsD vsock listener.

No official DogStatsD client speaks vsock yet, so this script is the client. vsock framing is
identical to a UDS stream listener: each message is prefixed with a 4-byte little-endian length
header and sent over one persistent connection.

The script asserts three things against the running listener:

1. Framed metrics, events, and service checks are ingested and counted under `listener_type:vsock`.
2. Unframed bytes are rejected as a framing error rather than being parsed or silently dropped.
3. A vsock peer produces no origin-detection error, since vsock carries no process credentials.
"""

import socket
import struct
import time
import urllib.request
from pathlib import Path

# VMADDR_CID_LOCAL. Matches `DD_DOGSTATSD_VSOCK=local:58126` in config.yaml, so the listener and
# this client agree on the loopback CID.
VMADDR_CID_LOCAL = 1
VSOCK_PORT = 58126

TELEMETRY_URL = "http://127.0.0.1:55100/metrics"
RESULT = Path("/tmp/dogstatsd-vsock-integration-result")

METRICS = [
    b"integration.vsock.counter:5|c|#source:vsock,env:test",
    b"integration.vsock.gauge:42.5|g|#source:vsock",
    b"integration.vsock.dist:1.5|d|#source:vsock",
]
EVENT = b"_e{21,11}:integration vsock evt|hello world|t:info"
SERVICE_CHECK = b"_sc|integration.vsock.check|0|#source:vsock"

# Deliberately not length-delimited. The framer reads the first four bytes as a length, which lands
# far beyond the buffer size, so the frame is rejected and the connection is torn down.
UNFRAMED = b"integration.vsock.unframed:1|c\n"


def frame(payload):
    """Length-delimit a DogStatsD message, exactly as the UDS stream framer expects."""
    return struct.pack("<I", len(payload)) + payload


def connect():
    client = socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM)
    client.settimeout(10)
    client.connect((VMADDR_CID_LOCAL, VSOCK_PORT))
    return client


def send(payloads, framed=True):
    with connect() as client:
        for payload in payloads:
            message = payload + b"\n"
            client.sendall(frame(message) if framed else message)
        # Close the write side so the handler sees EOF and drains anything still buffered.
        client.shutdown(socket.SHUT_WR)


def scrape_vsock_telemetry():
    """Returns every `listener_type="vsock"` sample from ADP's Prometheus endpoint, keyed by series."""
    with urllib.request.urlopen(TELEMETRY_URL, timeout=10) as response:
        body = response.read().decode("utf-8")

    samples = {}
    for line in body.splitlines():
        if line.startswith("#") or 'listener_type="vsock"' not in line:
            continue
        series, _, value = line.rpartition(" ")
        samples[series] = float(value)
    return samples


def find_sample(samples, metric, **labels):
    """Looks up one sample by metric name and a subset of its labels, or `0.0` if it isn't present.

    Label order in the exposition format is not guaranteed, so match on substrings rather than
    reconstructing the full series string. A missing series reads as zero rather than raising:
    ADP's internal telemetry state is rebuilt on an interval, so a series that exists in the
    process may not have reached `/metrics` yet, and callers poll until it does.
    """
    wanted = [f'{key}="{value}"' for key, value in labels.items()]
    matches = [
        value
        for series, value in samples.items()
        if series.startswith(f"adp__{metric}{{") and all(label in series for label in wanted)
    ]
    if len(matches) > 1:
        raise AssertionError(f"expected at most one {metric} sample matching {labels}, found {len(matches)}")
    return matches[0] if matches else 0.0


def wait_for(predicate, description, attempts=60, delay=1.0):
    """Polls telemetry until `predicate` holds, since counters are scraped asynchronously."""
    samples = {}
    for _ in range(attempts):
        samples = scrape_vsock_telemetry()
        if predicate(samples):
            return samples
        time.sleep(delay)
    raise AssertionError(f"timed out waiting for {description}; last vsock telemetry:\n{format_samples(samples)}")


def format_samples(samples):
    if not samples:
        return "  <no listener_type=\"vsock\" samples>"
    return "\n".join(f"  {series} {value}" for series, value in sorted(samples.items()))


def check_probe_reaches_the_listener():
    """Fails early, with a clear reason, when the environment can't do vsock at all.

    A host with no vsock transport loaded rejects the address family outright. That's an
    environment problem rather than an ADP regression, so name it as such.
    """
    try:
        connect().close()
    except OSError as e:
        raise AssertionError(
            f"could not connect to vsock://{VMADDR_CID_LOCAL}:{VSOCK_PORT} ({e}). "
            "This needs a vsock transport on the host (the `vsock_loopback` kernel module) and a "
            "container whose seccomp profile permits AF_VSOCK sockets."
        ) from e


def validate_framed_ingestion():
    send(METRICS + [EVENT, SERVICE_CHECK])

    samples = wait_for(
        lambda s: find_sample(s, "component_events_received_total", message_type="metrics") >= len(METRICS)
        and find_sample(s, "component_events_received_total", message_type="events") >= 1
        and find_sample(s, "component_events_received_total", message_type="service_checks") >= 1,
        "framed metrics, events, and service checks to be received over vsock",
    )

    if find_sample(samples, "component_bytes_received_total") <= 0:
        raise AssertionError(f"no bytes recorded against the vsock listener:\n{format_samples(samples)}")

    for message_type in ["metrics", "events", "service_checks"]:
        decode_errors = find_sample(samples, "component_errors_total", error_type="decode", message_type=message_type)
        if decode_errors != 0:
            raise AssertionError(f"vsock {message_type} failed to decode ({decode_errors}):\n{format_samples(samples)}")

    return samples


def validate_unframed_is_a_framing_error(before):
    framing_errors_before = find_sample(before, "component_errors_total", error_type="framing")

    send([UNFRAMED], framed=False)

    samples = wait_for(
        lambda s: find_sample(s, "component_errors_total", error_type="framing") > framing_errors_before,
        "unframed vsock data to be rejected as a framing error",
    )

    # The bogus frame must not have been parsed into an event on the way to being rejected.
    metrics_received = find_sample(samples, "component_events_received_total", message_type="metrics")
    if metrics_received != find_sample(before, "component_events_received_total", message_type="metrics"):
        raise AssertionError(f"unframed vsock data was parsed as a metric:\n{format_samples(samples)}")


def origin_detection_errors():
    """Total origin-detection errors across all listeners.

    This counter carries no `listener_type` label, so it can't be attributed to vsock directly.
    The test instead compares it before and after sending, and nothing else drives traffic into
    this container.
    """
    with urllib.request.urlopen(TELEMETRY_URL, timeout=10) as response:
        body = response.read().decode("utf-8")

    total = 0.0
    for line in body.splitlines():
        if line.startswith("#") or 'error_type="origin_detection"' not in line:
            continue
        _, _, value = line.rpartition(" ")
        total += float(value)
    return total


def main():
    check_probe_reaches_the_listener()

    # vsock carries no process credentials, so traffic over it must not be counted as an
    # origin-detection failure the way an unreadable UDS peer would be.
    origin_errors_before = origin_detection_errors()

    after_framed = validate_framed_ingestion()
    validate_unframed_is_a_framing_error(after_framed)

    origin_errors_after = origin_detection_errors()
    if origin_errors_after != origin_errors_before:
        raise AssertionError(
            f"vsock traffic produced origin-detection errors ({origin_errors_before} -> {origin_errors_after})"
        )

    RESULT.write_text("passed\n", encoding="utf-8")


if __name__ == "__main__":
    main()
