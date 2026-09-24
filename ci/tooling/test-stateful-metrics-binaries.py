#!/usr/bin/env python3
"""Experimental local binary integration tests for ADP and the Foldspace metrics intake.

Uses loopback endpoints and a dummy API key. This is development tooling, not a
production benchmark or a CI job. See docs/development/stateful-metrics.md.
"""

import argparse
from collections import Counter
import hashlib
import http.server
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request

from stateful_metrics_proxy import FaultProxy


HOSTNAME = "foldspace-binary-test"
PREFIX = "adp.foldspace.binary."
TIMEOUT = 20


def require(condition, message):
    if not condition:
        raise AssertionError(message)


def free_port(kind=socket.SOCK_STREAM):
    with socket.socket(socket.AF_INET, kind) as listener:
        listener.bind(("127.0.0.1", 0))
        return listener.getsockname()[1]


def wait_for(predicate, description, timeout=TIMEOUT):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.05)
    raise AssertionError(f"Timed out waiting for {description}")


class Fixture:
    def __init__(self, args, name, flush=1, capacity=10000, persist=False):
        self.args = args
        self.root = args.output / name
        self.root.mkdir()
        self.intake_port = free_port()
        self.udp_port = free_port(socket.SOCK_DGRAM)
        self.api_port = free_port()
        self.journal = self.root / "metrics.jsonl"
        self.processes = []
        self.http_paths = []
        self.adp = None
        self.intake = None
        self.proxy = None
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("DD_")}
        for key in ("INTAKE_MODE", "LISTEN_ADDR", "JOURNAL_PATH"):
            self.env.pop(key, None)
        paths = self.http_paths

        class Sink(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length", "0")))
                paths.append(self.path)
                self.send_response(202)
                self.end_headers()

            def log_message(self, *_args):
                pass

        self.http = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Sink)
        self.http_thread = threading.Thread(target=self.http.serve_forever, daemon=True)
        self.http_thread.start()
        self.config = {
            "hostname": HOSTNAME, "api_key": "test-key", "disable_file_logging": True,
            "dd_url": f"http://127.0.0.1:{self.http.server_port}",
            "dogstatsd_port": self.udp_port, "dogstatsd_socket": "",
            "dogstatsd_origin_detection": False, "bind_host": "127.0.0.1",
            "flush_timeout_secs": flush, "serializer_max_metrics_per_payload": capacity,
            "metrics_level": "debug", "health_port": 0,
            "ipc_cert_file_path": str(args.output / "ipc_cert.pem"),
            "auth_token_file_path": str(self.root / "auth_token"),
            "data_plane": {
                "enabled": True, "standalone_mode": True, "dogstatsd": {"enabled": True},
                "api_listen_address": f"tcp://127.0.0.1:{self.api_port}",
                "secure_api_listen_address": "tcp://127.0.0.1:0", "stop_timeout": 8,
                "stateful_metrics_endpoint": f"http://127.0.0.1:{self.intake_port}",
            },
        }
        if persist:
            self.config.update({
                "forwarder_storage_path": str(self.root / "retry"),
                "forwarder_storage_max_size_in_bytes": 16 * 1024 * 1024,
                "forwarder_storage_max_disk_ratio": 1.0,
            })
        (self.root / "datadog.yaml").write_text(json.dumps(self.config, indent=2))

    def spawn(self, label, command, env):
        log = self.root / f"{label}-{len(self.processes)}.log"
        with log.open("w") as output:
            process = subprocess.Popen(command, env=env, stdout=output, stderr=subprocess.STDOUT)
        self.processes.append(process)
        return process, log

    def start_intake(self):
        self.intake, _ = self.spawn("intake", [str(self.args.intake)], {
            **self.env, "INTAKE_MODE": "metrics",
            "LISTEN_ADDR": f"127.0.0.1:{self.intake_port}", "JOURNAL_PATH": str(self.journal),
        })

        def ready():
            require(self.intake.poll() is None, "Intake exited during startup")
            try:
                with socket.create_connection(("127.0.0.1", self.intake_port), timeout=0.1):
                    return True
            except OSError:
                return False

        wait_for(ready, "intake listener")

    def start_adp(self):
        (self.root / "datadog.yaml").write_text(json.dumps(self.config, indent=2))
        self.adp, self.adp_log = self.spawn("adp", [
            str(self.args.adp), "--config", str(self.root / "datadog.yaml"), "run",
        ], self.env)

        def ready():
            require(self.adp.poll() is None, "ADP exited during startup")
            return "Topology healthy." in self.adp_log.read_text()

        wait_for(ready, "ADP readiness")

    def telemetry(self, metric, component=None):
        require(self.adp.poll() is None, "ADP exited while waiting for telemetry")
        # Ignore ambient HTTP proxy settings for the loopback API.
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(f"http://127.0.0.1:{self.api_port}/metrics", timeout=2) as response:
            body = response.read().decode()
        (self.root / "telemetry.txt").write_text(body)
        return sum(
            float(line.split()[1]) for line in body.splitlines()
            if not line.startswith("#") and metric in line.split("{")[0]
            and (component is None or f'component_id="{component}"' in line)
        )

    def wait_metric(self, metric, minimum=1, component=None):
        wait_for(lambda: self.telemetry(metric, component) >= minimum, metric)

    def send(self, samples, timestamp=None):
        timestamp = timestamp or int(time.time())
        with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sender:
            sender.sendto("\n".join(f"{PREFIX}{sample}|T{timestamp}" for sample in samples).encode(),
                          ("127.0.0.1", self.udp_port))
        return timestamp

    def rows(self):
        if not self.journal.exists():
            return []
        # A concurrent journal write may end in an incomplete line.
        lines = self.journal.read_text().splitlines(keepends=True)
        return [json.loads(line) for line in lines if line.endswith("\n")]

    def expect(self, name, value, timestamp, kind="Gauge", interval=0, extra_resources=()):
        def matching():
            if self.adp is not None:
                require(self.adp.poll() is None, "ADP exited before delivery")
            return [row for row in self.rows() if row["name"] == PREFIX + name
                    and {"timestamp": timestamp, "value": value} in row["points"]]

        rows = wait_for(matching, f"decoded {name}={value}")
        for row in rows:
            require(row["metric_type"] == kind and row["interval"] == interval, row)
            require(sorted(row["tags"]["prefix"] + row["tags"]["values"]) == ["env:binary-test"], row)
            resources = [{"kind": "host", "name": HOSTNAME}, *extra_resources]
            require(sorted(row["resources"], key=str) == sorted(resources, key=str), row)
            require(row["origin"] == {"product": 10, "category": 10, "service": 0}, row)

    def stop_adp(self):
        self.adp.send_signal(signal.SIGTERM)
        require(self.adp.wait(timeout=15) == 0, "ADP shutdown failed")
        require("Agent Data Plane shut down successfully." in self.adp_log.read_text(), "Unclean shutdown")
        self.adp = None

    def stop_intake(self):
        self.intake.kill()
        self.intake.wait(timeout=5)
        self.intake = None

    def no_http_series(self):
        require(not any("/series" in path for path in self.http_paths), self.http_paths)

    def start_proxy(self):
        self.proxy = FaultProxy(self.intake_port)
        self.config["data_plane"]["stateful_metrics_endpoint"] = f"http://127.0.0.1:{self.proxy.port}"

    def snapshot(self):
        usage = subprocess.check_output(["ps", "-o", "rss=,%cpu=", "-p", str(self.adp.pid)], text=True).split()
        intake_usage = subprocess.check_output(["ps", "-o", "rss=,%cpu=", "-p", str(self.intake.pid)], text=True).split()
        return {"time": time.monotonic(), "rss_kib": int(usage[0]), "cpu_percent": float(usage[1]),
                "intake_rss_kib": int(intake_usage[0]), "intake_cpu_percent": float(intake_usage[1]),
                "high_priority_entries": self.telemetry("endpoint_high_prio_queue_insertions_total", "dd_stateful_metrics_out")
                - self.telemetry("endpoint_high_prio_queue_removals_total", "dd_stateful_metrics_out"),
                "queue_entries": self.telemetry("network_http_retry_queue_size", "dd_stateful_metrics_out"),
                "acked": self.telemetry("stateful_metrics_batches_acked_total"),
                "failures": self.telemetry("stateful_metrics_stream_failures_total"),
                "dropped": self.telemetry("stateful_metrics_points_dropped_total"),
                "decoded_points": sum(len(row["points"]) for row in self.rows())}

    def close(self):
        for process in reversed(self.processes):
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
        self.http.shutdown()
        self.http.server_close()
        self.http_thread.join()
        if self.proxy:
            self.proxy.close()
        (self.root / "http-paths.json").write_text(json.dumps(self.http_paths, indent=2))


