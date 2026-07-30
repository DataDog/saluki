import gzip
import json
import os
import re
import signal
import socketserver
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOSTNAME = "integration-test-liveness-container-tags"
INTAKE_ADDRESS = ("127.0.0.1", 18080)
RUNNING_METRIC = "datadog.agent_data_plane.running"
UP_SERVICE_CHECK = "datadog.agent_data_plane.up"
RUNNING_MARKER = "/tmp/adp-liveness-running-validated"
UP_MARKER = "/tmp/adp-liveness-up-validated"
REQUESTS_PATH = "/tmp/adp-liveness-intake-requests.jsonl"
ERROR_PATH = "/tmp/adp-liveness-intake-error"
DOCKER_SOCKET_PATH = "/tmp/adp-liveness-docker.sock"
DOCKER_API_PREFIX = "/v1.54"
DOCKER_IMAGES_PATH = DOCKER_API_PREFIX + "/images/json"
DOCKER_CONTAINERS_PATH = DOCKER_API_PREFIX + "/containers/json"
DOCKER_EVENTS_PATH = DOCKER_API_PREFIX + "/events"
DOCKER_INFO_PATH = DOCKER_API_PREFIX + "/info"
EXPECTED_CONTAINER_TAGS = (
    "docker_image:example/adp-liveness:1.0",
    "env:liveness-fake",
)
CURRENT_TIMESTAMP_TOLERANCE_SECS = 30

agent_process = None
lock = threading.Lock()


def remove_artifacts():
    for path in (RUNNING_MARKER, UP_MARKER, REQUESTS_PATH, ERROR_PATH):
        try:
            os.remove(path)
        except FileNotFoundError:
            # These marker and request artifacts are absent before the fixture starts.
            pass


def has_container_tag(tags):
    return all(expected_tag in tags for expected_tag in EXPECTED_CONTAINER_TAGS)


def has_version_tag(tags):
    return any(isinstance(tag, str) and len(tag) > len("version:") and tag.startswith("version:") for tag in tags)


def has_current_gauge_point(points):
    now = time.time()
    for point in points:
        if not isinstance(point, list) or len(point) != 2:
            continue
        timestamp, value = point
        if not isinstance(timestamp, (int, float)) or timestamp <= 0:
            continue
        if abs(now - timestamp) > CURRENT_TIMESTAMP_TOLERANCE_SECS:
            continue
        if value == 1:
            return True
    return False


def write_marker(path, message):
    with open(path, "w", encoding="utf-8") as marker_file:
        marker_file.write(message + "\n")


def validate_series(payload):
    for series in payload.get("series", []):
        if series.get("metric") != RUNNING_METRIC:
            continue

        tags = series.get("tags", [])
        if (
            series.get("host") == HOSTNAME
            and series.get("type") == "gauge"
            and has_current_gauge_point(series.get("points", []))
            and has_version_tag(tags)
            and has_container_tag(tags)
        ):
            write_marker(RUNNING_MARKER, "validated ADP running gauge with version and container tag")
            return


def validate_service_checks(payload):
    if not isinstance(payload, list):
        return

    for service_check in payload:
        if service_check.get("check") != UP_SERVICE_CHECK:
            continue

        tags = service_check.get("tags", [])
        if service_check.get("host_name") == HOSTNAME and service_check.get("status") == 0 and has_container_tag(tags):
            write_marker(UP_MARKER, "validated ADP up service check with container tag")
            return


def get_self_container_id():
    with open("/proc/self/cgroup", encoding="utf-8") as cgroup_file:
        for line in cgroup_file:
            cgroup_name = line.rstrip().rsplit("/", 1)[-1]
            match = re.search(r"[0-9a-f]{64}", cgroup_name)
            if match:
                return match.group(0)
    raise RuntimeError("could not resolve the target container ID from /proc/self/cgroup")


def decode_request_body(headers, body):
    content_encoding = headers.get("Content-Encoding", "").lower()
    if content_encoding == "gzip":
        return gzip.decompress(body)
    if content_encoding:
        raise ValueError("unsupported content encoding: " + content_encoding)
    return body


class IntakeHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        try:
            content_length = int(self.headers.get("Content-Length", "0"))
            body = decode_request_body(self.headers, self.rfile.read(content_length))
            payload = json.loads(body.decode("utf-8"))
            with lock:
                with open(REQUESTS_PATH, "a", encoding="utf-8") as requests_file:
                    requests_file.write(json.dumps({"path": self.path, "payload": payload}) + "\n")
                if self.path == "/api/v1/series":
                    validate_series(payload)
                elif self.path == "/api/v1/check_run":
                    validate_service_checks(payload)
        except Exception as error:
            with lock:
                with open(ERROR_PATH, "a", encoding="utf-8") as error_file:
                    error_file.write(str(error) + "\n")
        finally:
            self.send_response(202)
            self.end_headers()

    def log_message(self, _format, *_args):
        pass


class ReusableThreadingHTTPServer(ThreadingHTTPServer):
    allow_reuse_address = True


class ThreadingUnixHTTPServer(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True


class DockerDiscoveryHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def send_json(self, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_not_found(self):
        self.send_response(404)
        self.send_header("Content-Length", "0")
        self.end_headers()

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        expected_container_id = self.server.expected_container_id
        if path == "/_ping":
            self.send_response(200)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"OK")
        elif path == DOCKER_IMAGES_PATH:
            self.send_json([])
        elif path == DOCKER_CONTAINERS_PATH:
            self.send_json(
                [
                    {
                        "Id": expected_container_id,
                        "Names": ["/adp-liveness-fake"],
                        "Image": "example/adp-liveness:1.0",
                        "ImageID": "sha256:" + "a" * 64,
                        "Labels": {"com.datadoghq.tags.env": "liveness-fake"},
                        "State": "running",
                        "Status": "Up",
                    }
                ]
            )
        elif path == DOCKER_API_PREFIX + "/containers/" + expected_container_id + "/json":
            self.send_json(
                {
                    "Id": expected_container_id,
                    "Name": "/adp-liveness-fake",
                    "Created": "2026-01-01T00:00:00.000000000Z",
                    "Image": "sha256:" + "a" * 64,
                    "Config": {
                        "Hostname": "adp-liveness-fake",
                        "Image": "example/adp-liveness:1.0",
                        "Labels": {"com.datadoghq.tags.env": "liveness-fake"},
                    },
                    "State": {"Status": "running", "Running": True, "Pid": 1},
                    "HostConfig": {"NetworkMode": "default"},
                    "NetworkSettings": {"Networks": {}},
                }
            )
        elif path == DOCKER_INFO_PATH:
            self.send_json({})
        elif path == DOCKER_EVENTS_PATH:
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            while agent_process is not None and agent_process.poll() is None:
                time.sleep(0.1)
        else:
            self.send_not_found()

    def log_message(self, _format, *_args):
        pass


def handle_signal(_signum, _frame):
    if agent_process is not None and agent_process.poll() is None:
        agent_process.terminate()


def main():
    global agent_process

    remove_artifacts()
    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    server = ReusableThreadingHTTPServer(INTAKE_ADDRESS, IntakeHandler)
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()

    try:
        os.remove(DOCKER_SOCKET_PATH)
    except FileNotFoundError:
        # The fake socket does not exist before the fixture binds it.
        pass
    expected_container_id = get_self_container_id()
    docker_server = ThreadingUnixHTTPServer(DOCKER_SOCKET_PATH, DockerDiscoveryHandler)
    docker_server.expected_container_id = expected_container_id
    docker_thread = threading.Thread(target=docker_server.serve_forever, daemon=True)
    docker_thread.start()

    try:
        agent_process = subprocess.Popen(["/bin/entrypoint.sh"])
        return agent_process.wait()
    finally:
        docker_server.shutdown()
        docker_server.server_close()
        docker_thread.join(timeout=1)
        try:
            os.remove(DOCKER_SOCKET_PATH)
        except FileNotFoundError:
            # The fake socket may already be absent after server teardown.
            pass
        server.shutdown()
        server.server_close()
        server_thread.join(timeout=1)


if __name__ == "__main__":
    sys.exit(main())
