# Checks telemetry over FIT

Agent Check Runner (ACR) sends check metrics, logs, service checks, and events to Agent Data Plane (ADP) through a
single-producer, single-consumer FIT shared-memory ring. ADP owns the ring and its setup listener. ACR connects to that
listener, completes the versioned setup handshake, and publishes records to shared memory. The setup socket closes after
the handshake; it does not carry check payloads or monitor the peer. Core Agent configuration, status, and telemetry
connections continue to use their existing gRPC services.

## Configure both processes

Set `checks_ipc_endpoint` in ADP and `check_runner.endpoints.ipc.endpoint` in ACR to the same setup address. Both default
to `tcp:127.0.0.1:5101`. The TCP address must be loopback. For a Unix socket, use `unix:/absolute/path` in both places
and ensure the parent directory is accessible to both processes. The two processes must run on the same host as the same
OS user. A gRPC URI such as `http://127.0.0.1:5101` is not a FIT endpoint.

ADP's `checks_ipc_ring_capacity_bytes` controls the shared ring allocation. It defaults to `1048576` (1 MiB), must be a
multiple of eight, and must be between 16 bytes and 1 GiB. The ring needs room for a complete record, including its
header and alignment. Increasing capacity absorbs longer bursts at the cost of shared memory; it does not raise the
maximum single-record size beyond the ring's usable capacity. ACR does not allocate or configure the ring.

For an ACR sibling checkout, keep the Saluki checkout at `../saluki` relative to ACR: ACR's workspace manifest uses
relative path dependencies on `../saluki/lib/saluki-fit` and `../saluki/lib/datadog-agent/checks-protocol`. Build the two
repositories from mutually compatible feature revisions. FIT on macOS requires macOS 14.4 or newer; Linux and macOS
little-endian x86-64 and AArch64 are supported by the transport.

## Start and stop a session

Start ADP before ACR. ADP binds the setup endpoint; ACR connects to it. Both sides have a fixed 60-second setup deadline.
An occupied port or socket path prevents ADP from starting the Checks source; an absent listener eventually makes ACR
setup fail. Neither side silently chooses another endpoint. Both components become ready only after successful setup.

ACR converts each topology batch to the [Checks wire contract](../../lib/datadog-agent/checks-protocol/protocol/checks-fit.md)
and publishes its fitting ordered prefix in one operation. The destination reports the accepted count and drops the
unaccepted suffix. It does not retry a published prefix, including when the subsequent wake operation fails. ADP reads
and decodes records on a worker thread, then hands owned events to its bounded async pipeline. If downstream stops
accepting data, ADP stops reading; the ring can fill, and ACR then drops new records. A record is reclaimed once ADP has
read it from the ring, before downstream delivery.

Shut down both processes to close a session. Local cancellation wakes an idle FIT receive so ADP can stop cleanly. A
peer exit after setup is not detected by a heartbeat or the closed setup socket. There is no automatic reconnection,
fallback, or replay. If either process exits or the session fails, restart **both** processes to establish a fresh ring.
After changing endpoint or capacity settings, restart both processes as well.

## Measure a transport change

Use release builds of the paired ACR and ADP revisions and preserve their commit IDs, configuration, compiler version,
macOS version, architecture, and hardware in each report. Compare the previous Checks gRPC implementation with FIT using
the same deterministic *logical* payloads, logging settings, delivery boundary, and machine. A transport-only microbenchmark
cannot establish the CPU cost of the application integration. The FIT result also includes the custom codec change; the
comparison does not isolate transport from encoding.

Measure sparse traffic, steady traffic below saturation, and burst traffic that saturates the receiver. Include scalar
metrics with short and larger tag sets, a mix of all four payload kinds, and larger log/event strings. For each workload
and transport, run five measured repetitions with a five-second warmup and a 30-second measurement interval. Measure
setup duration separately. During the primary CPU runs, avoid per-record latency tracing. Repeat with equivalent
producer and consumer timestamps for p50/p95/p99 latency; disclose the instrumentation overhead. Report combined ACR and
ADP CPU time or utilization, accepted and delivered throughput, intentional drops, and memory for each process.

Reconcile produced, accepted, decoded, rejected, and forwarded counts for non-saturating runs. Report saturation loss
separately: lower CPU from dropping more data is not an efficiency improvement. Compare CPU at equal delivered rates
where possible. Do not claim a performance win until both baseline and FIT measurements, including their delivery
counts, are available.

The currently committed cross-process test exercises the ACR sender against a typed FIT consumer in a separate process.
It does not run the full ACR/ADP topology or provide a gRPC performance baseline. Complete that host-level acceptance
harness and preserve a runnable gRPC baseline before publishing an application-level benchmark result.