def timer_and_metadata(fixture):
    fixture.start_intake()
    fixture.start_adp()
    started = time.monotonic()
    tags = "#env:binary-test"
    timestamp = fixture.send([f"gauge:7|g|{tags},device:disk0,dd.internal.resource:pod:pod-a",
                              f"counter:30|c|{tags}"])
    fixture.expect("gauge", 7, timestamp, extra_resources=[
        {"kind": "device", "name": "disk0"}, {"kind": "pod", "name": "pod-a"},
    ])
    fixture.expect("counter", 3, timestamp, "Rate", 10)
    fixture.wait_metric("stateful_metrics_batches_acked_total")
    time.sleep(4)
    require(fixture.telemetry("stateful_metrics_batches_acked_total") >= 1, "ACK counter vanished while idle")
    # Reuse acknowledged name/tag dictionaries with another value and timestamp.
    second = fixture.send([f"counter:50|c|{tags}"], timestamp + 1)
    fixture.expect("counter", 5, second, "Rate", 10)
    fixture.wait_metric("stateful_metrics_batches_acked_total", 2)
    fixture.stop_adp()
    require(len(fixture.rows()) == 3, fixture.rows())
    fixture.no_http_series()
    return {"elapsed_seconds": round(time.monotonic() - started, 3), "decoded_series": 3}


