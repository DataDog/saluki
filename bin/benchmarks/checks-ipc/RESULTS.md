# FIT and gRPC Checks comparison on macOS

These measurements compare the FIT Checks transport with the previous gRPC Checks transport on one Apple Silicon macOS
host on 2026-10-06. They measure two scopes: isolated producer/consumer processes running the same logical payload fixture,
and the full ACR plus ADP applications running a synthetic Python check. Values below are medians of five alternating-order
trials per transport unless stated otherwise. Raw local outputs are under `results/` for the isolated benchmark and
`results/global-application-2026-10-06/trials/` for the application run. The latter directory also contains the
application fixture and measurement script. These paths are not tracked by Git.

## Isolated ACR-to-ADP transport scope

The benchmark in this directory uses the actual FIT protocol implementation or a benchmark-only plaintext gRPC service
built from the existing Checks `.proto` service. Both workers run in separate processes, validate decoded records, and
use the same fixture. The measurement excludes check execution and ADP's downstream pipeline. Each run offered 10,000
records/s in explicit batches of 64, warmed up for 5 seconds, then measured for 30 seconds. The FIT ring was 1 MiB. CPU
is the sum of producer and consumer process CPU time during measurement; RSS is the sum of their time-averaged resident
sets. Shared FIT pages can appear in both resident sets.

| Workload | Transport | Combined CPU / 30 s | CPU / decoded record | Combined average RSS | Delivery and pacing |
| --- | --- | ---: | ---: | ---: | --- |
| Metrics | FIT | 1.150 s | 3.84 µs | 6.65 MiB | All accepted records decoded; zero ring rejects; 960 scheduled records missed across five trials |
| Metrics | gRPC | 2.804 s | 9.36 µs | 6.75 MiB | All accepted records decoded; 8,512 scheduled records missed across five trials |
| Logs | FIT | 1.066 s | 3.55 µs | 6.64 MiB | All accepted records decoded; zero ring rejects; 64 scheduled records missed across five trials |
| Logs | gRPC | 3.273 s | 10.94 µs | 6.86 MiB | All accepted records decoded; 2,560 scheduled records missed across five trials |

| Workload | FIT CPU change versus gRPC | FIT average RSS change versus gRPC |
| --- | ---: | ---: |
| Metrics | 59.0% lower | 1.5% lower |
| Logs | 67.4% lower | 3.2% lower |

At this offered rate, gRPC used 2.44× the combined CPU for metrics and 3.07× for logs. These are transport-scope
comparisons, not whole-application speedups. The benchmark's strict pass criterion includes **zero** schedule misses;
all five gRPC trials, all five FIT metrics trials, and one FIT logs trial failed that criterion. `schedule_missed` means
the producer fell more than 10 ms behind the pacing schedule and skipped records before submitting them. It is distinct
from FIT ring rejection or loss after acceptance. Accordingly, this run does **not** establish a no-loss capacity.

A short 10-second sensitivity check at the same 10,000 records/s rate showed that batch size matters:

| Workload | Batch | FIT CPU / 10 s | gRPC CPU / 10 s | FIT avg RSS | gRPC avg RSS | Pacing |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Metrics | 1 | 0.926 s | 5.137 s | 6.5 MiB | 6.6 MiB | gRPC missed 385 of 100,000 scheduled records |
| Metrics | 256 | 0.321 s | 0.550 s | 6.8 MiB | 7.5 MiB | Both delivered 100,000 records |
| Logs | 1 | 1.153 s | 6.661 s | 6.5 MiB | 6.5 MiB | gRPC missed 2,355 of 100,000 scheduled records |
| Logs | 256 | 0.123 s | 0.416 s | 7.0 MiB | 7.8 MiB | Both delivered 100,000 records |

### Observed saturation throughput

Three additional unprofiled 10-second trials per combination removed pacing and sent batches as fast as the producer
could. The table reports the median **decoded** rate. All accepted records were decoded. With batch size 1, neither
transport rejected records in any trial. At larger batch sizes, the 1 MiB FIT ring rejected attempted records in every
metrics trial and some logs trials; gRPC applied backpressure through sequential RPCs and rejected none. These values
measure maximum observed delivery under this fixture's self-paced load, **not** a guaranteed externally offered,
loss-free operating limit. `results/throughput-2026-10-06.json` indexes the raw saturation and controlled-rate trials.
At batch size 1, the gRPC benchmark makes one sequential unary RPC per record; this is an important part of the result.

