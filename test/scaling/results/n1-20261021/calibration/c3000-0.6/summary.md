cpus: 0.6; labels: native plan; rounds: 1; native_image: saluki-images/agent-data-plane:legacy-b940bf1f8b (sha256:03109eef3d3ebe2b58796edd93010b79159a7f2547ab5b3733e37d83e1af16be); plan_image: saluki-images/agent-data-plane:plan-n1 (sha256:d0b13f91c1aa4b85e38196ca854b901d02824e2eb6b421a2ca7e59c77f85d373); plan_config: /Users/luke.steensen/code/saluki/test/scaling/adp/lanes-off.yaml; contexts: 3000 (context limit 31000), cache 500 Mb, transport unix_datagram; steady_rate_per_cpu: 3 GiB; burst_factor: 1; steady_rates (KiB/s): none; warmup 10 s, steady 20 s, burst lead 3 s, burst 1 s, recovery 1 s; adp cpuset 0-1, 2 workers, 2 decoders; lading 2-3

### 0.6 CPUs

| Measure | native (1) | plan (1) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 377008 | 436958 | 1.159 | - | - | - | - |
| steady: CPU ns/metric | 1583 | 1370 | 0.865 | - | - | - | - |
| steady: CPU (cores) | 0.597 | 0.599 | 1.003 | - | - | - | - |
| steady: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| steady: throttled ms/s | 1169 | 1175 | 1.006 | - | - | - | - |
| steady: shortfall | 0.831 | 0.804 | 0.968 | - | - | - | - |
| steady: anon (MiB) | 71.4 | 39.3 | 0.551 | - | - | - | - |
| burst: decoded/s | 425051 | 557453 | 1.311 | - | - | - | - |
| burst+recovery: CPU ns/metric | 1530 | 1271 | 0.830 | - | - | - | - |
| burst: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| burst: throttled ms/s | 1162 | 1146 | 0.986 | - | - | - | - |
| burst: shortfall | 0.848 | 0.801 | 0.944 | - | - | - | - |
| burst: longest stall (ms) | 0 | 0 | - | - | - | - | - |
| burst: admission after / before the flush | 0.619 | 0.533 | 0.862 | - | - | - | - |
| burst+recovery: anon peak (MiB) | 78.6 | 40.7 | 0.517 | - | - | - | - |
| burst end: pool capacity | 4692 | 819 | 0.175 | - | - | - | - |
| burst: pool created | 1413 | 690 | 0.488 | - | - | - | - |
| run: memory.peak (MiB) | 88.0 | 63.9 | 0.726 | - | - | - | - |
| flushes in burst | 0 | 0 | - | - | - | - | - |
