cpus: 0.05 0.3 1.2; labels: native plan; rounds: 1; native_image: saluki-images/agent-data-plane:legacy-b940bf1f8b (sha256:03109eef3d3ebe2b58796edd93010b79159a7f2547ab5b3733e37d83e1af16be); plan_image: saluki-images/agent-data-plane:plan-n1 (sha256:d0b13f91c1aa4b85e38196ca854b901d02824e2eb6b421a2ca7e59c77f85d373); plan_config: /Users/luke.steensen/code/saluki/test/scaling/adp/lanes-off.yaml; contexts: 3000 (context limit 31000), cache 500 Mb, transport unix_datagram; steady_rate_per_cpu: 3 GiB; burst_factor: 1; warmup 10 s, steady 20 s, burst lead 3 s, burst 1 s, recovery 1 s; adp cpuset 0-1, 2 workers, 2 decoders; lading 2-3

### 0.05 CPUs

| Measure | native (1) | plan (1) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 11666 | 12066 | 1.034 | - | - | - | - |
| steady: CPU ns/metric | 4213 | 3903 | 0.926 | - | - | - | - |
| steady: CPU (cores) | 0.049 | 0.047 | 0.958 | - | - | - | - |
| steady: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| steady: throttled ms/s | 1897 | 1792 | 0.945 | - | - | - | - |
| steady: shortfall | 0.942 | 0.935 | 0.993 | - | - | - | - |
| steady: anon (MiB) | 123.3 | 32.0 | 0.260 | - | - | - | - |
| burst: decoded/s | 8725 | 17313 | 1.984 | - | - | - | - |
| burst+recovery: CPU ns/metric | 4929 | 2546 | 0.517 | - | - | - | - |
| burst: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| burst: throttled ms/s | 1848 | 1750 | 0.947 | - | - | - | - |
| burst: shortfall | 0.963 | 0.898 | 0.933 | - | - | - | - |
| burst: longest stall (ms) | 0 | 0 | - | - | - | - | - |
| burst: admission after / before the flush | - | - | - | - | - | - | - |
| burst+recovery: anon peak (MiB) | 82.0 | 31.7 | 0.387 | - | - | - | - |
| burst end: pool capacity | 3589 | 664 | 0.185 | - | - | - | - |
| burst: pool created | - | - | - | - | - | - | - |
| run: memory.peak (MiB) | 148.7 | 47.5 | 0.320 | - | - | - | - |
| flushes in burst | 0 | 0 | - | - | - | - | - |

### 0.3 CPUs

| Measure | native (1) | plan (1) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 92252 | 96196 | 1.043 | - | - | - | - |
| steady: CPU ns/metric | 3238 | 3108 | 0.960 | - | - | - | - |
| steady: CPU (cores) | 0.299 | 0.299 | 1.001 | - | - | - | - |
| steady: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| steady: throttled ms/s | 1577 | 1576 | 1.000 | - | - | - | - |
| steady: shortfall | 0.918 | 0.914 | 0.996 | - | - | - | - |
| steady: anon (MiB) | 53.2 | 34.3 | 0.645 | - | - | - | - |
| burst: decoded/s | 138474 | 148682 | 1.074 | - | - | - | - |
| burst+recovery: CPU ns/metric | 2669 | 2474 | 0.927 | - | - | - | - |
| burst: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| burst: throttled ms/s | 1686 | 1695 | 1.005 | - | - | - | - |
| burst: shortfall | 0.918 | 0.914 | 0.996 | - | - | - | - |
| burst: longest stall (ms) | 0 | 0 | - | - | - | - | - |
| burst: admission after / before the flush | - | - | - | - | - | - | - |
| burst+recovery: anon peak (MiB) | 67.4 | 36.1 | 0.535 | - | - | - | - |
| burst end: pool capacity | 2714 | 1042 | 0.384 | - | - | - | - |
| burst: pool created | - | - | - | - | - | - | - |
| run: memory.peak (MiB) | 74.1 | 45.0 | 0.607 | - | - | - | - |
| flushes in burst | 0 | 0 | - | - | - | - | - |

### 1.2 CPUs

| Measure | native (1) | plan (1) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 831040 | 888048 | 1.069 | - | - | - | - |
| steady: CPU ns/metric | 1442 | 1353 | 0.939 | - | - | - | - |
| steady: CPU (cores) | 1.198 | 1.202 | 1.003 | - | - | - | - |
| steady: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| steady: throttled ms/s | 619 | 612 | 0.988 | - | - | - | - |
| steady: shortfall | 0.814 | 0.802 | 0.985 | - | - | - | - |
| steady: anon (MiB) | 90.6 | 52.7 | 0.581 | - | - | - | - |
| burst: decoded/s | 1240215 | 1374032 | 1.108 | - | - | - | - |
| burst+recovery: CPU ns/metric | 1200 | 1118 | 0.932 | - | - | - | - |
| burst: throttled periods | 1.000 | 1.000 | 1.000 | - | - | - | - |
| burst: throttled ms/s | 679 | 699 | 1.029 | - | - | - | - |
| burst: shortfall | 0.817 | 0.803 | 0.983 | - | - | - | - |
| burst: longest stall (ms) | 0 | 0 | - | - | - | - | - |
| burst: admission after / before the flush | - | - | - | - | - | - | - |
| burst+recovery: anon peak (MiB) | 81.2 | 54.9 | 0.676 | - | - | - | - |
| burst end: pool capacity | 3600 | 2378 | 0.661 | - | - | - | - |
| burst: pool created | - | - | - | - | - | - | - |
| run: memory.peak (MiB) | 103.6 | 68.1 | 0.657 | - | - | - | - |
| flushes in burst | 0 | 0 | - | - | - | - | - |
