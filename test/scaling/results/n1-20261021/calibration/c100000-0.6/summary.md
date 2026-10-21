cpus: 0.6; labels: native plan; rounds: 1; native_image: saluki-images/agent-data-plane:legacy-b940bf1f8b (sha256:03109eef3d3ebe2b58796edd93010b79159a7f2547ab5b3733e37d83e1af16be); plan_image: saluki-images/agent-data-plane:plan-n1 (sha256:d0b13f91c1aa4b85e38196ca854b901d02824e2eb6b421a2ca7e59c77f85d373); plan_config: /Users/luke.steensen/code/saluki/test/scaling/adp/lanes-off.yaml; contexts: 100000 (context limit 1001000), cache 2000 Mb, transport unix_datagram; steady_rate_per_cpu: 3 GiB; burst_factor: 1; steady_rates (KiB/s): none; warmup 10 s, steady 20 s, burst lead 3 s, burst 1 s, recovery 1 s; adp cpuset 0-1, 2 workers, 2 decoders; lading 2-3

### 0.6 CPUs

| Measure | native (1) | plan (1) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 287576 | 310450 | 1.080 | - | - | - | - |
| steady: CPU ns/metric | 2084 | 1928 | 0.925 | - | - | - | - |
| steady: CPU (cores) | 0.599 | 0.599 | 0.999 | - | - | - | - |
| steady: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| steady: throttled ms/s | 1284 | 1222 | 0.952 | - | - | - | - |
| steady: shortfall | 0.869 | 0.860 | 0.989 | - | - | - | - |
| steady: anon (MiB) | 416.2 | 313.5 | 0.753 | - | - | - | - |
| burst: decoded/s | 473100 | 481704 | 1.018 | - | - | - | - |
| burst+recovery: CPU ns/metric | 1535 | 1444 | 0.941 | - | - | - | - |
| burst: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| burst: throttled ms/s | 1191 | 1117 | 0.938 | - | - | - | - |
| burst: shortfall | 0.843 | 0.843 | 1.001 | - | - | - | - |
| burst: longest stall (ms) | 0 | 0 | - | - | - | - | - |
| burst: admission after / before the flush | 0.414 | 0.245 | 0.591 | - | - | - | - |
| burst+recovery: anon peak (MiB) | 411.6 | 322.0 | 0.782 | - | - | - | - |
| burst end: pool capacity | 4401 | 699 | 0.159 | - | - | - | - |
| burst: pool created | 0 | 570 | - | - | - | - | - |
| run: memory.peak (MiB) | 603.1 | 367.3 | 0.609 | - | - | - | - |
| flushes in burst | 0 | 0 | - | - | - | - | - |
