#!/usr/bin/env python3
"""Minimal HTTP server that accepts any POST/GET and returns 200. Used as a fake
Datadog intake so ADP's forwarder doesn't back up on a closed socket during benchmarks."""

from http.server import BaseHTTPRequestHandler, HTTPServer


class Blackhole(BaseHTTPRequestHandler):
    def do_POST(self):
        self.send_response(200)
        self.end_headers()
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)

    def do_GET(self):
        self.send_response(200)
        self.end_headers()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    server = HTTPServer(("127.0.0.1", 2049), Blackhole)
    server.serve_forever()
