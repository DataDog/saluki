# Fuzz harness — agent notes

Brief for the next agent picking up the ADP fuzz harness.

## Goal

End-to-end fuzz of agent-data-plane (ADP) against a synthetic DogStatsD corpus. Every iteration drives the **full** ADP topology — source → transforms → aggregator → encoders → forwarder → intake — entirely in-process under **paused tokio** (auto-advance) so iterations cost ~200 ms wall clock at ~5 exec/s. Surfaces crashes/panics in the pipeline.

## Architecture

Three coupled tricks make paused-tokio auto-advance behave. Each is necessary; remove any and iterations stretch to many real seconds. Documented in `bin/agent-data-plane/src/fuzz.rs` (module docs, top of file).

1. **In-memory transport on both edges of ADP.** Neither leg goes through the kernel.
   - **Input**: DogStatsD source is fed by `Listener::in_process(mpsc::Receiver<(Bytes, ConnectionAddress)>)` (see `lib/saluki-io/src/net/listener.rs`). Backed by an `mpsc` channel; injection pushes packets, source pulls them.
   - **Output**: Datadog forwarder hits an `InProcessHandler` (see `lib/saluki-io/src/net/client/http/conn.rs`) that hands `tokio::io::DuplexStream` halves to a spawned hyper-server task running the intake's axum router (see `bin/correctness/datadog-intake/src/lib.rs`, `build_intake()`).
2. **Ambient worker pool** for the topology. `built_topology.with_ambient_worker_pool()` in `bin/agent-data-plane/src/fuzz_run.rs` so components inherit `start_paused`.
3. **Tokio-anchored unix clock**. `lib/saluki-common/src/time.rs::get_unix_timestamp` anchors on `tokio::time::Instant` so the aggregator's bucket logic advances with simulated time.

## Key files

| File | What it does |
|--|--|
| `bin/agent-data-plane/src/fuzz.rs` | Main harness. `inner(corpus)`, `inject_plan`, `build_config_object`. |
| `bin/agent-data-plane/src/fuzz_run.rs` | `handle_run_command` — threads the injected listener + intake handler into the topology blueprint. |
| `bin/agent-data-plane/src/integration_small_test.rs` | Debug binary. Runs `inner()` once with a random corpus. Use this for fast iteration. |
| `fuzz/fuzz_targets/apd.rs` | libfuzzer entry. `Arbitrary` impl decodes the tape via `barkus_core` against the grammar at `fuzz/dogstatsd_multi_offset.ebnf`. Returns `Error::IncorrectFormat` for empty corpora to skip. |
| `lib/saluki-io/src/net/{listener,stream,addr}.rs` | `Listener::in_process(rx)` + `ListenAddress::InProcess` + `Connectionless::InProcess`. |
| `lib/saluki-components/src/sources/dogstatsd/mod.rs` | `DogStatsDConfiguration::with_extra_listener` lets the test inject a pre-built listener at config time. |
| `lib/saluki-components/src/transforms/aggregate/mod.rs` | Aggregator. Default 10 s window, 15 s primary flush, 300 s counter expiry (zero-fill for counters only). |
| `bin/correctness/datadog-intake/src/lib.rs` | `build_intake() -> (Router, IntakeSignals)`. `IntakeSignals.series_v2_count: watch::Receiver<usize>`. |

## Commands

```bash
# Fast smoke test (random 50-metric corpus, debug build). ~350 ms.
cargo run --bin integration_small_test

# Same but accurate timing — avoids cargo overhead.
cargo build --bin integration_small_test
time ./target/debug/integration_small_test

# Release mode (changes task-scheduling order under paused tokio — see "Watch out for").
cargo build --release --bin integration_small_test
time ./target/release/integration_small_test

# Fuzz. The script sets RUSTFLAGS='--cfg tokio_unstable' and standard libfuzzer flags.
./scripts/fuzz_run.sh -max_total_time=30        # 30 sec sanity check
./scripts/fuzz_run.sh -max_total_time=600       # 10 min
./scripts/fuzz_run.sh -max_total_time=7200      # 2 h
# Don't pass shorter timeouts via cargo flags — the script appends $@ after fuzz/corpus.
```

## Watch out for

- **The 40-sec simulated-timeout on the intake-payload wait** (`fuzz.rs::inner`) currently *silences* inputs that never produce a payload within the budget. This bounds iteration cost but **also masks real pipeline correctness issues**. If you change this, make sure you panic on timeout so libfuzzer captures the input. Don't quietly remove the timeout — some inputs WILL legitimately need it as an escape hatch.
- **Global statics persist across fuzz iterations.** `BOOTSTRAP_GUARD` (in `fuzz_run.rs`) and `TOKIO_UNIX_ANCHOR` (in `saluki-common/src/time.rs`) are process-wide `OnceLock`s. They're initialized on iteration 1 and reused. This is intentional — re-bootstrapping per iteration would re-initialize logging/TLS/metrics — but it means the unix-clock anchor is a real wall-clock instant captured during iteration 1, and `tokio::time::Instant::now()` in later iterations is from a different paused-runtime epoch. Has not caused visible problems yet but is a footgun if you start asserting on absolute unix timestamps.
- **Don't confuse pre-existing artifacts with new ones.** `ls -la fuzz/artifacts/apd/` shows file dates. Anything older than your fuzz run start is irrelevant.
- **Fuzz target output goes to stdout, not stderr.** `2>&1` in `scripts/fuzz_run.sh` merges them. Watch for the `SUMMARY:` / `ERROR:` lines from libfuzzer's signal handler — these indicate a panic.

## Aggregator behavior cheat sheet

- **Window**: 10 s (default). Metrics aggregate within a window.
- **Flush interval**: 15 s (default). Each tick flushes closed buckets.
- **Counter zero-fill**: counters only, default 300 s expiry (`counter_expiry_seconds` / `dogstatsd_expiry_seconds`). After last update, counter contexts are kept alive and emit a zero on each flush for up to 300 s. **Gauges, histograms, distributions, sets, timers do not re-emit** — they're dropped after one flush.
- **Empty state skips flush entirely**: `if !self.state.is_empty() { ... flush ... }`. A corpus that decodes to zero metrics produces zero payloads. The fuzz target rejects these via `IncorrectFormat` to avoid hangs.

## Known oddities

- Saluki's default DSD prefix blocklist (`statsd_metric_namespace_blocklist`) includes short prefixes like `jvm`, `hive` that the 1–4-letter grammar names could match. The default config passes these unmodified to the aggregator — verify before relying on names making it through.

## Recent history (context)

- The harness was originally end-to-end via UDS + TCP loopback to a subprocess intake.
- Moved intake in-process via `InProcessHandler` + `DuplexStream` (output side).
- Moved source in-process via `Listener::in_process` + mpsc channel (input side).
- Briefly experimented with "wait until intake observes all input metric names" (intake exposed `collected_names: watch::Receiver<HashSet<String>>`) — reverted in favor of the simpler "first payload or 40 s timeout".
- The wait condition has been through: `>=2 payloads`, `>=1 payload`, `>=2 payloads`, `sleep 20s + assert >=1`, and finally `timeout(40s, wait_for(>=1))`. Don't go backwards without re-reading the rationale in the module docs.