def threshold(fixture):
    fixture.start_intake()
    fixture.start_adp()
    timestamp = fixture.send(["threshold.a:1|g|#env:binary-test", "threshold.b:2|g|#env:binary-test"])
    fixture.expect("threshold.a", 1, timestamp)
    fixture.expect("threshold.b", 2, timestamp)
    fixture.stop_adp()
    require(len(fixture.rows()) == 2, fixture.rows())
    fixture.no_http_series()


def shutdown(fixture):
    fixture.start_intake()
    fixture.start_adp()
    timestamp = fixture.send(["shutdown:9|g|#env:binary-test"])
    fixture.wait_metric("endpoint_high_prio_queue_removals_total", component="dd_stateful_metrics_out")
    require(not fixture.rows(), "Partial batch flushed before shutdown")
    fixture.stop_adp()
    fixture.expect("shutdown", 9, timestamp)
    require(len(fixture.rows()) == 1, fixture.rows())
    fixture.no_http_series()


def reconnect(fixture):
    fixture.start_intake()
    fixture.start_adp()
    timestamp = fixture.send(["reconnect:1|g|#env:binary-test"])
    fixture.expect("reconnect", 1, timestamp)
    fixture.wait_metric("stateful_metrics_batches_acked_total")
    fixture.stop_intake()
    fixture.wait_metric("stateful_metrics_stream_failures_total")
    second = fixture.send(["reconnect:2|g|#env:binary-test", "outage:3|g|#env:binary-test"], timestamp + 1)
    fixture.wait_metric("endpoint_high_prio_queue_insertions_total", 2, "dd_stateful_metrics_out")
    fixture.no_http_series()
    fixture.start_intake()
    fixture.expect("reconnect", 2, second)
    fixture.expect("outage", 3, second)
    fixture.stop_adp()
    require(len(fixture.rows()) == 3, fixture.rows())
    fixture.no_http_series()