| Workload | Batch | FIT decoded/s | gRPC decoded/s | FIT/gRPC | FIT throughput delta | FIT ring rejection |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Metrics | 1 | 911,183 | 15,827 | 57.57× | 5,657% higher | None in three FIT trials |
| Metrics | 64 | 3.32 million | 0.54 million | 6.10× | 510% higher | All three FIT trials rejected records |
| Metrics | 256 | 3.39 million | 1.04 million | 3.26× | 226% higher | All three FIT trials rejected records |
| Logs | 1 | 942,232 | 15,519 | 60.71× | 5,971% higher | None in three FIT trials |
| Logs | 64 | 4.17 million | 0.63 million | 6.64× | 564% higher | Two of three FIT trials rejected records |
| Logs | 256 | 4.51 million | 1.32 million | 3.41× | 241% higher | One of three FIT trials rejected records |

In a separate controlled-rate 10-second sweep with batches of 64, a FIT metrics trial delivered all 500,000 scheduled
records/s without rejection; FIT logs did so at 1,000,000 records/s. Those are single successful observations, not
confirmed limits. FIT metrics trials at 1,000,000 records/s and FIT logs trials at 2,000,000 records/s missed scheduled
records and/or rejected writes. A gRPC metrics trial at 100,000 records/s passed, while tested higher rates had some
pacing misses. The strict 10 ms pacing cutoff produced misses even at lower rates and sometimes passed at higher ones,
so it did not yield a stable, monotonic zero-loss capacity boundary on this macOS host. A no-loss maximum remains
unmeasured.

The benchmark's `profile --kind memory` mode launches one worker with `MallocStackLogging=1` and captures `vmmap`,
`heap`, and `malloc_history` at mid-measurement. One instrumented run per worker and workload gave these live malloc
heap sizes and physical footprints (not a statistical estimate):

| Workload | Transport | Producer live heap | Consumer live heap | Producer footprint | Consumer footprint |
| --- | --- | ---: | ---: | ---: | ---: |
| Metrics | FIT | 30 KiB | 27 KiB | 4.05 MiB | 3.78 MiB |
| Metrics | gRPC | 179 KiB | 208 KiB | 3.50 MiB | 3.72 MiB |
| Logs | FIT | 30 KiB | 27 KiB | 4.08 MiB | 3.77 MiB |
| Logs | gRPC | 179 KiB | 182 KiB | 3.78 MiB | 3.77 MiB |

The FIT workers each mapped about 1.05 MiB of shared memory; a 1 MiB ring dominates that mapping. The gRPC workers
had about 32 KiB of shared mappings. FIT's live malloc heap was smaller, while the mapped ring offsets its physical
footprint. Instrumentation changes allocation behavior, so the unprofiled RSS table above is the better steady-state
comparison. The raw allocation trees under `results/run-*/memory-000-*.txt` identify the FIT mapping and gRPC runtime
allocations.

macOS `sample` CPU stack captures for both workers were also saved under `results/cpu-*/`. They point to fixture
construction and string encoding/decoding on the FIT path, and Tokio/Tonic/HTTP/2 framing, protobuf work, and
round-trip waiting on the gRPC path. Saturating, unpaced profile runs had different offered and rejected counts, so
their per-record CPU numbers are not used in the comparison table.

## Whole-application ACR plus ADP scope

For the gRPC baseline, ACR was built from `5bd3492` and ADP from `9cdbc3c052`, immediately before their FIT Checks
changes. The FIT applications were built from ACR `4df0d48` and ADP `eb02cee950`. All four binaries used their release
or optimized-release profiles. Both configurations ran standalone with the same local intake and a synthetic Python
check. Each check invocation submitted 500 metrics and 500 256-byte logs. ACR sent Checks to ADP over FIT or gRPC;
ADP sent output to a local HTTP intake. Each trial warmed up for 10 seconds and measured both processes for 30 seconds.
The enabled-check and idle configurations were otherwise identical across transports. This is a process-level fixture,
not a production Agent workload.

