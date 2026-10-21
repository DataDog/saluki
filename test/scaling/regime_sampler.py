#!/usr/bin/env python3
"""Samples ADP's container from outside it, every 100 ms, for `run-regime.sh`.

Runs in a privileged container with the host's PID and cgroup namespaces (`compose.yaml`, service `sampler`), so that
reading spends none of ADP's CPU quota. Finds ADP's process by its name, then appends a CSV row per sample to the file
named by its argument:

- `t`: wall-clock seconds;
- `phase`: the content of `phase` next to the output file, which `run-regime.sh` writes as a run moves through its
  phases (`boot`, `warmup`, `steady`, `align`, `burst`, `recovery`, `done`);
- `usage_usec`, `nr_periods`, `nr_throttled`, `throttled_usec`: ADP's cgroup `cpu.stat`;
- `memory_current`, `anon`, `memory_peak`: `memory.current`, `anon` from `memory.stat`, and `memory.peak`;
- `udp_in`, `udp_rcvbuf_errors`: `InDatagrams` and `RcvbufErrors` of ADP's network namespace (`/proc/<pid>/net/snmp`);
- `rx_queue`: the bytes queued on ADP's UDP socket on port 8125 (`/proc/<pid>/net/udp`);
- `steady_written`, `burst_written`: lading's `bytes_written` from the generators' telemetry (`steady` and `burst` in
  the compose network), polled by a thread each, so that a paused generator, which does not answer, delays nothing.
  Over a Unix datagram socket the sender blocks while the socket is full, so a generator whose bytes stop growing while
  it has bytes to send is waiting for ADP to read.

A field that cannot be read is empty. Standard library only.
"""

import os
import sys
import threading
import time
import urllib.request

PROCESS_NAME = "agent-data-plan"  # `comm` is truncated to 15 bytes
INTERVAL = 0.1
GENERATORS = {"steady_written": "http://lading:9229/metrics", "burst_written": "http://burst:9229/metrics"}
FIELDS = [
    "t",
    "phase",
    "usage_usec",
    "nr_periods",
    "nr_throttled",
    "throttled_usec",
    "memory_current",
    "anon",
    "memory_peak",
    "udp_in",
    "udp_rcvbuf_errors",
    "rx_queue",
    *GENERATORS,
]
latest = {}


def read(path):
    try:
        with open(path) as file:
            return file.read()
    except OSError:
        return None


def find_adp():
    """Returns ADP's PID and cgroup directory, or None."""
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        if (read(f"/proc/{entry}/comm") or "").strip() == PROCESS_NAME:
            cgroup = read(f"/proc/{entry}/cgroup") or ""
            for line in cgroup.splitlines():
                if line.startswith("0::"):
                    return entry, "/sys/fs/cgroup" + line[3:].strip()
    return None


def keyed(text):
    values = {}
    for line in (text or "").splitlines():
        parts = line.split()
        if len(parts) == 2:
            values[parts[0]] = parts[1]
    return values


def udp_counters(pid):
    lines = [line.split() for line in (read(f"/proc/{pid}/net/snmp") or "").splitlines() if line.startswith("Udp:")]
    if len(lines) < 2:
        return "", ""
    row = dict(zip(lines[0][1:], lines[1][1:]))
    return row.get("InDatagrams", ""), row.get("RcvbufErrors", "")


def rx_queue(pid):
    total = None
    for line in (read(f"/proc/{pid}/net/udp") or "").splitlines()[1:]:
        parts = line.split()
        if len(parts) > 4 and parts[1].endswith(":1FBD"):
            total = (total or 0) + int(parts[4].split(":")[1], 16)
    return "" if total is None else str(total)


def poll_generator(field, url):
    """Keeps `latest[field]` at the generator's bytes written, with the time it was read."""
    while True:
        try:
            with urllib.request.urlopen(url, timeout=1) as response:
                total = sum(
                    float(line.split()[-1])
                    for line in response.read().decode().splitlines()
                    if line.startswith("bytes_written")
                )
            latest[field] = f"{total:.0f}"
        except Exception:
            pass
        time.sleep(0.05)


def main(out):
    for field, url in GENERATORS.items():
        threading.Thread(target=poll_generator, args=(field, url), daemon=True).start()
    phase_path = os.path.join(os.path.dirname(out), "phase")
    with open(out, "w", buffering=1) as file:
        file.write(",".join(FIELDS) + "\n")
        adp = None
        next_tick = time.time()
        while True:
            if adp is None or not os.path.exists(f"/proc/{adp[0]}"):
                adp = find_adp()
            row = {"t": f"{time.time():.3f}", "phase": (read(phase_path) or "").strip()}
            if adp is not None:
                pid, cgroup = adp
                row.update({k: v for k, v in keyed(read(f"{cgroup}/cpu.stat")).items() if k in FIELDS})
                row["memory_current"] = (read(f"{cgroup}/memory.current") or "").strip()
                row["anon"] = keyed(read(f"{cgroup}/memory.stat")).get("anon", "")
                row["memory_peak"] = (read(f"{cgroup}/memory.peak") or "").strip()
                row["udp_in"], row["udp_rcvbuf_errors"] = udp_counters(pid)
                row["rx_queue"] = rx_queue(pid)
            row.update(latest)
            file.write(",".join(row.get(field, "") for field in FIELDS) + "\n")
            next_tick += INTERVAL
            time.sleep(max(0.0, next_tick - time.time()))


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: regime_sampler.py timeline.csv")
    main(sys.argv[1])