def disk_restart(fixture):
    fixture.start_adp()
    timestamp = fixture.send(["persisted:11|g|#env:binary-test"])
    fixture.wait_metric("endpoint_high_prio_queue_insertions_total", component="dd_stateful_metrics_out")
    fixture.stop_adp()
    files = list((fixture.root / "retry").rglob("retry-*.json"))
    require(files, "Shutdown did not persist logical retries")
    require(any(PREFIX + "persisted" in file.read_text() for file in files), "Missing logical series on disk")
    fixture.no_http_series()
    fixture.start_intake()
    fixture.start_adp()
    fixture.expect("persisted", 11, timestamp)
    fixture.wait_metric("stateful_metrics_batches_acked_total")
    fixture.stop_adp()
    require(len(fixture.rows()) == 1, fixture.rows())
    fixture.no_http_series()


def canonical_foldspace(rows):
    return Counter(
        (row["name"], tuple(sorted(row["tags"]["prefix"] + row["tags"]["values"] +
                                 ["host:" + r["name"] for r in row["resources"] if r["kind"] == "host"])),
         row["metric_type"], row["interval"], point["timestamp"], point["value"])
        for row in rows for point in row["points"]
    )


def http_comparison(fixture):
    require(fixture.args.http_intake is not None, "--http-intake is required for the HTTP comparison")
    timestamp = int(time.time())
    rounds = [
        ["compare.gauge:7.25|g|#env:binary-test,zone:a", "compare.counter:30|c|#env:binary-test"],
        ["compare.gauge:-2.5|g|#env:binary-test,zone:a", "compare.counter:50|c|#env:binary-test"],
        ["compare.zero:0|g|#env:binary-test", "compare.unicode:123456.125|g|#env:binary-test,label:café"],
    ]
    fixture.start_intake()
    fixture.start_adp()
    for index, samples in enumerate(rounds):
        fixture.send(samples, timestamp + index)
        wait_for(lambda: sum(len(r["points"]) for r in fixture.rows()) >= (index + 1) * 2, "Foldspace comparison points")
    fixture.stop_adp()
    expected = canonical_foldspace(fixture.rows())
    fixture.no_http_series()

    # The existing Saluki intake independently decodes HTTP V3 protobuf payloads.
    for port, kind in ((2049, socket.SOCK_STREAM), (9125, socket.SOCK_DGRAM)):
        with socket.socket(socket.AF_INET, kind) as check:
            check.bind(("0.0.0.0", port))
    intake, _ = fixture.spawn("http-intake", [str(fixture.args.http_intake)], fixture.env)
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))

    def get(path):
        require(intake.poll() is None, "HTTP intake exited")
        try:
            with opener.open("http://127.0.0.1:2049" + path, timeout=2) as response:
                return response.read()
        except OSError:
            return None

    wait_for(lambda: get("/ready") is not None, "HTTP intake readiness")
    del fixture.config["data_plane"]["stateful_metrics_endpoint"]
    fixture.config["dd_url"] = "http://127.0.0.1:2049"
    fixture.config["use_v3_api"] = {"series": {"enabled": "true"}}
    fixture.start_adp()
    for index, samples in enumerate(rounds):
        fixture.send(samples, timestamp + index)
    wait_for(lambda: sum(len(row["values"]) for row in json.loads(get("/metrics/dump"))) >= 6,
             "HTTP comparison points")
    fixture.stop_adp()
    rows = json.loads(get("/metrics/dump"))
    (fixture.root / "http-decoded.json").write_text(json.dumps(rows, indent=2))
    actual = Counter(
        (row["context"]["name"], tuple(sorted(row["context"]["tags"])), value["mtype"],
         value.get("interval", 0), timestamp, value["value"])
        for row in rows for timestamp, value in row["values"]
        if row["context"]["name"].startswith(PREFIX)
    )
    require(actual == expected, {"http_only": list((actual - expected).elements()),
                                 "foldspace_only": list((expected - actual).elements())})
    return {"matched_points": sum(actual.values()), "http_format": "V3",
            "fields": ["name", "tags", "host", "type", "interval", "timestamp", "value"]}


