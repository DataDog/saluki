#!/usr/bin/env python3
"""Local outage scenarios for ADP's stateful metrics path under sustained DogStatsD load.

Testing only; not a CI job. Each scenario starts ack-only stateful intakes (`stateful-metrics-blackhole`)
with injected faults, ADP configured from a generated SMP case, and lading driving that case's load.
Lading's capture file records ADP's internal telemetry once a second. The script also samples ADP's RSS
and CPU, and writes a summary per scenario. See docs/development/stateful-metrics.md.
"""

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request

import yaml


REPO = Path(__file__).resolve().parents[2]
DEFAULT_CASE = REPO / "test/smp/regression/adp/full/cases/stateful_dsd_50mb_100k_contexts_cpu"
FIRST_INTAKE_PORT = 9201
TELEMETRY_URL = "http://127.0.0.1:5100/metrics"
TELEMETRY_PREFIX = "adp__"
STARTUP_TIMEOUT = 60

OUTAGE = {"OUTAGE_EVERY_SECS": "180", "OUTAGE_FOR_SECS": "60"}

# Each scenario lists one fault environment per endpoint, primary first.
SCENARIOS = {
    "healthy": ("Two healthy endpoints, for comparison.", [{}, {}]),
    "endpoint_outage": (
        "The second of two endpoints is down for 60s every 180s; its copies wait in its retry lane.",
        [{}, OUTAGE],
    ),
    "full_outage": (
        "The only endpoint is down for 60s every 180s; everything waits in the shared retry queue.",
        [OUTAGE],
    ),
    "ack_stall": (
        "The only endpoint stays connected but withholds acks for 45s every 180s, past ADP's 30s ack deadline.",
        [{"ACK_PAUSE_EVERY_SECS": "180", "ACK_PAUSE_FOR_SECS": "45"}],
    ),
    "slow_endpoint": (
        "The second of two endpoints acks 250ms late, like a distant intake.",
        [{}, {"ACK_DELAY_MS": "250"}],
    ),
}


def wait_for(predicate, description, timeout=STARTUP_TIMEOUT):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.2)
    raise RuntimeError(f"Timed out waiting for {description}")


def adp_environment(case, endpoints):
    """ADP's environment from the SMP case, pointed at the case's files and this scenario's intakes."""
    experiment = yaml.safe_load((case / "experiment.yaml").read_text())
    target_dir = str(case / "agent-data-plane")
    env = {key: value for key, value in os.environ.items() if not key.startswith("DD_")}
    for key, value in experiment["target"]["environment"].items():
        env[key] = str(value).replace("/etc/agent-data-plane", target_dir)
    env.pop("STATEFUL_INTAKE_PORTS", None)
    # Logs go to each scenario's adp.log; the default log file's directory may not exist here.
    env["DD_DISABLE_FILE_LOGGING"] = "true"
    addresses = [f"http://127.0.0.1:{FIRST_INTAKE_PORT + index}" for index in range(endpoints)]
    env["DD_DATA_PLANE_STATEFUL_METRICS_ENDPOINT"] = addresses[0]
    env.pop("DD_DATA_PLANE_STATEFUL_METRICS_ADDITIONAL_ENDPOINTS", None)
    if len(addresses) > 1:
        env["DD_DATA_PLANE_STATEFUL_METRICS_ADDITIONAL_ENDPOINTS"] = " ".join(addresses[1:])
    return env, experiment["target"]["environment"].get("DD_DOGSTATSD_SOCKET")


def scrape():
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(TELEMETRY_URL, timeout=2) as response:
        return response.read().decode()


def stateful_totals(telemetry):
    """Sums every `stateful_metrics_*` series across its labels; distributions keep only their sum and count."""
    totals = {}
    for line in telemetry.splitlines():
        line = line.removeprefix(TELEMETRY_PREFIX)
        if not line.startswith("stateful_metrics_") or "quantile=" in line:
            continue
        name, value = line.split("{")[0].split()[0], line.rsplit(maxsplit=1)[-1]
        if name.endswith("_bucket"):
            continue
        totals[name] = totals.get(name, 0.0) + float(value)
    return dict(sorted(totals.items()))


def usage(pid):
    rss, cpu = subprocess.check_output(["ps", "-o", "rss=,%cpu=", "-p", str(pid)], text=True).split()
    return int(rss), float(cpu)


