import http.client
import importlib.util
import socket
import tempfile
import threading
import unittest
from pathlib import Path


FIXTURE_PATH = Path(__file__).with_name("run_liveness_test.py")
EXPECTED_CONTAINER_ID = "a" * 64
UNKNOWN_CONTAINER_ID = "b" * 64
spec = importlib.util.spec_from_file_location("run_liveness_test", FIXTURE_PATH)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class DockerDiscoveryHandlerTest(unittest.TestCase):
    def setUp(self):
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary_directory.cleanup)
        socket_path = str(Path(self.temporary_directory.name) / "docker.sock")
        self.server = fixture.ThreadingUnixHTTPServer(socket_path, fixture.DockerDiscoveryHandler)
        self.server.expected_container_id = EXPECTED_CONTAINER_ID
        server_thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        server_thread.start()
        self.addCleanup(server_thread.join, 1)
        self.addCleanup(self.server.server_close)
        self.addCleanup(self.server.shutdown)
        self.socket_path = socket_path

    def get(self, path):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.connect(self.socket_path)
            request = "GET " + path + " HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n"
            client.sendall(request.encode("ascii"))
            response = http.client.HTTPResponse(client)
            response.begin()
            response.read()
            return response

    def test_inspect_for_unknown_container_returns_not_found(self):
        response = self.get("/v1.54/containers/" + UNKNOWN_CONTAINER_ID + "/json")
        self.assertEqual(response.status, 404)

    def test_routes_ignore_the_client_api_version(self):
        # The Docker client picks the version prefix, and raises it as the Agent upgrades.
        for prefix in ("", "/v1.40", "/v1.54", "/v1.55", "/v2.0"):
            for route in ("/containers/" + EXPECTED_CONTAINER_ID + "/json", "/containers/json", "/images/json"):
                with self.subTest(prefix=prefix, route=route):
                    self.assertEqual(self.get(prefix + route).status, 200)

    def test_ping_advertises_an_api_version(self):
        # Without this header the client skips negotiation and requests its own maximum version.
        response = self.get("/_ping")
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader("API-Version"), fixture.DOCKER_ADVERTISED_API_VERSION)


if __name__ == "__main__":
    unittest.main()
