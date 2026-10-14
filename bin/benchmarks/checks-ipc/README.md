# Checks IPC throughput benchmark

The [macOS FIT versus gRPC measurements](RESULTS.md) include isolated transport and whole-application comparisons.

This host-only benchmark runs a producer and consumer in separate processes. It uses the real typed FIT Checks protocol
or a benchmark-only plaintext gRPC server using the existing Checks `.proto` service. Each explicit batch is sent
sequentially. A record is delivered when the consumer decodes and validates it. This measures logical payload
construction, codec, transport, and validation; it does not include ACR check execution or ADP's downstream pipeline.

Build on macOS with debug symbols and full optimization:

```sh
cargo build --profile optimized-release -p checks-ipc-bench
```

Run a single FIT metrics trial at 10,000 records/s, 64 records per batch, with five seconds of warmup and 30 seconds of
measurement:

```sh
target/optimized-release/checks-ipc-bench run --transport fit --workload metrics --rate 10000
```

Set `--workload logs` for logs or `--transport grpc` for the gRPC reference. Set `--batch 1`, `64`, or `256` to test
batch size. `--rate unlimited` removes pacing and reports saturation with loss; it is not a zero-loss capacity result.
`--ring` sets FIT ring capacity in bytes, defaulting to `1048576`. `--warmup`, `--duration`, `--repetitions`, and
`--output` control the run. The same executable and fixtures are used on both transports.

Find an observed no-loss rate with screening probes and five independent confirmation runs:

```sh
target/optimized-release/checks-ipc-bench search --transport fit --workload metrics --batch 64
```

Search starts at 10,000 records/s, doubles or halves to find a passing/failing bracket between 100 and 10 million
records/s, bisects to within 5%, then confirms with five 30-second runs. `--min-rate` and `--max-rate` change the search
bounds. A passing run has no rejected, missed, duplicated, out-of-order, invalid, or unreconciled records; at least 99%
arrive during the measurement interval; drain takes at most 100 ms; and sampled backlog does not persistently grow.
This finite-run result is an observed capacity under the stated workload, not a universal transport limit.

The metrics fixture is one scalar gauge named `ipc.benchmark.gauge`, with five fixed tags, a fixed hostname and
timestamp, and its sequence ID in the numeric value. Each log is an Info record of exactly 256 bytes, with a 16-byte
hexadecimal sequence prefix. The sequence number changes without changing record size. The benchmark checks every
decoded record and reconciles producer/consumer counts and sequence checksums. `scheduled` is the number of records
the producer was asked to offer; `schedule_missed` means it fell more than 10 ms behind and skipped those records.
FIT `rejected` means the shared ring did not accept the record. These counts remain separate in the raw results.

For a single sequence-1 record, the metric encodes to a 153-byte FIT payload (168-byte aligned ring record) or a
128-byte protobuf request. The log encodes to a 264-byte FIT payload (272-byte ring record) or a 267-byte protobuf
request. The protobuf sizes include the enclosing `SendCheckPayloadRequest`; the FIT payload sizes exclude the ring
header and alignment. Multi-record request overhead is different, so these numbers are fixture checks, not a byte-for-byte
transport cost comparison.

Each invocation creates a unique ignored directory under `bin/benchmarks/checks-ipc/results/`. `trial-*.json` contains
all counts, a 100 ms time series, throughput, CPU time, backlog slope, drain time, and pass/failure reasons.
`summary.csv` and `summary.md` give compact views; `provenance.json` records the checkout and build context, and a copy
of the executable is saved alongside the results. Setup, warmup, and draining are excluded from the in-window
rate. The `cpu_ms` printed at the console is the sum of producer and consumer process CPU during measurement. Each
worker reports current RSS and peak RSS; the trial includes RSS at completed setup, after warmup, and as 100 ms samples
during measurement. The summary gives the sum of both workers' average and peak RSS. Shared pages can appear in both
processes' RSS, so that sum is a process-accounting comparison rather than unique system memory.

`--consumer-delay-us` is a test-only option for deliberately slowing the decoder. Leave it at zero for performance
results.

## Profile one side

Install [Samply](https://github.com/mstange/samply) separately. Profiling is optional and is never included in the
unprofiled search result:

```sh
target/optimized-release/checks-ipc-bench profile --transport fit --workload metrics --rate 50000 --side producer
target/optimized-release/checks-ipc-bench profile --transport fit --workload metrics --rate 50000 --side consumer
samply load bin/benchmarks/checks-ipc/results/<run>/profile-000.json \
  --symbol-dir bin/benchmarks/checks-ipc/results/<run>
```

Each command profiles only the named child while the other child runs normally. The profile is written locally with
`--save-only`; Samply does not upload it. The command runs `dsymutil` and saves a matching `.dSYM` with the binary for
symbolication. Profiles show sampled CPU stacks and waiting, not precise allocation counts.
The profile's setup and warmup interval precedes the 30-second measurement interval; use `start_ns` and `end_ns` from
the corresponding trial JSON to identify the measured section. The profile run also writes its own counts, but sampling
can change throughput, so compare only unprofiled runs for headline rates.

## Profile memory on macOS

Memory profiling uses the macOS `vmmap`, `heap`, and `malloc_history` tools. It launches only the selected worker with
`MallocStackLogging=1` and captures a snapshot halfway through the measurement interval:

```sh
target/optimized-release/checks-ipc-bench profile --kind memory --transport fit --workload metrics --rate 10000 --side producer
target/optimized-release/checks-ipc-bench profile --kind memory --transport grpc --workload metrics --rate 10000 --side consumer
```

Each run saves `memory-000-vmmap.txt` for the process map and physical footprint, `memory-000-heap.txt` for live heap
sizes, `memory-000-allocations.txt` for the stack-logged allocation tree, and `memory-000-allocation-counts.txt` for
allocation frequency. Run both sides and both transports
separately. This instrumentation changes allocation costs, so compare steady-state RSS and CPU with ordinary `run`
results, and use these files only to attribute memory to benchmark fixtures, codecs, FIT mapping, or gRPC buffers.