def rejected_stream(fixture):
    fixture.start_intake()
    fixture.start_proxy()
    fixture.proxy.reject = True
    fixture.start_adp()
    wait_for(lambda: fixture.proxy.rejected > 0, "injected gRPC UNAVAILABLE rejection")
    timestamp = fixture.send(["rejected:13|g|#env:binary-test"])
    fixture.wait_metric("endpoint_high_prio_queue_insertions_total", component="dd_stateful_metrics_out")
    require(not fixture.rows(), "Rejected stream delivered unexpectedly")
    fixture.proxy.reject = False
    fixture.expect("rejected", 13, timestamp)
    fixture.wait_metric("stateful_metrics_batches_acked_total")
    fixture.stop_adp()
    fixture.no_http_series()
    return {"rejected_streams": fixture.proxy.rejected}


def lost_ack(fixture):
    fixture.start_intake()
    fixture.start_proxy()
    fixture.start_adp()
    timestamp = fixture.send(["ack.warmup:1|g|#env:binary-test"])
    fixture.expect("ack.warmup", 1, timestamp)
    fixture.wait_metric("stateful_metrics_batches_acked_total")
    fixture.proxy.pause_acks.set()
    timestamp = fixture.send(["ack.retry:17|g|#env:binary-test"])
    fixture.expect("ack.retry", 17, timestamp)
    time.sleep(2)
    require(fixture.telemetry("stateful_metrics_batches_acked_total") == 1, "Blocked ACK was counted")
    fixture.proxy.disconnect()
    fixture.proxy.pause_acks.clear()
    wait_for(lambda: sum(row["name"] == PREFIX + "ack.retry" for row in fixture.rows()) == 2,
             "unacknowledged point to be replayed")
    fixture.expect("ack.retry", 17, timestamp)
    fixture.wait_metric("stateful_metrics_batches_acked_total", 2)
    fixture.wait_metric("stateful_metrics_batches_retried_total")
    fixture.stop_adp()
    require(len(fixture.rows()) == 3, fixture.rows())
    fixture.no_http_series()
    return {"expected_duplicate_points": 1, "connections": fixture.proxy.connections}


def stalled_reads(fixture):
    fixture.start_intake()
    fixture.start_proxy()
    fixture.start_adp()
    timestamp = fixture.send(["stall.warmup:1|g|#env:binary-test"])
    fixture.expect("stall.warmup", 1, timestamp)
    fixture.wait_metric("stateful_metrics_batches_acked_total")
    fixture.proxy.pause_reads.set()
    expected = {}
    for batch in range(30):
        samples = []
        for index in range(20):
            name = f"stall.{batch}.{index}"
            value = batch * 20 + index
            expected[PREFIX + name] = value
            samples.append(f"{name}:{value}|g|#env:binary-test")
        fixture.send(samples, timestamp)
        time.sleep(0.02)
    time.sleep(3)
    stalled = fixture.snapshot()
    require(len(fixture.rows()) == 1, "Data crossed the paused proxy")
    fixture.proxy.pause_reads.clear()
    wait_for(lambda: len(fixture.rows()) >= 601, "all points after resumed consumption")
    actual = {row["name"]: row["points"][0]["value"] for row in fixture.rows()
              if row["name"] != PREFIX + "stall.warmup"}
    require(actual == expected and len(fixture.rows()) == 601, "Lost or duplicated points after stalled reads")
    recovered = fixture.snapshot()
    require(recovered["dropped"] == 0, recovered)
    fixture.stop_adp()
    fixture.no_http_series()
    return {"stalled": stalled, "recovered": recovered}


