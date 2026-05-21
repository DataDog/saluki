import os
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.request

PAYLOAD = b"adp.forwarding.integration:1|c|#source:integration"
MALFORMED_PAYLOAD = b"this is not dogstatsd"
UDP_BATCHED_PAYLOAD = MALFORMED_PAYLOAD + b"\n" + PAYLOAD
STREAM_PAYLOAD = b"adp.forwarding.stream_one:1|c\nadp.forwarding.stream_two:2|c\n"
UDS_PAYLOAD = b"adp.forwarding.uds_datagram:1|c"
UDS_STREAM_PAYLOAD = b"adp.forwarding.uds_stream:1|c\n"
FORWARDED_UDP_BATCH_PATH = "/tmp/dsd-forwarded-udp-batch"
FORWARDED_STREAM_PATH = "/tmp/dsd-forwarded-stream-packet"
FORWARDED_UDS_PATH = "/tmp/dsd-forwarded-uds-packet"
FORWARDED_UDS_STREAM_PATH = "/tmp/dsd-forwarded-uds-stream-packet"
PARSED_METRIC_PATH = "/tmp/dsd-metric-parsed"
FORWARD_ADDR = ("127.0.0.1", 9125)
DOGSTATSD_ADDR = ("127.0.0.1", 8125)
DOGSTATSD_STREAM_ADDR = ("127.0.0.1", 9126)
DOGSTATSD_UDS_PATH = "/tmp/dsd-forwarding.sock"
DOGSTATSD_UDS_STREAM_PATH = "/tmp/dsd-forwarding-stream.sock"
TELEMETRY_URLS = ("http://127.0.0.1:5102/metrics", "http://127.0.0.1:5102/compat/metrics")
PROBE_TIMEOUT_SECS = 60
PROBE_INTERVAL_SECS = 0.25
SOCKET_TIMEOUT_SECS = 0.2

stop_event = threading.Event()
agent_process = None


def remove_paths(paths):
    for path in paths:
        try:
            os.remove(path)
        except FileNotFoundError:
            pass


def remove_marker_files():
    remove_paths(
        (
            FORWARDED_UDP_BATCH_PATH,
            FORWARDED_STREAM_PATH,
            FORWARDED_UDS_PATH,
            FORWARDED_UDS_STREAM_PATH,
            PARSED_METRIC_PATH,
        )
    )


def forwarded_packets_received():
    return (
        os.path.exists(FORWARDED_UDP_BATCH_PATH)
        and os.path.exists(FORWARDED_STREAM_PATH)
        and os.path.exists(FORWARDED_UDS_PATH)
        and os.path.exists(FORWARDED_UDS_STREAM_PATH)
    )


def receive_forwarded_packet():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp_socket:
        udp_socket.bind(FORWARD_ADDR)
        udp_socket.settimeout(SOCKET_TIMEOUT_SECS)
        deadline = time.monotonic() + PROBE_TIMEOUT_SECS

        while not stop_event.is_set() and time.monotonic() < deadline:
            try:
                data, _addr = udp_socket.recvfrom(65535)
            except TimeoutError:
                continue

            if data == UDP_BATCHED_PAYLOAD:
                with open(FORWARDED_UDP_BATCH_PATH, "wb") as output_file:
                    output_file.write(data)
            elif data == STREAM_PAYLOAD:
                with open(FORWARDED_STREAM_PATH, "wb") as output_file:
                    output_file.write(data)
            elif data == UDS_PAYLOAD:
                with open(FORWARDED_UDS_PATH, "wb") as output_file:
                    output_file.write(data)
            elif data == UDS_STREAM_PAYLOAD:
                with open(FORWARDED_UDS_STREAM_PATH, "wb") as output_file:
                    output_file.write(data)

            if forwarded_packets_received():
                return


def send_dogstatsd_payload():
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp_socket:
        deadline = time.monotonic() + PROBE_TIMEOUT_SECS

        while not stop_event.is_set() and time.monotonic() < deadline:
            udp_socket.sendto(MALFORMED_PAYLOAD, DOGSTATSD_ADDR)
            udp_socket.sendto(PAYLOAD, DOGSTATSD_ADDR)
            if forwarded_packets_received():
                return
            time.sleep(PROBE_INTERVAL_SECS)