def run_scenario(args, name, output):
    description, faults = SCENARIOS[name]
    output.mkdir()
    processes = []

    def spawn(label, command, env, cpus=None):
        if cpus:
            command = ["taskset", "-c", cpus, *command]
        log = (output / f"{label}.log").open("w")
        process = subprocess.Popen(command, env=env, stdout=log, stderr=subprocess.STDOUT)
        processes.append(process)
        return process

    try:
        for index, fault in enumerate(faults):
            env = {key: value for key, value in os.environ.items() if key != "LISTEN_ADDR"}
            env.update(fault, LISTEN_ADDR=f"127.0.0.1:{FIRST_INTAKE_PORT + index}")
            spawn(f"intake-{index}", [str(args.intake)], env, args.load_cpus)

        env, socket_path = adp_environment(args.case, len(faults))
        if socket_path:
            Path(socket_path).unlink(missing_ok=True)
        adp = spawn("adp", [str(args.adp), "--config", str(args.case / "agent-data-plane/empty.yaml"), "run"], env,
                    args.adp_cpus)
        wait_for(lambda: adp.poll() is None and "Topology healthy." in (output / "adp.log").read_text(), "ADP")

        target = ["--target-pid", str(adp.pid)] if sys.platform == "linux" else ["--no-target"]
        lading = spawn("lading", [
            str(args.lading), "--config-path", str(args.case / "lading/lading.yaml"), *target,
            "--warmup-duration-seconds", "0", "--experiment-duration-seconds", str(args.duration),
            "--capture-path", str(output / "capture.jsonl"),
        ], {key: value for key, value in os.environ.items() if not key.startswith("DD_")}, args.load_cpus)

        samples = []
        started = time.monotonic()
        while lading.poll() is None:
            if adp.poll() is not None:
                raise RuntimeError(f"ADP exited with {adp.returncode} during the run")
            rss_kib, cpu_percent = usage(adp.pid)
            samples.append({"elapsed_seconds": round(time.monotonic() - started, 1),
                            "adp_rss_kib": rss_kib, "adp_cpu_percent": cpu_percent})
            time.sleep(args.sample_seconds)
        telemetry = scrape()
        (output / "telemetry.txt").write_text(telemetry)
        summary = {
            "scenario": name,
            "description": description,
            "case": args.case.name,
            "duration_seconds": args.duration,
            "lading_exit_code": lading.returncode,
            "peak_adp_rss_kib": max((sample["adp_rss_kib"] for sample in samples), default=None),
            "stateful_metrics": stateful_totals(telemetry),
        }
        (output / "samples.json").write_text(json.dumps(samples, indent=2))
        (output / "summary.json").write_text(json.dumps(summary, indent=2))
        return summary
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--adp", required=True, type=Path, help="agent-data-plane binary (release build)")
    parser.add_argument("--intake", required=True, type=Path, help="stateful-metrics-blackhole binary")
    parser.add_argument("--lading", required=True, type=Path, help="lading binary, matching test/smp's version")
    parser.add_argument("--case", type=Path, default=DEFAULT_CASE, help="generated SMP case directory")
    parser.add_argument("--scenario", action="append", choices=sorted(SCENARIOS),
                        help="Run only named scenarios; repeat to select several")
    parser.add_argument("--duration", type=int, default=600, help="Load duration per scenario, in seconds")
    parser.add_argument("--sample-seconds", type=float, default=5, help="ADP RSS and CPU sampling interval")
    parser.add_argument("--output", type=Path, help="New directory for logs, captures, and summaries")
    parser.add_argument("--adp-cpus", help="taskset CPU list for ADP, e.g. 0-3 to match SMP's 4-CPU allotment")
    parser.add_argument("--load-cpus", help="taskset CPU list for lading and the intakes, e.g. 4-9")
    args = parser.parse_args()
    args.case = args.case.resolve()
    args.output = args.output or Path(tempfile.mkdtemp(prefix="stateful-metrics-outages-"))
    args.output.mkdir(parents=True, exist_ok=True)
    print(f"Writing results to {args.output}", flush=True)

    summaries = {}
    for name in args.scenario or SCENARIOS:
        print(f"Running {name}: {SCENARIOS[name][0]}", flush=True)
        try:
            summaries[name] = run_scenario(args, name, args.output / name)
        except Exception as error:
            summaries[name] = {"scenario": name, "error": str(error)}
            print(f"FAILED {name}: {error}", flush=True)
    (args.output / "summary.json").write_text(json.dumps(summaries, indent=2))
    return 1 if any("error" in summary for summary in summaries.values()) else 0


if __name__ == "__main__":
    sys.exit(main())
