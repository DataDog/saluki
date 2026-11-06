#!/opt/datadog-agent/embedded/bin/python3

import socket
import subprocess
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

AGENT = Path("/opt/datadog-agent/bin/agent/agent")
REMOTE_STATS = [str(AGENT), "remote", "dogstatsd", "stats"]
DOGSTATSD_ADDRESS = ("127.0.0.1", 48125)
RESULT = Path("/tmp/dogstatsd-remote-commands-integration-result")

# The sum of these budgets must stay below the case's `target_exec` timeout.
READY_TIMEOUT_SECS = 60
READY_ATTEMPT_TIMEOUT_SECS = 10
COMMAND_TIMEOUT_SECS = 30
SENDER_JOIN_TIMEOUT_SECS = 5

COLLECTION_SECS = 3
SEND_INTERVAL_SECS = 0.1
PAYLOAD = b"\n".join(
    [
        b"integration.remote.requests:1|c|#instance:a",
        b"integration.remote.requests:1|c|#instance:b",
        b"integration.remote.queue:1|c|#queue:alpha",
        b"integration.decoy:1|c|#instance:c",
    ]
)
HELP_FLAGS = ["--duration-secs", "--mode", "--filter"]
DURATION_LIMIT_ERROR = "Collection duration cannot be greater than 600 seconds."
EXPECTED_TABLE = [
    ("Metric", "Unique Contexts", "Highest Cardinality Tags (top 5)"),
    ("integration.remote.requests", "2", "instance (2)"),
    ("integration.remote.queue", "1", "queue (1)"),
]


class CommandTimeout(AssertionError):
    pass


def as_text(output):
    return output.decode("utf-8", "replace") if isinstance(output, bytes) else output or ""


def describe(command, stdout, stderr, exit_code=None):
    return (
        f"command: {' '.join(command)}\nexit code: {exit_code}\n"
        f"stdout:\n{as_text(stdout)}\nstderr:\n{as_text(stderr)}"
    )


def run_remote_stats(arguments, timeout):
    command = [*REMOTE_STATS, *arguments]
    try:
        return subprocess.run(command, stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=timeout)
    except subprocess.TimeoutExpired as error:
        details = describe(command, error.stdout, error.stderr)
        raise CommandTimeout(f"timed out after {timeout:.1f}s\n{details}") from None


def check(condition, message, completed):
    if not condition:
        details = describe(completed.args, completed.stdout, completed.stderr, completed.returncode)
        raise AssertionError(f"{message}\n{details}")


def wait_for_remote_stats_command():
    deadline = time.monotonic() + READY_TIMEOUT_SECS
    last_attempt = "no attempt completed"
    while (remaining := deadline - time.monotonic()) > 0:
        try:
            completed = run_remote_stats(["--help"], min(READY_ATTEMPT_TIMEOUT_SECS, remaining))
        except CommandTimeout as error:
            last_attempt = str(error)
        else:
            flags_listed = all(flag in completed.stdout + completed.stderr for flag in HELP_FLAGS)
            if completed.returncode == 0 and flags_listed:
                return
            last_attempt = describe(completed.args, completed.stdout, completed.stderr, completed.returncode)
        time.sleep(max(0.0, min(1.0, deadline - time.monotonic())))
    raise AssertionError(f"remote stats command was not available after {READY_TIMEOUT_SECS}s:\n{last_attempt}")


def check_rejected_duration():
    completed = run_remote_stats(["--duration-secs", "601"], COMMAND_TIMEOUT_SECS)
    check(completed.returncode == 1, "rejected duration did not exit with code 1", completed)
    check("over the next 601 seconds" in completed.stdout, "missing progress output", completed)
    check(DURATION_LIMIT_ERROR in completed.stderr, "missing duration limit error on stderr", completed)
    check(DURATION_LIMIT_ERROR not in completed.stdout, "duration limit error was written to stdout", completed)


def send_payload(stopping):
    """Sends the payload repeatedly so it spans the whole collection window."""
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
        client.settimeout(1.0)
        while not stopping.is_set():
            client.sendto(PAYLOAD, DOGSTATSD_ADDRESS)
            stopping.wait(SEND_INTERVAL_SECS)


def table_rows(stdout):
    rows = []
    for line in map(str.strip, stdout.splitlines()):
        if len(line) > 1 and line.startswith("|") and line.endswith("|"):
            rows.append(tuple(cell.strip() for cell in line[1:-1].split("|")))
    return rows


def check_cardinality_report():
    stopping = threading.Event()
    executor = ThreadPoolExecutor(max_workers=1)
    sender = executor.submit(send_payload, stopping)
    try:
        completed = run_remote_stats(
            ["--duration-secs", str(COLLECTION_SECS), "--mode", "cardinality", "--filter", "integration.remote."],
            COMMAND_TIMEOUT_SECS,
        )
    finally:
        stopping.set()
        executor.shutdown(wait=False)
    try:
        sender.result(timeout=SENDER_JOIN_TIMEOUT_SECS)
    except Exception as error:
        # A sender that has not stopped raises TimeoutError here; a failed sender re-raises its exception.
        check(False, f"DogStatsD sender failed or did not stop: {error!r}", completed)

    check(completed.returncode == 0, "cardinality stats did not exit with code 0", completed)
    check(f"over the next {COLLECTION_SECS} seconds" in completed.stdout, "missing progress output", completed)
    check("integration.decoy" not in completed.stdout, "filtered metric appeared in the report", completed)
    rows = table_rows(completed.stdout)
    check(rows == EXPECTED_TABLE, f"unexpected cardinality table rows: {rows}", completed)


def main():
    wait_for_remote_stats_command()
    check_rejected_duration()
    check_cardinality_report()
    RESULT.write_text("passed\n", encoding="utf-8")


if __name__ == "__main__":
    main()