def send_dogstatsd_stream_payload():
    stream_frame = len(STREAM_PAYLOAD).to_bytes(4, "little") + STREAM_PAYLOAD
    deadline = time.monotonic() + PROBE_TIMEOUT_SECS

    while not stop_event.is_set() and time.monotonic() < deadline:
        try:
            with socket.create_connection(DOGSTATSD_STREAM_ADDR, timeout=1) as stream_socket:
                stream_socket.sendall(stream_frame)
        except OSError:
            time.sleep(PROBE_INTERVAL_SECS)
            continue

        if os.path.exists(FORWARDED_STREAM_PATH):
            return
        time.sleep(PROBE_INTERVAL_SECS)


def send_unix_datagram_payload():
    client_path = "/tmp/dsd-forwarding-client.sock"
    remove_paths((client_path,))
    deadline = time.monotonic() + PROBE_TIMEOUT_SECS

    while not stop_event.is_set() and time.monotonic() < deadline:
        if not os.path.exists(DOGSTATSD_UDS_PATH):
            time.sleep(PROBE_INTERVAL_SECS)
            continue

        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM) as uds_socket:
                uds_socket.bind(client_path)
                uds_socket.sendto(UDS_PAYLOAD, DOGSTATSD_UDS_PATH)
        except OSError:
            time.sleep(PROBE_INTERVAL_SECS)
            continue
        finally:
            remove_paths((client_path,))

        if os.path.exists(FORWARDED_UDS_PATH):
            return
        time.sleep(PROBE_INTERVAL_SECS)


def send_unix_stream_payload():
    stream_frame = len(UDS_STREAM_PAYLOAD).to_bytes(4, "little") + UDS_STREAM_PAYLOAD
    deadline = time.monotonic() + PROBE_TIMEOUT_SECS

    while not stop_event.is_set() and time.monotonic() < deadline:
        if not os.path.exists(DOGSTATSD_UDS_STREAM_PATH):
            time.sleep(PROBE_INTERVAL_SECS)
            continue

        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream_socket:
                stream_socket.connect(DOGSTATSD_UDS_STREAM_PATH)
                stream_socket.sendall(stream_frame)
        except OSError:
            time.sleep(PROBE_INTERVAL_SECS)
            continue

        if os.path.exists(FORWARDED_UDS_STREAM_PATH):
            return
        time.sleep(PROBE_INTERVAL_SECS)


def metric_line_value(line):
    try:
        return float(line.rsplit(maxsplit=1)[1])
    except (IndexError, ValueError):
        return 0.0


def metric_parsed_line(line):
    return (
        "component_events_received_total" in line
        and 'message_type="metrics"' in line
        and 'listener_type="udp"' in line
        and metric_line_value(line) > 0.0
    )


def poll_ingestion_telemetry():
    deadline = time.monotonic() + PROBE_TIMEOUT_SECS

    while not stop_event.is_set() and time.monotonic() < deadline:
        for telemetry_url in TELEMETRY_URLS:
            try:
                with urllib.request.urlopen(telemetry_url, timeout=1) as response:
                    body = response.read().decode("utf-8", errors="replace")
            except Exception:
                continue

            for line in body.splitlines():
                if metric_parsed_line(line):
                    with open(PARSED_METRIC_PATH, "w", encoding="utf-8") as output_file:
                        output_file.write(line)
                    return

        time.sleep(PROBE_INTERVAL_SECS)


def handle_signal(_signum, _frame):
    stop_event.set()
    if agent_process is not None and agent_process.poll() is None:
        agent_process.terminate()


def start_thread(target):
    thread = threading.Thread(target=target, daemon=True)
    thread.start()
    return thread


def main():
    global agent_process

    remove_marker_files()
    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    receiver_thread = start_thread(receive_forwarded_packet)
    agent_process = subprocess.Popen(["/bin/entrypoint.sh"])
    sender_thread = start_thread(send_dogstatsd_payload)
    stream_sender_thread = start_thread(send_dogstatsd_stream_payload)
    uds_sender_thread = start_thread(send_unix_datagram_payload)
    uds_stream_sender_thread = start_thread(send_unix_stream_payload)
    telemetry_thread = start_thread(poll_ingestion_telemetry)

    try:
        return agent_process.wait()
    finally:
        stop_event.set()
        receiver_thread.join(timeout=1)
        sender_thread.join(timeout=1)
        stream_sender_thread.join(timeout=1)
        uds_sender_thread.join(timeout=1)
        uds_stream_sender_thread.join(timeout=1)
        telemetry_thread.join(timeout=1)


if __name__ == "__main__":
    sys.exit(main())