def ack_timeout(fixture):
    fixture.start_intake()
    fixture.start_proxy()
    fixture.start_adp()
    timestamp = fixture.send(["timeout.warmup:1|g|#env:binary-test"])
    fixture.expect("timeout.warmup", 1, timestamp)
    fixture.wait_metric("stateful_metrics_batches_acked_total")
    fixture.proxy.pause_acks.set()
    timestamp = fixture.send(["timeout.retry:19|g|#env:binary-test"])
    fixture.expect("timeout.retry", 19, timestamp)
    started = time.monotonic()
    wait_for(lambda: fixture.telemetry("stateful_metrics_stream_failures_total") > 0,
             "ADP acknowledgement timeout", timeout=40)
    elapsed = time.monotonic() - started
    require('kind="DeadlineExceeded"' in (fixture.root / "telemetry.txt").read_text(), "Wrong timeout classification")
    fixture.proxy.pause_acks.clear()
    wait_for(lambda: sum(row["name"] == PREFIX + "timeout.retry" for row in fixture.rows()) == 2,
             "replay after ACK timeout")
    fixture.expect("timeout.retry", 19, timestamp)
    fixture.wait_metric("stateful_metrics_batches_acked_total", 2)
    fixture.stop_adp()
    fixture.no_http_series()
    return {"timeout_wait_seconds": round(elapsed, 3), "expected_duplicate_points": 1}


