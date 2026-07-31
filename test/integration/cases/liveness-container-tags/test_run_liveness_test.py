import http.client
import importlib.util
import socket
import tempfile
import threading
import unittest
from pathlib import Path


FIXTURE_PATH = Path(__file__).with_name("run_liveness_test.py")
EXPECTED_CONTAINER_ID = "a" * 64
spec = importlib.util.spec_from_file_location("run_liveness_test", FIXTURE_PATH)
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class DockerDiscoveryHandlerTest(unittest.TestCase):
    def test_inspect_for_unknown_container_returns_not_found(self):
        with tempfile.TemporaryDirectory() as temporary_directory:
            socket_path = str(Path(temporary_directory) / "docker.sock")
            server = fixture.ThreadingUnixHTTPServer(socket_path, fixture.DockerDiscoveryHandler)
            server.expected_container_id = EXPECTED_CONTAINER_ID
            server_thread = threading.Thread(target=server.serve_forever, daemon=True)
            server_thread.start()
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                    client.connect(socket_path)
                    client.sendall(
                        b"GET /v1.54/containers/"
                        + b"b" * 64
                        + b"/json HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n"
                    )
                    response = http.client.HTTPResponse(client)
                    response.begin()
                    self.assertEqual(response.status, 404)
            finally:
                server.shutdown()
                server.server_close()
                server_thread.join(timeout=1)


if __name__ == "__main__":
    unittest.main()
