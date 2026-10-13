# Scaling curves in containers

This harness measures how ADP's DogStatsD throughput and CPU cost scale with cores, for two images side by side: the
legacy metrics path (the `main` image at the branch's merge base) and the plan-driven path (this tree). It is the
container half of the stage 9 measurements in `docs/design/programmable-metrics-stages-7-and-9.md` (sections 8.4 to
8.6); the in-process half is `bin/agent-data-plane/src/programmable_metrics/topology/measurement_tests/`, which can run
legacy and plan from one binary. The legacy path exists only under `cfg(test)` on this branch, so the container
comparison needs two images.

## What one run is

For each CPU count N, `compose.yaml` starts three containers on disjoint CPU sets:

| Container | Cores | What it does |
|---|---|---|
| `adp` | `--cpus N --cpuset-cpus 0-(N-1)` | The image under test with `SALUKI_WORKER_THREADS=N`, the decoders set to `max(2, N - 2)` (`DD_DOGSTATSD_WORKERS_COUNT`, because the native default reads the host's vCPUs), DogStatsD on one Unix socket only (datagram or stream, `adp/<transport>.env`), standalone mode, the profile's configuration (`adp/<profile>.yaml`, with `adp/gateways.env` or `adp/defaults.env`), and the `blackhole` container as its intake. The plan image also gets `SALUKI_CPU_TIME_SAMPLING=1`, so that its sampling is measured at equal N (the legacy image ignores it). |
| `lading` | `N` and `N+1` | lading's DogStatsD generator (rendered per run from the shape of `lading/generator.yaml`, which comes from the SMP case `dsd_uds_500mb_3k_contexts_throughput`) writing to the socket on a volume both containers mount. |
| `blackhole` | `N+2` and `N+3` | lading's HTTP blackhole, listening on port 9091 (the primary endpoint) and on 9092 to 9094 (the `gateways` profile's routing groups and Multi-Region Failover). |

`run-curves.sh` waits for the warm-up, then reads at both ends of the window:

- `adp__component_events_received_total{component_id="dsd_in",message_type="metrics"}` from ADP's `:5100/metrics`
  (the metrics the DogStatsD source decoded);
- `usage_usec` from the ADP container's `/sys/fs/cgroup/cpu.stat` (every thread of the process: the primary runtime,
  the global pool, and blocking threads);
- `bytes_written` from lading's `:9229/metrics`;

and after the window the container's peak memory (`memory.peak`, or `memory.current` on kernels before 5.19). At
saturation the generator waits on the socket, so the source's decoded rate is the rate of ADP's slowest stage.

`summarize.py` prints the series' configuration, then, per N and image, the medians over rounds: throughput,
throughput per CPU, CPU ns per metric, lading's bytes per decoded metric (which should not move with N: if it does, the
generator, not ADP, set the rate), peak memory, and the spread of throughput from p25 to p75 against the median; then
the plan's ratios to legacy (throughput, scaling `T(N) / T(N_min)` against legacy's, CPU per metric).

## Requirements

- Linux, with cgroup v2, Docker, and Docker Compose v2. The host needs at least `N_max + 4` homogeneous cores: 20 for
  N = 16, 36 for N = 32 (plan question Q2). The script refuses a smaller host unless `SCALING_ALLOW_SMALL_HOST=1`,
  which overlaps CPU sets and is not for gating.
- `curl` and `python3` on the host.
- The two images (below), and `ghcr.io/datadog/lading:$LADING_VERSION` (the `LADING_VERSION` that `Makefile` sets).

Docker Desktop and similar tools on macOS run a Linux VM whose vCPUs float over performance and efficiency cores: the
harness runs there, and can check F7 (CPU-time reads cost a system call on Linux) and M6, but not homogeneous scaling.

## Running it

```sh
# Both images: legacy from the merge base (in a temporary work tree under target/), plan from this tree.
make scaling-images

# First, per host, an A/A series: legacy against legacy sets the tolerance for this host.
SCALING_AA=1 make scaling-curves

# Then legacy against plan.
make scaling-curves

# Fewer CPU counts, shorter windows, a paced rate, more contexts, the mapper profile:
SCALING_WORKERS="1 2 4" SCALING_ROUNDS=3 SCALING_WINDOW_SECONDS=30 SCALING_RATE="200 MiB" SCALING_CONTEXTS=100000 \
  SCALING_PROFILE=mapper make scaling-curves
```