def sustained_load(fixture):
    fixture.start_intake()
    fixture.start_proxy()
    fixture.start_adp()
    timestamp = fixture.send(["load.warmup:1|g|#env:binary-test"])
    fixture.expect("load.warmup", 1, timestamp)
    fixture.wait_metric("stateful_metrics_batches_acked_total")
    samples = []
    sent = 0
    started = time.monotonic()
    # Hold consumption long enough to fill the eight-payload window and queue fresh work.
    for second in range(30):
        if second == 10:
            fixture.proxy.pause_reads.set()
        if second == 10 + fixture.args.load_pause_seconds:
            fixture.proxy.pause_reads.clear()
        for packet in range(50):
            fixture.send([f"load.series{index}:{sent + index}|g|#env:binary-test,packet:{packet}"
                          for index in range(20)], timestamp + second + 1)
            sent += 20
            deadline = started + second + (packet + 1) / 50
            time.sleep(max(0, deadline - time.monotonic()))
        samples.append(fixture.snapshot())
    wait_for(lambda: sum(len(row["points"]) for row in fixture.rows()) >= sent + 1, "load drain", timeout=40)
    expected = Counter((PREFIX + f"load.series{index}", f"packet:{packet}", timestamp + second + 1,
                        second * 1000 + packet * 20 + index)
                       for second in range(30) for packet in range(50) for index in range(20))
    actual = Counter((row["name"], next(tag for tag in row["tags"]["values"] if tag.startswith("packet:")),
                      point["timestamp"], point["value"])
                     for row in fixture.rows() if row["name"] != PREFIX + "load.warmup" for point in row["points"])
    require(actual == expected, {"missing": sum((expected - actual).values()),
                                 "extra": sum((actual - expected).values())})
    samples.append(fixture.snapshot())
    require(samples[-1]["dropped"] == 0 and samples[-1]["queue_entries"] == 0
            and samples[-1]["high_priority_entries"] == 0, samples[-1])
    if fixture.args.load_pause_seconds >= 12:
        require(any(sample["high_priority_entries"] > 0 for sample in samples),
                "Load did not exercise the fresh-work backlog")
    fixture.stop_adp()
    fixture.no_http_series()
    (fixture.root / "load-samples.json").write_text(json.dumps(samples, indent=2))
    return {"sent_points": sent, "decoded_points": sum(actual.values()),
            "target_points_per_second": 1000, "send_duration_seconds": 30,
            "paused_consumption_seconds": fixture.args.load_pause_seconds,
            "total_seconds": round(time.monotonic() - started, 3),
            "peak_adp_rss_kib": max(s["rss_kib"] for s in samples),
            "peak_intake_rss_kib": max(s["intake_rss_kib"] for s in samples),
            "peak_high_priority_entries": max(s["high_priority_entries"] for s in samples),
            "peak_queue_entries": max(s["queue_entries"] for s in samples),
            "failures": samples[-1]["failures"], "dropped": samples[-1]["dropped"],
            "proxy_up_bytes": fixture.proxy.up_bytes, "proxy_down_bytes": fixture.proxy.down_bytes}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--adp", required=True, type=Path)
    parser.add_argument("--intake", required=True, type=Path)
    parser.add_argument("--http-intake", type=Path)
    parser.add_argument("--case", action="append", help="Run only named cases; repeat to select multiple")
    parser.add_argument("--load-pause-seconds", type=int, default=12, help="Consumption pause during the load case (1-19)")
    parser.add_argument("--output", type=Path, help="New directory for logs, journals, and results")
    args = parser.parse_args()
    require(1 <= args.load_pause_seconds <= 19, "--load-pause-seconds must be between 1 and 19")
    args.adp, args.intake = args.adp.resolve(), args.intake.resolve()
    if args.http_intake:
        args.http_intake = args.http_intake.resolve()
    for binary in (args.adp, args.intake):
        require(binary.is_file() and os.access(binary, os.X_OK), f"Missing executable: {binary}")
    if args.output:
        args.output = args.output.resolve()
        args.output.mkdir(parents=True, exist_ok=False)
    else:
        args.output = Path(tempfile.mkdtemp(prefix="foldspace-binaries-")).resolve()
    print(f"Artifacts: {args.output}", flush=True)
    subprocess.run([
        "openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
        "-subj", "/CN=localhost", "-keyout", str(args.output / "key.pem"),
        "-out", str(args.output / "cert.pem"),
    ], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    (args.output / "ipc_cert.pem").write_bytes(
        (args.output / "cert.pem").read_bytes() + (args.output / "key.pem").read_bytes())
    result = {"binaries": {}, "cases": {}}
    binaries = {"adp": args.adp, "intake": args.intake}
    if args.http_intake:
        binaries["http_intake"] = args.http_intake
    for label, binary in binaries.items():
        with binary.open("rb") as source:
            result["binaries"][label] = {"path": str(binary), "sha256": hashlib.file_digest(source, "sha256").hexdigest()}
    cases = [(timer_and_metadata, {}), (http_comparison, {}), (threshold, {"flush": 60, "capacity": 2}),
             (shutdown, {"flush": 60}), (reconnect, {}), (disk_restart, {"persist": True}),
             (rejected_stream, {}), (lost_ack, {}), (stalled_reads, {"capacity": 1}),
             (ack_timeout, {}),
             (sustained_load, {"capacity": 100})]
    if args.case:
        require(set(args.case) <= {case.__name__ for case, _ in cases}, "Unknown --case")
        cases = [(case, settings) for case, settings in cases if case.__name__ in args.case]
    for case, settings in cases:
        fixture = Fixture(args, case.__name__, **settings)
        started = time.monotonic()
        try:
            details = case(fixture)
            result["cases"][case.__name__] = {
                "status": "passed", "seconds": round(time.monotonic() - started, 3), "details": details,
            }
            print(f"PASS {case.__name__}", flush=True)
        except Exception as error:
            result["cases"][case.__name__] = {"status": "failed", "error": str(error)}
            raise
        finally:
            fixture.close()
            (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n")


if __name__ == "__main__":
    main()
