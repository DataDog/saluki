#!/opt/datadog-agent/embedded/bin/python3

import json
import shutil
import socket
import ssl
import stat
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path

ADP = Path("/opt/datadog-agent/embedded/bin/agent-data-plane")
AGENT = Path("/opt/datadog-agent/bin/agent/agent")
CONFIG = Path("/etc/datadog-agent/datadog.yaml")
AUTH_TOKEN = Path("/etc/datadog-agent/auth_token")
API_URL = "https://127.0.0.1:55101/dogstatsd/contexts/dump"
DUMP_FILENAME = "dogstatsd_contexts.json.zstd"
COPIED_DUMP = Path("/tmp/dogstatsd-top-copied.json.zstd")
ONLINE_OUTPUT = Path("/tmp/dogstatsd-top-online-output")
OFFLINE_OUTPUT = Path("/tmp/dogstatsd-top-offline-output")
AGENT_OUTPUT = Path("/tmp/dogstatsd-top-agent-output")
RESULT = Path("/tmp/dogstatsd-top-integration-result")

REQUEST_ROW = "          2\tintegration.top.requests\t(2 instance, 1 env, 1 service)"
QUEUE_ROW = "          1\tintegration.top.queue\t(1 queue)"


def run(command):
    completed = subprocess.run(command, text=True, capture_output=True, timeout=30)
    if completed.returncode != 0:
        raise RuntimeError(
            f"command failed ({completed.returncode}): {' '.join(map(str, command))}\n"
            f"stdout:\n{completed.stdout}\nstderr:\n{completed.stderr}"
        )
    return completed.stdout


def run_adp(*arguments):
    return run([str(ADP), "--config", str(CONFIG), "dogstatsd", *arguments])


def unverified_tls_context():
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    return context


def post_dump(authorization=None):
    headers = {}
    if authorization is not None:
        headers["Authorization"] = f"Bearer {authorization}"
    request = urllib.request.Request(API_URL, data=b"", headers=headers, method="POST")
    return urllib.request.urlopen(request, context=unverified_tls_context(), timeout=10)


def assert_unauthorized_request():
    try:
        post_dump()
    except urllib.error.HTTPError as error:
        body = error.read().decode("utf-8")
        if error.code != 401 or body != "Authentication required.":
            raise AssertionError(f"unexpected unauthenticated response: {error.code} {body!r}")
    else:
        raise AssertionError("context dump API accepted an unauthenticated request")


def send_dogstatsd_contexts():
    payload = b"\n".join(
        [
            b"integration.top.requests:1|c|#host:host-a,env:prod,service:web,instance:a",
            b"integration.top.requests:1|c|#host:host-b,env:prod,service:web,instance:b",
            b"integration.top.queue:1|c|#queue:alpha",
        ]
    )
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as client:
        for _ in range(3):
            client.sendto(payload, ("127.0.0.1", 58125))


def wait_for_online_report():
    last_output = ""
    for _ in range(40):
        last_output = run_adp("top")
        if REQUEST_ROW in last_output and QUEUE_ROW in last_output:
            ONLINE_OUTPUT.write_text(last_output, encoding="utf-8")
            return last_output
        time.sleep(0.5)
    raise AssertionError(f"retained contexts did not appear in online output:\n{last_output}")


def parse_written_path(output):
    lines = output.splitlines()
    if len(lines) != 1 or not lines[0].startswith("Wrote "):
        raise AssertionError(f"unexpected dump-contexts output: {output!r}")
    path = Path(lines[0].removeprefix("Wrote "))
    if not path.is_absolute() or path.name != DUMP_FILENAME:
        raise AssertionError(f"unexpected context dump path: {path}")
    if not path.is_file():
        raise AssertionError(f"context dump was not created: {path}")
    return path


def normalize_report(output):
    return output.replace("\r\n", "\n").rstrip("\n")


def validate_online_api(token):
    with post_dump(token) as response:
        body = response.read().decode("utf-8")
        if response.status != 200:
            raise AssertionError(f"authenticated API returned {response.status}: {body}")
        if not response.headers.get_content_type() == "application/json":
            raise AssertionError(f"unexpected API content type: {response.headers.get('Content-Type')}")
    path = Path(json.loads(body))
    if path.name != DUMP_FILENAME or not path.is_file():
        raise AssertionError(f"authenticated API returned an invalid artifact path: {path}")


def validate_dump_and_offline_interoperability():
    dump_path = parse_written_path(run_adp("dump-contexts"))
    if stat.S_IMODE(dump_path.stat().st_mode) != 0o600:
        raise AssertionError(f"context dump mode is not 0600: {oct(stat.S_IMODE(dump_path.stat().st_mode))}")

    shutil.copyfile(dump_path, COPIED_DUMP)
    offline = run_adp("top", "--path", str(COPIED_DUMP))
    OFFLINE_OUTPUT.write_text(offline, encoding="utf-8")
    if REQUEST_ROW not in offline or QUEUE_ROW not in offline:
        raise AssertionError(f"offline report is missing retained contexts:\n{offline}")

    agent = run([str(AGENT), "dogstatsd", "top", "--path", str(COPIED_DUMP)])
    AGENT_OUTPUT.write_text(agent, encoding="utf-8")
    if normalize_report(agent) != normalize_report(offline):
        raise AssertionError(f"Agent and ADP reports differ:\nADP:\n{offline}\nAgent:\n{agent}")


def main():
    for required_path in [ADP, AGENT, CONFIG, AUTH_TOKEN]:
        if not required_path.exists():
            raise AssertionError(f"required test path does not exist: {required_path}")

    assert_unauthorized_request()
    send_dogstatsd_contexts()
    online = wait_for_online_report()
    if not online.startswith("Wrote "):
        raise AssertionError(f"online top did not print the generated artifact path:\n{online}")

    token = AUTH_TOKEN.read_text(encoding="utf-8")
    validate_online_api(token)
    validate_dump_and_offline_interoperability()
    RESULT.write_text("passed\n", encoding="utf-8")


if __name__ == "__main__":
    main()