| Check state | Transport | ACR CPU / 30 s | ADP CPU / 30 s | Combined CPU / 30 s | ACR avg RSS | ADP avg RSS | Combined avg RSS |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Enabled | FIT | 1.788 s | 1.927 s | 3.714 s | 41.50 MiB | 38.21 MiB | 78.97 MiB |
| Enabled | gRPC | 2.483 s | 3.228 s | 5.711 s | 40.45 MiB | 36.15 MiB | 76.75 MiB |
| Idle | FIT | 0.036 s | 0.143 s | 0.179 s | 31.84 MiB | 29.97 MiB | 61.91 MiB |
| Idle | gRPC | 0.043 s | 0.145 s | 0.188 s | 31.27 MiB | 29.66 MiB | 60.86 MiB |

| Check state | FIT combined CPU change versus gRPC | FIT combined average RSS change versus gRPC |
| --- | ---: | ---: |
| Enabled | 35.0% lower | 2.9% higher |
| Idle | 4.6% lower | 1.7% higher |

With the check enabled, FIT used 35.0% less combined process CPU (1.54× gRPC/FIT): 28.0% less in ACR and 40.3% less in
ADP. Subtracting each transport's idle median gives an incremental 3.535 CPU seconds for FIT and 5.523 for gRPC over
the 30-second window, a 36% reduction.
The local intake received a median of 66 requests in FIT trials and 65 in gRPC trials across warmup plus measurement,
including roughly 15–16 check-run requests per trial as well as logs and series. The application fixture therefore
confirms data reached the downstream intake through both transports, but the intake request count is not a per-record
delivery proof; the isolated benchmark provides that check.

FIT used 2.22 MiB (2.9%) more combined average RSS with the check enabled: 2.6% more in ACR and 5.7% more in ADP.
At idle, FIT used 1.05 MiB (1.7%) more. The enabled-minus-idle resident-set increase was 17.06 MiB for FIT and
15.89 MiB for gRPC. The difference includes the shared ring and all
other application behavior; summed per-process RSS double-counts shared pages and must not be interpreted as unique
physical memory. The applications and check may retain allocation arenas after warmup, so these 30-second values do
not establish long-running peak memory or leak behavior.

The comparison is on one macOS host with one fixture, one payload size per workload, and five short trials. The
transport benchmark uses sequential gRPC calls and explicit batches; changing concurrent RPCs, ring size, check mix,
batch size, or downstream pressure can change the result. The whole-application baselines are separate revisions, so
their difference can include effects beyond the wire transport. No formal zero-loss capacity search or production
workload profile was completed.

## Batch-512 and batch-1024 follow-up on 2026-10-09

An isolated worktree at benchmark revision `dabefef622` ran three unprofiled 10-second saturation trials per workload
and transport at each batch size of 256, 512, and 1024. The batch-256 control trials ran in the same session as batch
512 to compare those sizes under similar machine conditions; batch 1024 followed shortly after. Each trial warmed up
for two seconds and used a 1 MiB FIT ring. The table reports medians of decoded records per second.

| Workload | Batch | FIT decoded/s | gRPC decoded/s | FIT/gRPC |
| --- | ---: | ---: | ---: | ---: |
| Metrics | 256 | 4,164,537 | 1,035,878 | 4.02× |
| Metrics | 512 | 4,275,777 | 1,166,746 | 3.66× |
| Metrics | 1024 | 4,302,014 | 1,244,058 | 3.46× |
| Logs | 256 | 4,779,115 | 1,298,227 | 3.68× |
| Logs | 512 | 5,073,784 | 1,638,605 | 3.10× |
| Logs | 1024 | 5,110,761 | 1,579,827 | 3.24× |

Going from 256 to 512 raised FIT throughput by 2.7% for metrics and 6.2% for logs. gRPC throughput rose by 12.6% and
26.2%, respectively. From 512 to 1024, FIT gained only 0.6% for metrics and 0.7% for logs. gRPC metrics gained 6.6%,
while gRPC logs fell 3.6%. The metrics ratio narrowed to 3.46×; the logs ratio rose to 3.24×. Neither approaches 1×.
These rates are self-paced saturation measurements, not no-loss operating limits: FIT rejected attempted records in every
trial, while gRPC's sequential RPCs applied backpressure. Every accepted record was decoded on both transports.
Raw trial paths and counts remain local in `results/batch-256-512-1024-2026-10-09.json` (ignored by Git).
