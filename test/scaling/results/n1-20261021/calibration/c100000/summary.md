cpus: 0.05 0.3 1.2; labels: native plan; rounds: 1; native_image: saluki-images/agent-data-plane:legacy-b940bf1f8b (sha256:03109eef3d3ebe2b58796edd93010b79159a7f2547ab5b3733e37d83e1af16be); plan_image: saluki-images/agent-data-plane:plan-n1 (sha256:d0b13f91c1aa4b85e38196ca854b901d02824e2eb6b421a2ca7e59c77f85d373); plan_config: /Users/luke.steensen/code/saluki/test/scaling/adp/lanes-off.yaml; contexts: 100000 (context limit 1001000), cache 2000 Mb, transport unix_datagram; steady_rate_per_cpu: 3 GiB; burst_factor: 1; warmup 10 s, steady 20 s, burst lead 3 s, burst 1 s, recovery 1 s; adp cpuset 0-1, 2 workers, 2 decoders; lading 2-3

### 0.05 CPUs

| Measure | native (1) | plan (1) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 2927 | 2999 | 1.025 | - | - | - | - |
| steady: CPU ns/metric | 16949 | 16442 | 0.970 | - | - | - | - |
| steady: CPU (cores) | 0.050 | 0.049 | 0.994 | - | - | - | - |
| steady: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| steady: throttled ms/s | 1922 | 1907 | 0.992 | - | - | - | - |
| steady: shortfall | 0.988 | 0.981 | 0.993 | - | - | - | - |
| steady: anon (MiB) | 152.4 | 136.5 | 0.896 | - | - | - | - |
| burst: decoded/s | 1486 | 2679 | 1.803 | - | - | - | - |
| burst+recovery: CPU ns/metric | 46247 | 19125 | 0.414 | - | - | - | - |
| burst: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| burst: throttled ms/s | 1876 | 1903 | 1.014 | - | - | - | - |
| burst: shortfall | 0.992 | 0.984 | 0.992 | - | - | - | - |
| burst: longest stall (ms) | 0 | 0 | - | - | - | - | - |
| burst: admission after / before the flush | - | - | - | - | - | - | - |
| burst+recovery: anon peak (MiB) | 180.2 | 243.7 | 1.352 | - | - | - | - |
| burst end: pool capacity | 337 | 129 | 0.383 | - | - | - | - |
| burst: pool created | - | - | - | - | - | - | - |
| run: memory.peak (MiB) | 187.7 | 253.0 | 1.348 | - | - | - | - |
| flushes in burst | 0 | 0 | - | - | - | - | - |

### 0.3 CPUs

| Measure | native (1) | plan (1) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 46887 | 55013 | 1.173 | - | - | - | - |
| steady: CPU ns/metric | 6393 | 5419 | 0.848 | - | - | - | - |
| steady: CPU (cores) | 0.300 | 0.298 | 0.995 | - | - | - | - |
| steady: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| steady: throttled ms/s | 1603 | 1609 | 1.004 | - | - | - | - |
| steady: shortfall | 0.958 | 0.951 | 0.993 | - | - | - | - |
| steady: anon (MiB) | 336.9 | 307.1 | 0.911 | - | - | - | - |
| burst: decoded/s | 98313 | 147473 | 1.500 | - | - | - | - |
| burst+recovery: CPU ns/metric | 3306 | 2804 | 0.848 | - | - | - | - |
| burst: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| burst: throttled ms/s | 1534 | 1456 | 0.949 | - | - | - | - |
| burst: shortfall | 0.927 | 0.931 | 1.005 | - | - | - | - |
| burst: longest stall (ms) | 0 | 0 | - | - | - | - | - |
| burst: admission after / before the flush | - | - | - | - | - | - | - |
| burst+recovery: anon peak (MiB) | 345.7 | 311.2 | 0.900 | - | - | - | - |
| burst end: pool capacity | 4776 | 1073 | 0.225 | - | - | - | - |
| burst: pool created | - | - | - | - | - | - | - |
| run: memory.peak (MiB) | 360.0 | 346.6 | 0.963 | - | - | - | - |
| flushes in burst | 0 | 0 | - | - | - | - | - |

### 1.2 CPUs

| Measure | native (1) | plan (1) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 657771 | 694989 | 1.057 | - | - | - | - |
| steady: CPU ns/metric | 1817 | 1720 | 0.947 | - | - | - | - |
| steady: CPU (cores) | 1.195 | 1.195 | 1.000 | - | - | - | - |
| steady: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| steady: throttled ms/s | 681 | 614 | 0.902 | - | - | - | - |
| steady: shortfall | 0.852 | 0.843 | 0.990 | - | - | - | - |
| steady: anon (MiB) | 471.9 | 308.6 | 0.654 | - | - | - | - |
| burst: decoded/s | 1085478 | 1136659 | 1.047 | - | - | - | - |
| burst+recovery: CPU ns/metric | 1351 | 1299 | 0.961 | - | - | - | - |
| burst: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| burst: throttled ms/s | 633 | 611 | 0.964 | - | - | - | - |
| burst: shortfall | 0.841 | 0.835 | 0.993 | - | - | - | - |
| burst: longest stall (ms) | 0 | 0 | - | - | - | - | - |
| burst: admission after / before the flush | - | - | - | - | - | - | - |
| burst+recovery: anon peak (MiB) | 397.0 | 323.4 | 0.815 | - | - | - | - |
| burst end: pool capacity | 4777 | 1521 | 0.318 | - | - | - | - |
| burst: pool created | - | - | - | - | - | - | - |
| run: memory.peak (MiB) | 521.8 | 363.3 | 0.696 | - | - | - | - |
| flushes in burst | 0 | 0 | - | - | - | - | - |
