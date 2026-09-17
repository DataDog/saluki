"""Experimental loopback fault proxy for test-stateful-metrics-binaries.py.

Healthy connections relay bytes to the actual Foldspace intake. Injected failures
test client recovery; they do not model production server load shedding.
"""

import select
import socket
import threading

from h2.config import H2Configuration
from h2.connection import H2Connection
from h2.events import RequestReceived


class FaultProxy:
    def __init__(self, upstream_port):
        self.upstream_port = upstream_port
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.port = self.listener.getsockname()[1]
        self.listener.listen()
        self.listener.settimeout(0.1)
        self.closed = threading.Event()
        self.pause_reads = threading.Event()
        self.pause_acks = threading.Event()
        self.reject = False
        self.rejected = 0
        self.connections = 0
        self.up_bytes = 0
        self.down_bytes = 0
        self.sockets = set()
        self.lock = threading.Lock()
        self.threads = []
        self.accept_thread = threading.Thread(target=self.accept, daemon=True)
        self.accept_thread.start()

    def accept(self):
        while not self.closed.is_set():
            try:
                client, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            with self.lock:
                self.sockets.add(client)
            thread = threading.Thread(target=self.serve, args=(client,), daemon=True)
            self.threads.append(thread)
            thread.start()

    def serve(self, client):
        upstream = None
        try:
            if self.reject:
                self.reject_stream(client)
                return
            upstream = socket.create_connection(("127.0.0.1", self.upstream_port), timeout=2)
            upstream.settimeout(None)
            with self.lock:
                self.sockets.add(upstream)
                self.connections += 1
            while not self.closed.is_set():
                readers = []
                if not self.pause_reads.is_set():
                    readers.append(client)
                if not self.pause_acks.is_set():
                    readers.append(upstream)
                ready, _, _ = select.select(readers, [], [], 0.05)
                for source in ready:
                    data = source.recv(65536)
                    if not data:
                        return
                    destination = upstream if source is client else client
                    destination.sendall(data)
                    with self.lock:
                        if source is client:
                            self.up_bytes += len(data)
                        else:
                            self.down_bytes += len(data)
        except (OSError, ValueError):
            # Resetting sockets and stopping the intake are deliberate test actions.
            pass
        finally:
            with self.lock:
                for stream in (client, upstream):
                    if stream:
                        self.sockets.discard(stream)
                        stream.close()

    def reject_stream(self, client):
        connection = H2Connection(config=H2Configuration(client_side=False))
        connection.initiate_connection()
        client.sendall(connection.data_to_send())
        client.settimeout(5)
        while not self.closed.is_set():
            data = client.recv(65536)
            if not data:
                return
            for event in connection.receive_data(data):
                if isinstance(event, RequestReceived):
                    connection.send_headers(event.stream_id, [
                        (":status", "200"), ("content-type", "application/grpc"),
                        ("grpc-status", "14"), ("grpc-message", "injected overload"),
                    ], end_stream=True)
                    client.sendall(connection.data_to_send())
                    with self.lock:
                        self.rejected += 1
                    return
            client.sendall(connection.data_to_send())

    def disconnect(self):
        with self.lock:
            for stream in self.sockets:
                try:
                    stream.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def close(self):
        self.closed.set()
        self.listener.close()
        self.disconnect()
        self.accept_thread.join(timeout=2)
        for thread in self.threads:
            thread.join(timeout=6)
