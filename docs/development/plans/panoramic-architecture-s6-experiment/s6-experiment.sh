#!/usr/bin/env bash
set -euo pipefail

assets=$(cd "${BASH_SOURCE[0]%/*}" && pwd)
case "${1:-}" in
    run|provenance) ;;
    *) printf 'Usage: %s {run|provenance}\n' "$0" >&2; exit 2 ;;
esac

experiment() {
    python3 -u - "$assets" "$1" <<'PY'
import datetime
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shlex
import statistics
import struct
import subprocess
import sys
import tarfile
import time
import urllib.parse

ASSETS = Path(sys.argv[1])
ROOT = ASSETS.parents[3]
HOST = os.environ.get("DOCKER_HOST", "unix:///var/run/docker.sock")
if not HOST.startswith("unix://"):
    raise SystemExit("DOCKER_HOST must be a unix:// socket")
SOCKET = HOST[7:]
IMAGE = os.environ.get("IMAGE", "saluki-images/datadog-agent:testing-devel")
FIXUP = os.environ.get("FIXUP_IMAGE", "alpine:latest")
LABELS = {"panoramic.experiment": "s6"}
FILTER = {"filters": json.dumps({"label": ["panoramic.experiment=s6"]})}
FAILED = []


def api(method, path, data=None, query=None, raw=False, missing=False, timeout=60):
    url = "http://d" + path
    if query:
        url += "?" + urllib.parse.urlencode(query)
    command = ["curl", "--silent", "--show-error", "--max-time", str(timeout),
               "--unix-socket", SOCKET, "-X", method, "--write-out", "\n%{http_code}", url]
    if data is not None:
        binary = isinstance(data, bytes)
        command += ["-H", "Content-Type: " + ("application/x-tar" if binary else "application/json"),
                    "--data-binary", "@-"]
        data = data if binary else json.dumps(data).encode()
    result = subprocess.run(command, input=data, stdout=subprocess.PIPE, check=True).stdout
    body, status = result.rsplit(b"\n", 1)
    if status == b"404" and missing:
        return None
    if not 200 <= int(status) < 300:
        raise RuntimeError(f"{method} {path}: HTTP {status.decode()}: {body.decode(errors='replace')}")
    return body if raw else (json.loads(body) if body else None)


def leftovers():
    containers = api("GET", "/containers/json", query={"all": 1, **FILTER})
    volumes = api("GET", "/volumes", query=FILTER).get("Volumes") or []
    return [c["Id"] for c in containers], [v["Name"] for v in volumes]


def cleanup():
    errors = []
    containers, volumes = leftovers()
    for path in [f"/containers/{c}" for c in containers] + [f"/volumes/{v}" for v in volumes]:
        try:
            api("DELETE", path, query={"force": 1, "v": 1} if path.startswith("/containers/") else None)
        except Exception as error:
            errors.append(str(error))
    remaining = leftovers()
    check("cleanup leaves no labelled containers or volumes", not errors and remaining == ([], []),
          f"containers={remaining[0]} volumes={remaining[1]} errors={errors}")
    return not FAILED


def check(name, passed, detail=""):
    print(f"{'PASS' if passed else 'FAIL'} {name}")
    if detail:
        print("  " + str(detail).replace("\n", "\n  "))
    if not passed:
        FAILED.append(name)
    return passed


def measure(name, value):
    print(f"MEASURE {name} {value}")


def require_image(ref):
    image = api("GET", f"/images/{ref}/json", missing=True)
    if image is None:
        raise RuntimeError(f"Missing image {ref}; load it into this daemon before running (run never pulls)")
    return image


def create(image, **config):
    return api("POST", "/containers/create", {"Image": image, "Labels": LABELS, **config})["Id"]


def remove(container, volume=None):
    api("DELETE", f"/containers/{container}", query={"force": 1, "v": 1})
    if volume:
        api("DELETE", f"/volumes/{volume}")


def archive(container, path, missing=False):
    blob = api("GET", f"/containers/{container}/archive", query={"path": path}, raw=True, missing=missing)
    if blob is None:
        return b""
    with tarfile.open(fileobj=io.BytesIO(blob)) as tar:
        files = [m for m in tar.getmembers() if m.isfile()]
        if len(files) != 1:
            raise RuntimeError(f"Expected one file in archive of {path}: {files}")
        return tar.extractfile(files[0]).read()


def execute(container, command, allow_failure=False):
    # TTY exec returns raw output instead of Docker's framed streams.
    exec_id = api("POST", f"/containers/{container}/exec", {
        "AttachStdout": True, "AttachStderr": True, "Tty": True, "Cmd": ["sh", "-c", command]})["Id"]
    output = api("POST", f"/exec/{exec_id}/start", {"Detach": False, "Tty": True}, raw=True).decode()
    state = api("GET", f"/exec/{exec_id}/json")
    if state["Running"] or (state["ExitCode"] != 0 and not allow_failure):
        raise RuntimeError(f"Exec {command!r}: {state['ExitCode']}: {output}")
    return output.replace("\r\n", "\n").strip()


def live_file(container, path):
    return execute(container, f"if test -f {shlex.quote(path)}; then cat {shlex.quote(path)}; fi")


def records(container, service="data-plane", stopped=False):
    path = f"/airlock/adp/exit/{service}"
    text = archive(container, path, missing=True).decode() if stopped else live_file(container, path)
    result = []
    for line in text.splitlines():
        if not re.fullmatch(r"\d+ \d+ \d+\.\d{9}", line):
            raise RuntimeError(f"Malformed exit record: {line!r}")
        code, signal, timestamp = line.split()
        result.append((int(code), int(signal), float(timestamp)))
    return result


def status(container, service="data-plane"):
    return execute(container, f"s6-svstat /var/run/s6/services/{service}")


def pid(container, service="data-plane"):
    match = re.search(r"up \(pid (\d+)\)", status(container, service))
    return int(match[1]) if match else None


def poll(description, action, timeout=45):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = action()
        if result:
            return result
        time.sleep(0.25)
    raise RuntimeError(f"Timed out waiting for {description}")


def logs(container, stream):
    data = api("GET", f"/containers/{container}/logs", query={
        "stdout": int(stream == 1), "stderr": int(stream == 2)}, raw=True)
    result = bytearray()
    while data:
        if len(data) < 8:
            raise RuntimeError("Truncated Docker log frame")
        kind, size = data[0], struct.unpack(">I", data[4:8])[0]
        if kind != stream or len(data) < 8 + size:
            raise RuntimeError(f"Invalid Docker log frame: stream={kind}, size={size}")
        result.extend(data[8:8 + size])
        data = data[8 + size:]
    return result.decode(errors="replace")


def init_events(container):
    text = archive(container, "/var/log/datadog/init.log").decode()
    events = []
    for line in text.splitlines():
        match = re.match(r"(\d{4}-\d\d-\d\d \d\d:\d\d:\d\d\.\d+) (.*)", line)
        if match:
            # Parse microseconds for Python versions that reject nanosecond fractions.
            stamp = datetime.datetime.fromisoformat(match[1][:26]).replace(tzinfo=datetime.timezone.utc).timestamp()
            events.append((stamp, match[2]))
    if not events:
        raise RuntimeError(f"No timestamped init.log lines: {text[:500]}")
    return events


def restart_durations(container, exits):
    starts = [t for t, line in init_events(container) if line == "starting agent-data-plane"]
    return [min(t for t in starts if t > rec[2]) - rec[2] for rec in exits if any(t > rec[2] for t in starts)]


def overlay(container, scenario):
    files, expected_finishes = {}, {}
    for service, process in [("data-plane", "adp"), ("agent", "agent")]:
        shipped = archive(container, f"/etc/services.d/{service}/finish")
        source = shipped
        if service == "data-plane":
            source = (ROOT / "docker/s6-services/data-plane/finish").read_bytes()
            if not check(f"{scenario} image data-plane finish matches the repository body", source == shipped):
                raise RuntimeError("Use a subject image whose data-plane finish matches docker/s6-services/data-plane/finish")
        shebang, body = source.split(b"\n", 1)
        record = (ASSETS / "overlay/record-exit").read_bytes().replace(b"@SERVICE@", service.encode())
        prefix = shebang + b"\n" + record
        expected_finishes[service] = (prefix, shipped.split(b"\n", 1)[1])
        composed = prefix + body
        if scenario == "S3" and service == "data-plane":
            composed = prefix + b"s6-sleep 30\n"
        files[f"{service}/finish"] = composed
        files[f"{service}/log/run"] = (ASSETS / "overlay/log-run").read_bytes().replace(
            b"@PROCESS@", process.encode())
    if scenario == "S4":
        for process in ["probe", "probe-merged"]:
            files[f"panoramic-{process}/run"] = (ASSETS / f"overlay/{process}-run").read_bytes()
            files[f"panoramic-{process}/log/run"] = (ASSETS / "overlay/log-run").read_bytes().replace(
                b"@PROCESS@", process.encode())
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w") as tar:
        directories = sorted({str(parent) for name in files for parent in Path(name).parents if str(parent) != "."})
        for name in directories:
            info = tarfile.TarInfo(name)
            info.type, info.mode, info.uid, info.gid = tarfile.DIRTYPE, 0o755, 0, 0
            tar.addfile(info)
        for name, content in files.items():
            info = tarfile.TarInfo(name)
            info.size, info.mode, info.uid, info.gid = len(content), 0o755, 0, 0
            tar.addfile(info, io.BytesIO(content))
    api("PUT", f"/containers/{container}/archive", buffer.getvalue(), query={"path": "/etc/services.d"})
    for name, content in files.items():
        if archive(container, f"/etc/services.d/{name}") != content:
            raise RuntimeError(f"Overlay upload differs: {name}")
    return expected_finishes


def subject(scenario, crash=False):
    print(f"SCENARIO {scenario}")
    volume = api("POST", "/volumes/create", {"Labels": LABELS})["Name"]
    host = {"Binds": [f"{volume}:/airlock"], "PidMode": "host"}
    fixup = create(FIXUP, HostConfig=host, Cmd=["sh", "-c",
        "mkdir -p /airlock/adp/exit /airlock/adp/log /airlock/adp/shared && chmod -R 777 /airlock"])
    api("POST", f"/containers/{fixup}/start")
    result = api("POST", f"/containers/{fixup}/wait")
    if result["StatusCode"] != 0:
        raise RuntimeError(f"Volume preparation failed: {result}")
    remove(fixup)
    # /initlog.sh uses s6-tai64nlocal, so fix its timezone for interval calculations.
    env = ["PANORAMIC_PARTICIPANT=adp", "DD_API_KEY=" + "0" * 32, "DD_HOSTNAME=s6-experiment",
           "DD_DATA_PLANE_ENABLED=true", "DD_DATA_PLANE_STANDALONE_MODE=false", "TZ=UTC"]
    if crash:
        env.append("DD_HEROKU_DYNO=true")
    container = create(IMAGE, HostConfig=host, Hostname="adp", Env=env, Tty=False)
    expected_finishes = overlay(container, scenario)
    api("POST", f"/containers/{container}/start")
    for service, (prefix, body) in expected_finishes.items():
        if scenario == "S3" and service == "data-plane":
            print("INFO S3 data-plane finish replaced by timeout variant")
            continue
        active = poll(f"active {service} finish", lambda: archive(
            container, f"/var/run/s6/services/{service}/finish", missing=True))
        check(f"{scenario} active {service} finish has the record prefix and shipped body byte for byte",
              active.startswith(prefix) and active[len(prefix):] == body)
    return container, volume


def stopped_state(container):
    state = api("GET", f"/containers/{container}/json")["State"]
    return state if not state["Running"] else None


def stop(container):
    before = float(execute(container, "date +%s.%N"))
    api("POST", f"/containers/{container}/stop", query={"t": 10}, timeout=20)
    state = stopped_state(container)
    # Parse microseconds for Python versions that reject nanosecond fractions.
    finished = datetime.datetime.fromisoformat(state["FinishedAt"][:26]).replace(tzinfo=datetime.timezone.utc).timestamp()
    return finished - before, state, before


def signal_service(container, signal, service="data-plane"):
    process = pid(container, service)
    if not process:
        raise RuntimeError(f"{service} is not up: {status(container, service)}")
    timestamp = float(execute(container, f"date +%s.%N; kill -{signal} {process}"))
    return process, timestamp


def healthy(container, minimum_age=20, started_count=1):
    def ready():
        state = status(container)
        match = re.search(r"up \(pid (\d+)\) (\d+) seconds", state)
        text = live_file(container, "/airlock/adp/log/adp.log")
        return state if (match and int(match[2]) >= minimum_age and
                         text.count("Agent Data Plane running.") >= started_count) else None
    try:
        return poll("started ADP", ready, timeout=60)
    except RuntimeError:
        print("  ADP status=" + status(container))
        print("  ADP output=" + live_file(container, "/airlock/adp/log/adp.log")[-6000:])
        raise


def provenance():
    tag = os.environ.get("TAG") or re.search(
        r"^ARG DD_AGENT_VERSION=(\S+)", (ROOT / "docker/Dockerfile.datadog-agent").read_text(), re.M)[1]
    ref = f"registry.datadoghq.com/agent:{tag}"
    require_image(IMAGE)
    existed = api("GET", f"/images/{ref}/json", missing=True) is not None
    before_ids = {i["Id"] for i in api("GET", "/images/json", query={"all": 1})}
    try:
        if not existed:
            print(f"PULL {ref}")
            output = api("POST", "/images/create", query={"fromImage": "registry.datadoghq.com/agent", "tag": tag},
                         raw=True, timeout=240)
            for line in output.splitlines():
                item = json.loads(line)
                if "error" in item:
                    raise RuntimeError(item["error"])
        else:
            print(f"REUSE {ref}")
        hashes = {}
        for name, image_ref in [("pinned", ref), ("subject", IMAGE)]:
            info = require_image(image_ref)
            labels = info["Config"].get("Labels") or {}
            print(f"IMAGE {name} ref={image_ref} id={info['Id']} digests={info.get('RepoDigests')}")
            for key in ["org.opencontainers.image.version", "org.opencontainers.image.revision"]:
                print(f"LABEL {name} {key}={labels.get(key)}")
            container = create(image_ref)
            try:
                paths = ["/etc/services.d/agent/finish", "/etc/s6/init/init-stage2", "/etc/s6/init/init-stage3"]
                if name == "subject":
                    paths.append("/etc/services.d/data-plane/finish")
                for path in paths:
                    content = archive(container, path)
                    hashes[name, path] = hashlib.sha256(content).hexdigest()
                    print(f"SHA256 {name} {path} {hashes[name, path]}")
                    if name == "pinned" and path.endswith("agent/finish"):
                        (ROOT / "bin/correctness/panoramic/participants/src/agent_adp/assets/s6/agent-finish").write_bytes(content)
            finally:
                remove(container)
        for path in ["/etc/services.d/agent/finish", "/etc/s6/init/init-stage2", "/etc/s6/init/init-stage3"]:
            check(f"provenance hashes match for {path}", hashes["pinned", path] == hashes["subject", path])
        print("COPY_DATE " + datetime.datetime.now(datetime.timezone.utc).date().isoformat())
    finally:
        if not existed:
            pulled = api("GET", f"/images/{ref}/json", missing=True)
            if pulled:
                api("DELETE", f"/images/{ref}")
                remains = api("GET", f"/images/{pulled['Id']}/json", missing=True)
                check("provenance removes the image it pulled", remains is None or pulled["Id"] in before_ids)


def run():
    for ref in [IMAGE, FIXUP]:
        info = require_image(ref)
        print(f"IMAGE {ref} {info['Id']} version={(info['Config'].get('Labels') or {}).get('org.opencontainers.image.version')}")
    version = api("GET", "/version")
    print("DAEMON " + json.dumps({k: version[k] for k in ["Version", "ApiVersion", "Os", "Arch", "KernelVersion"]}))

    c, v = subject("S1", crash=True)
    poll("ADP crash records", lambda: len(records(c)) >= 2)
    early_adp = live_file(c, "/airlock/adp/log/adp.log")
    early_agent = live_file(c, "/airlock/adp/log/agent.log")
    time.sleep(10)
    exits = records(c)
    print("  exit file excerpt: " + "; ".join(live_file(c, "/airlock/adp/exit/data-plane").splitlines()[:3]))
    events = init_events(c)
    finishes = [t for t, line in events if "exited with code 1, signal 0" in line]
    check("S1 an ADP exit with code 1 appends a record before the shipped finish body runs",
          len(exits) >= 3 and len(finishes) >= len(exits) and
          all(r[:2] == (1, 0) and 0 <= f - r[2] < 0.5 for r, f in zip(exits, finishes)),
          f"records={exits[:3]}; finish_log_times={finishes[:3]}")
    durations = restart_durations(c, exits)
    intervals = [b[2] - a[2] for a, b in zip(exits, exits[1:])]
    check("S1 ADP is restarted about two seconds after each nonzero exit",
          len(durations) >= 2 and all(1.8 < t < 3.0 for t in durations), f"record_to_start={durations}")
    shipped = statistics.median(durations)
    measure("restart_interval_seconds_median", f"{statistics.median(intervals):.6f}")
    measure("shipped_finish_seconds_median", f"{shipped:.6f}")
    stdout, stderr = logs(c, 1), logs(c, 2)
    adp = live_file(c, "/airlock/adp/log/adp.log")
    agent = live_file(c, "/airlock/adp/log/agent.log")
    adp_lines = [line for line in adp.splitlines() if "ERROR" in line and line in stdout]
    agent_lines = [line for line in agent.splitlines() if "CORE" in line and line in stdout]
    check("S1 adp.log grows while the container log still shows the same ADP lines",
          len(adp) > len(early_adp) and bool(adp_lines),
          f"bytes={len(early_adp)}->{len(adp)}; shared_stdout={adp_lines[:1]}")
    check("S1 agent.log grows while the container log still shows the same Agent lines",
          len(agent) > len(early_agent) and bool(agent_lines),
          f"bytes={len(early_agent)}->{len(agent)}; shared_stdout={agent_lines[:1]}")
    stderr_lines = [line for line in stderr.splitlines() if line.strip()]
    print(f"  container stderr lines={len(stderr_lines)}")
    if stderr_lines:
        check("S1 adp.log contains ADP's stdout and not container stderr",
              bool(adp_lines) and all(line not in adp for line in stderr_lines),
              f"stderr excerpts={stderr_lines[:2]}")
    else:
        print("SKIP S1 adp.log contains ADP's stdout and not container stderr (container stderr empty; see S4)")
    check("S1 adp.log also contains the run and finish scripts' own log lines",
          "starting agent-data-plane" in adp and "exited with code 1, signal 0, restarting in 2 seconds" in adp)
    # Hold the short-lived crash-loop process alive until the stop request arrives.
    paused = execute(c, "s6-svwait -u -t 5000 /var/run/s6/services/data-plane; "
                     "set -- $(s6-svstat /var/run/s6/services/data-plane); "
                     "test \"$1\" = up && kill -STOP \"${3%)}\" && echo paused_pid=\"${3%)}\"")
    print("  hold the crash-loop process alive until stop: " + paused)
    duration, state, before_stop = stop(c)
    stopped_exits = records(c, stopped=True)
    check("S1 stopping the container while ADP is running appends a signal record",
          any(r[:2] == (256, 15) and r[2] >= before_stop for r in stopped_exits[len(exits):]) and
          duration < 10 and state["ExitCode"] == 0,
          f"before_stop={before_stop:.9f}; last_record={stopped_exits[-1]}; container_exit={state['ExitCode']}")
    measure("stop_seconds_crash_loop_held", f"{duration:.6f}")
    check("S1 exit records and process logs are readable from the stopped container",
          bool(stopped_exits) and bool(records(c, "agent", stopped=True)) and all(
              archive(c, f"/airlock/adp/log/{p}.log") for p in ["adp", "agent"]))
    remove(c, v)

    c, v = subject("S2")
    print("  healthy=" + healthy(c))
    startup = next(line for line in live_file(c, "/airlock/adp/log/adp.log").splitlines()
                   if "Agent Data Plane starting" in line)
    print("  ADP version line: " + startup)
    old_pid, killed_at = signal_service(c, "KILL")
    killed = poll("SIGKILL record", lambda: next((r for r in records(c) if r[:2] == (256, 9)), None))
    new_pid = poll("new ADP pid", lambda: (p if (p := pid(c)) and p != old_pid else None))
    check("S2 SIGKILL to ADP records 256 9 and s6 restarts it with a new pid",
          killed[:2] == (256, 9) and new_pid != old_pid, f"record={killed}; pid={old_pid}->{new_pid}")
    measure("kill_to_record_seconds", f"{killed[2] - killed_at:.6f}")
    print("  restarted=" + healthy(c, started_count=2))
    signal_service(c, "TERM")
    term = poll("SIGTERM record", lambda: records(c) if len(records(c)) >= 2 else None)
    time.sleep(10)
    after_term, term_state = records(c), status(c)
    up_wantedup = execute(c, "s6-svstat -o up,wantedup /var/run/s6/services/data-plane")
    check("S2 SIGTERM to a started ADP records its exit code and the shipped finish disables the service when the code is 0",
          term[-1][:2] == (0, 0) and after_term == term and up_wantedup == "false false",
          f"record={term[-1]}; {term_state}; up,wantedup={up_wantedup}; records unchanged for 10 s")
    execute(c, "s6-svc -u /var/run/s6/services/data-plane")
    print("  re-enabled for Agent-exit shutdown=" + healthy(c, minimum_age=2, started_count=3))
    signal_service(c, "KILL", "agent")
    state = poll("container stops after Agent exit", lambda: stopped_state(c), timeout=20)
    agent_exits, shutdown_exits = records(c, "agent", stopped=True), records(c, stopped=True)
    for service in ["data-plane", "agent"]:
        print(f"  stopped {service} records: " + archive(c, f"/airlock/adp/exit/{service}").decode().strip().replace("\n", "; "))
    check("S2 SIGKILL to the Agent records 256 9 for agent and the container stops itself",
          agent_exits[-1][:2] == (256, 9) and len(shutdown_exits) > len(after_term),
          f"agent={agent_exits[-1]}; container_exit={state['ExitCode']}; data-plane={shutdown_exits[-1]}")
    check("S2 records are readable after the container stopped itself", bool(agent_exits) and bool(shutdown_exits))
    remove(c, v)

    c, v = subject("S3", crash=True)
    first = poll("overrunning finish record", lambda: records(c))
    timeout_file = execute(c, "if test -e /var/run/s6/services/data-plane/timeout-finish; then "
                           "cat /var/run/s6/services/data-plane/timeout-finish; else echo absent; fi")
    poll("ADP restart after finish timeout", lambda: len(records(c)) >= 2, timeout=15)
    durations = restart_durations(c, first)
    finish_timeout = durations[0]
    check("S3 a finish script that overruns is killed and ADP is restarted, and its record is kept",
          records(c)[0] == first[0] and 4.5 < finish_timeout < 6.5,
          f"first_record={first[0]}; timeout-finish={timeout_file}; record_to_next_start={finish_timeout:.6f}")
    measure("finish_timeout_seconds", f"{finish_timeout:.6f}")
    measure("finish_margin_seconds", f"{finish_timeout - shipped:.6f}")
    print("  Exit-to-record is bounded above by S2 kill-to-record; no independent process-exit timestamp is available.")
    remove(c, v)

    c, v = subject("S4")
    poll("probe logs", lambda: "panoramic-probe-merged-stderr" in live_file(c, "/airlock/adp/log/probe-merged.log"))
    probe = live_file(c, "/airlock/adp/log/probe.log")
    merged = live_file(c, "/airlock/adp/log/probe-merged.log")
    stdout, stderr = logs(c, 1), logs(c, 2)
    check("S4 s6 pipes only stdout to a service logger",
          probe == "panoramic-probe-stdout" and "panoramic-probe-stdout" in stdout and
          "panoramic-probe-stderr" in stderr and "panoramic-probe-stderr" not in stdout,
          f"probe.log={probe!r}; container stderr contains panoramic-probe-stderr")
    check("S4 redirecting stderr in the run script sends both streams to the logger",
          merged == "panoramic-probe-merged-stdout\npanoramic-probe-merged-stderr" and
          all(line in stdout and line not in stderr for line in merged.splitlines()), f"probe-merged.log={merged!r}")
    remove(c, v)

    c, v = subject("S5")
    print("  healthy=" + healthy(c, minimum_age=2))
    # Root can bypass mode 000; removing the directory forces the write to fail.
    execute(c, "rm -rf /airlock/adp/exit")
    old_pid, killed_at = signal_service(c, "KILL")
    new_pid = poll("restart despite record failure", lambda: (p if (p := pid(c)) and p != old_pid else None))
    text = live_file(c, "/airlock/adp/log/adp.log")
    stderr = logs(c, 2)
    starts = [t for t, line in init_events(c) if line == "starting agent-data-plane" and t > killed_at]
    write_errors = [line for line in stderr.splitlines() if "/airlock/adp/exit/data-plane" in line]
    check("S5 a failed record write does not change restart behaviour",
          new_pid != old_pid and "exited with code 256, signal 9, restarting in 2 seconds" in text and
          bool(write_errors) and not records(c) and starts and 1.8 < starts[0] - killed_at < 3.0,
          f"pid={old_pid}->{new_pid}; kill_to_start={starts[0] - killed_at:.6f}; stderr={write_errors}")
    check("S5 record-write errors reach container stderr only",
          bool(write_errors) and all(line not in text and line not in logs(c, 1) for line in write_errors),
          f"adp.log excludes {write_errors}")
    remove(c, v)

    c, v = subject("S6")
    print("  healthy before container stop=" + healthy(c))
    before_exits = {service: records(c, service) for service in ["data-plane", "agent"]}
    duration, state, before_stop = stop(c)
    after_exits = {service: records(c, service, stopped=True) for service in before_exits}
    for service in after_exits:
        print(f"  healthy-stop {service} records: " + archive(
            c, f"/airlock/adp/exit/{service}").decode().strip().replace("\n", "; "))
    check("S6 stopping a healthy container records clean exits for ADP and Agent",
          all(not before_exits[service] and len(exits) == 1 and exits[0][:2] == (0, 0) and
              exits[0][2] >= before_stop for service, exits in after_exits.items()) and
          duration < 10 and state["ExitCode"] == 0,
          f"before_stop={before_stop:.9f}; container_exit={state['ExitCode']}")
    measure("stop_seconds_healthy", f"{duration:.6f}")
    remove(c, v)


try:
    command = sys.argv[2]
    if command == "preflight":
        containers, volumes = leftovers()
        if containers or volumes:
            print("Refusing to run: labelled resources already exist. Review and remove them with:", file=sys.stderr)
            for path in [f"/containers/{c}?force=1&v=1" for c in containers] + [f"/volumes/{v}" for v in volumes]:
                print(f"curl --unix-socket {shlex.quote(SOCKET)} -X DELETE {shlex.quote('http://d' + path)}", file=sys.stderr)
            sys.exit(1)
    elif command == "cleanup":
        cleanup()
    elif command == "provenance":
        provenance()
    elif command == "run":
        run()
    else:
        raise RuntimeError(f"Unknown internal command: {command}")
except Exception as error:
    print(f"FAIL experiment: {error}", file=sys.stderr)
    sys.exit(1)
sys.exit(bool(FAILED))
PY
}

experiment preflight
trap 'result=$?; trap - EXIT; experiment cleanup || result=1; exit "$result"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
experiment "$1"