| Variable | Default | Meaning |
|---|---|---|
| `SCALING_WORKERS` | `1 2 4 8 16` | the CPU counts N |
| `SCALING_ROUNDS` | 5 | rounds per image and N; the images alternate which goes first |
| `SCALING_WARMUP_SECONDS`, `SCALING_WINDOW_SECONDS` | 20, 60 | the warm-up and the measured window of a run |
| `SCALING_PROFILE` | `defaults` | ADP's configuration and the metric names lading sends: `defaults`, `mapper` (a mapper profile and a metric filterlist), or `gateways` (the mapper profile, two endpoint routing groups, and Multi-Region Failover); see `adp/*.yaml` |
| `SCALING_ADP_CONFIG` | `adp/<profile>.yaml` | another ADP configuration file for the run, such as one setting `lanes.*` once those keys exist; the profile still sets lading's metric names and the environment file |
| `SCALING_TRANSPORT` | `unix_datagram` | `unix_datagram` or `unix_stream` (length-prefixed frames, a decoding task per connection) |
| `SCALING_GENERATORS` | 2 | lading generator entries, each a task of its own with its own block cache |
| `SCALING_CONNECTIONS` | 1 | connections per generator entry (`parallel_connections`); every connection sends `SCALING_RATE / (generators x connections)`, at most 4 GiB/s |
| `SCALING_RATE` | `4 GiB` | lading's total rate, in bytes per second (saturation by default); for a paced run, 70% of saturation is 0.7 times the saturated decoded rate times lading's bytes per metric, from an earlier saturated series |
| `SCALING_CONTEXTS` | 3000 | contexts lading generates |
| `SCALING_CONTEXT_LIMIT` | `SCALING_CONTEXTS` × 10 + 1000 | `aggregate_context_limit` of both images: above what lading's contexts become per metric kind and tag set, so that no run drops at aggregation |
| `SCALING_METRIC_WEIGHTS` | `208:66:72:9:1` | lading's weights of counts, gauges, distributions, sets, and histograms, each 0 to 255 |
| `SCALING_TIMESTAMP_PROBABILITY` | 0 | the share of counts and gauges that carry a timestamp (`\|T`), which takes them through the passthrough instead of aggregation |
| `SCALING_CACHE` | `500 Mb` | lading's block cache per generator entry, as lading reads it (`Mb` is megabits); a stream connection builds a cache of its own, so `unix_stream` divides it among the connections, 1 MiB each at least |
| `SCALING_DECODERS` | `max(2, N - 2)` | ADP's datagram decoders |
| `SCALING_LEGACY_IMAGE`, `SCALING_PLAN_IMAGE` | the tags `make scaling-images` creates | the images compared |
| `SCALING_LEGACY_BASE` | `b940bf1f8b` | the commit `make scaling-images` builds the legacy image from |
| `SCALING_AA` | 0 | 1 compares the legacy image against itself |
| `SCALING_PLAN_CPU_TIME_SAMPLING` | 1 | `SALUKI_CPU_TIME_SAMPLING` for the plan image |
| `SCALING_OUT` | `test/scaling/results/<time>` | where `runs.csv`, `summary.md`, the rendered lading configurations, and each run's ADP log go |
| `SCALING_DRY_RUN` | 0 | 1 renders the configurations and prints the docker commands without running them |

## The gate's configurations

Gate items 1 to 3 of plan section 8.5 (throughput ratio at least 0.93, scaling ratio at least 0.93, CPU per metric
ratio at most 1.07) are decided here, for each configuration of the gate and each N, one series per configuration:

| Configuration | Variables |
|---|---|
| defaults | `SCALING_PROFILE=defaults` |
| a mapper profile | `SCALING_PROFILE=mapper` |
| gateways | `SCALING_PROFILE=gateways` |
| 100,000 and 1M contexts | `SCALING_CONTEXTS=100000`, `SCALING_CONTEXTS=1000000` |
| histogram-heavy | `SCALING_METRIC_WEIGHTS=20:10:10:0:200` |
| passthrough-heavy | `SCALING_METRIC_WEIGHTS=200:100:0:0:0 SCALING_TIMESTAMP_PROBABILITY=0.9` |
| one decoder with `lanes.mode: auto` | `SCALING_DECODERS=1` (`auto` is the image's default) |
| 70% of saturation | `SCALING_RATE=<0.7 x saturated decoded rate x lading bytes per metric>` |
| about 500 UDS stream connections | `SCALING_TRANSPORT=unix_stream SCALING_GENERATORS=1 SCALING_CONNECTIONS=500` |

Gate item 4, one saturated stream connection, is `SCALING_TRANSPORT=unix_stream SCALING_GENERATORS=1
SCALING_CONNECTIONS=1`, and in process M1 (`one_saturated_stream_connection`).

What stays in process: the paced p99 of item 3 and the phase with a live filterlist change every 50 ms (containers have
no markers to time and no configuration stream to change), the tag filterlist of the mapper profile (lading's tag keys
are random), and, if M2 shows the datagram receiver or the resolver capping both curves, items 1 to 3 themselves (plan
section 8.5: in process legacy runs the same source code and gets the same fixes; the `main` image cannot).

## Reading the results

The A/A series first: its throughput spread, p25 to p75 over the median, per N, is the noise this host adds. The plan's
spread must fit inside it, and a ratio within the spread of 1 is not a difference. If lading's bytes per metric grows
with N, the generator is the limit: raise `SCALING_GENERATORS` (on the same two cores) or give lading more cores.

Report absolute throughput per CPU beside the ratios: two flat curves can pass items 1 and 2 while saying nothing about
lanes (plan section 8.5). If both flatten below the top N, the in-process M2 shows whether the datagram receiver or the
resolver caps them.

## Validating the files without a daemon

`docker compose --file test/scaling/compose.yaml config --quiet` checks the compose file and that the environment files
it names exist, and `SCALING_DRY_RUN=1 test/scaling/run-curves.sh` renders every lading configuration and prints the
commands of a series. Neither needs a running Docker daemon.
