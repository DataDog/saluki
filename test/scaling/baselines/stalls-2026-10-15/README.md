# What stalls admission at a flush, 2026-10-15

The runs behind "What stalls admission at a flush" in `docs/design/programmable-metrics-next.md` and G9 in
`docs/design/adp-efficiency-findings.md`. `main` is built from `a52c25dd52`; `plan` (`plan-flow` in `runs.csv`) is the
plan with the flow trace experiment applied (`flow-trace.patch`). One cell at a time, on the N1 host.

| Path | What |
|---|---|
| `flow/` | heavy at 0.3, 0.6, and 1.2 and ultraheavy at 1.2, main and plan, two rounds, 60-second windows, with the flow trace and the host's 100 ms samples (`SCALING_FLOW=1`): `runs.csv` and `summary.md`; `drains.txt`, every drain the trace recorded (`<run> d <start_unix_ms> <region> <duration_us> <steps> <dispatch_wait_us> <events_out> <queued_at_start> <queued_at_end>`); `flushes.txt`, per-flush effects (`tools/flush.py`); `profiles-around-flush.txt`, 100 ms profiles around a flush at heavy 0.6 and 1.2 |
| `profiles/` | heavy 1.2, main and plan, 62 seconds of instruction samples with wall-clock alignment (four flushes each): `flush-attribution.txt`, the instructions a flush adds by owning component (`tools/flushprof2.py`); `output-volume.txt`, series flushed and encoded per flush from the telemetry (`tools/outputs.py`, also over a shorter run at heavy 0.6 and 1.2) |
| `flow-trace.patch` | the instrumentation, not kept in the tree: a `flow_trace` module in `saluki-core` (each dispatcher output's time waiting to send, each region's input queue depth, and each drain, written every 100 ms to the file `SALUKI_EXPERIMENT_TRACE` names) and `run-limits.sh`'s `SCALING_FLOW=1` (the trace, and a 100 ms host sampler of CPU time, throttling, lading's bytes, and whether datagrams wait at the socket). It applies to `7c7631a4bb` with `git apply`. |
| `tools/` | the analysis scripts |

Caveats, also in the documents:

- **`flushes.txt`'s extra CPU and throttling figures are confounded** by lading's one-second pacing (it sends each
  second's bytes in about half a second, then idles), which shifts the windows they compare; `main`'s rows also anchor
  on CPU peaks, which proved unreliable. The drain figures, the 100 ms profiles, and the attribution in `profiles/`
  (anchored on drains and on the encoder's onset) are the reliable results.
- **The socket column is not a depth:** for a Unix datagram socket `ss` reports the size of the first datagram
  waiting, so it says only whether datagrams wait.
- **Throttled time is summed over CPUs** (cgroup v2), up to four times wall time on ADP's four cores.
- The per-second series, ADP's logs, the `perf` data, and the raw traces stayed on the measuring host.
