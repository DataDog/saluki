"""Serve the Docker metadata used for ADP container-tag discovery.

Arguments are the target container ID and the Unix socket path. The Core Agent discovers
only that container; inspect requests for other IDs return 404. Its remote tagger supplies
ADP with the deterministic image and label tags asserted by the case.
"""

import json
import re
import signal
import socketserver
import sys
import threading
from http.server import BaseHTTPRequestHandler
from pathlib import Path

# Strip the client-selected API version before matching routes.
DOCKER_API_VERSION_PREFIX_REGEX = re.compile(r"^/v[0-9]+\.[0-9]+")
# The ping header lets clients negotiate instead of using their maximum version.
DOCKER_ADVERTISED_API_VERSION = "1.55"
DOCKER_IMAGES_PATH = "/images/json"
DOCKER_CONTAINERS_PATH = "/containers/json"
DOCKER_EVENTS_PATH = "/events"
DOCKER_INFO_PATH = "/info"


class ThreadingUnixHTTPServer(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True

    def __init__(self, *args, **kwargs):
        self.stopping = threading.Event()
        super().__init__(*args, **kwargs)

    def server_close(self):
        self.stopping.set()
        super().server_close()


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
        path = DOCKER_API_VERSION_PREFIX_REGEX.sub("", self.path.split("?", 1)[0])
        expected_container_id = self.server.expected_container_id
        if path == "/_ping":
            self.send_response(200)
            self.send_header("API-Version", DOCKER_ADVERTISED_API_VERSION)
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
        elif path == "/containers/" + expected_container_id + "/json":
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
            self.server.stopping.wait()
        else:
            self.send_not_found()

    def log_message(self, _format, *_args):
        pass


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: fake_docker.py <container-id> <socket-path>")
    container_id, socket_path = sys.argv[1:]
    socket_path = Path(socket_path)
    socket_path.unlink(missing_ok=True)
    server = ThreadingUnixHTTPServer(str(socket_path), DockerDiscoveryHandler)
    server.expected_container_id = container_id
    signal.signal(signal.SIGTERM, lambda _signum, _frame: server.stopping.set())
    signal.signal(signal.SIGINT, lambda _signum, _frame: server.stopping.set())
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        server.stopping.wait()
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=1)
        socket_path.unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
