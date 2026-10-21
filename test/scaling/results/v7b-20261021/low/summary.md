cpus: 0.05 0.3 0.6 1.2; labels: plan-a plan-b candidate-a candidate-b; rounds: 3; native_image: saluki-images/agent-data-plane:legacy-b940bf1f8b (sha256:03109eef3d3ebe2b58796edd93010b79159a7f2547ab5b3733e37d83e1af16be); plan_image: saluki-images/agent-data-plane:plan-n1 (sha256:d0b13f91c1aa4b85e38196ca854b901d02824e2eb6b421a2ca7e59c77f85d373); plan_config: /Users/luke.steensen/code/saluki/test/scaling/adp/lanes-off.yaml; candidate_image: saluki-images/agent-data-plane:candidate-v7b (sha256:7e8a8dd6c9462165055ebfd58252fcbfbbe3429922d1676bf8c4f630971b601e), config /Users/luke.steensen/code/saluki/test/scaling/adp/lanes-off.yaml; contexts: 3000 (context limit 31000), cache 500 Mb, transport unix_datagram; steady_rate_per_cpu: 60 MiB; burst_factor: 4; steady_rates (KiB/s): 0.05:2947 0.3:23295 0.6:66020 1.2:209900; warmup 20 s, steady 30 s, burst lead 3 s, burst 8 s, recovery 12 s; adp cpuset 0-1, 2 workers, 2 decoders; lading 2-3

### 0.05 CPUs

| Measure | plan (6) | candidate (6) | candidate / plan | A/A plan | A/A candidate | spread plan | spread candidate |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 3526 | 3525 | 1.000 | 0.999 | 1.001 | 0.001 | 0.005 |
| steady: CPU ns/metric | 6432 | 6330 | 0.984 | 0.994 | 1.017 | 0.030 | 0.018 |
| steady: CPU (cores) | 0.023 | 0.022 | 0.983 | 0.992 | 1.020 | 0.030 | 0.022 |
| steady: throttled periods | 0.515 | 0.520 | 1.010 | 0.885 | 0.961 | 0.109 | 0.063 |
| steady: throttled ms/s | 540 | 570 | 1.056 | 1.033 | 0.946 | 0.189 | 0.058 |
| steady: shortfall | 0.006 | 0.004 | 0.723 | 0.993 | 0.366 | 0.795 | 1.438 |
| steady: anon (MiB) | 24.1 | 24.0 | 0.995 | 1.010 | 1.022 | 0.009 | 0.067 |
| burst: decoded/s | 9384 | 9642 | 1.028 | 1.104 | 0.843 | 0.078 | 0.144 |
| burst+recovery: CPU ns/metric | 4866 | 5150 | 1.058 | 0.881 | 1.104 | 0.107 | 0.083 |
| burst: throttled periods | 0.982 | 0.977 | 0.995 | 0.977 | 1.001 | 0.020 | 0.018 |
| burst: throttled ms/s | 1668 | 1636 | 0.981 | 0.952 | 1.012 | 0.041 | 0.033 |
| burst: shortfall | 0.257 | 0.243 | 0.947 | 0.631 | 1.408 | 0.422 | 0.351 |
| burst: longest stall (ms) | 299 | 201 | 0.669 | 1.003 | 0.995 | 0.752 | 0.749 |
| burst: admission after / before the flush | 0.800 | 0.794 | 0.993 | 1.102 | 0.935 | 0.323 | 0.105 |
| burst+recovery: anon peak (MiB) | 34.8 | 35.6 | 1.024 | 1.040 | 1.008 | 0.047 | 0.057 |
| burst end: pool capacity | 129 | 128 | 0.996 | 1.000 | 1.008 | 0.006 | 2.792 |
| burst: pool created | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.750 | 358.500 |
| run: memory.peak (MiB) | 36.9 | 38.5 | 1.044 | 1.035 | 0.910 | 0.032 | 0.081 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |

### 0.3 CPUs

| Measure | plan (6) | candidate (6) | candidate / plan | A/A plan | A/A candidate | spread plan | spread candidate |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 27721 | 27722 | 1.000 | 1.001 | 1.000 | 0.001 | 0.001 |
| steady: CPU ns/metric | 3318 | 3700 | 1.115 | 1.014 | 1.014 | 0.119 | 0.063 |
| steady: CPU (cores) | 0.092 | 0.103 | 1.114 | 1.014 | 1.016 | 0.120 | 0.064 |
| steady: throttled periods | 0.311 | 0.346 | 1.114 | 0.933 | 1.041 | 0.279 | 0.053 |
| steady: throttled ms/s | 318 | 336 | 1.060 | 0.894 | 0.918 | 0.511 | 0.131 |
| steady: shortfall | 0.004 | 0.000 | 0.000 | 0.000 | - | 1.445 | - |
| steady: anon (MiB) | 31.7 | 31.4 | 0.991 | 0.989 | 1.082 | 0.009 | 0.081 |
| burst: decoded/s | 88542 | 88814 | 1.003 | 1.000 | 0.995 | 0.026 | 0.016 |
| burst+recovery: CPU ns/metric | 3260 | 3329 | 1.021 | 0.988 | 1.036 | 0.018 | 0.035 |
| burst: throttled periods | 0.971 | 0.977 | 1.006 | 0.988 | 1.000 | 0.030 | 0.000 |
| burst: throttled ms/s | 1505 | 1476 | 0.981 | 1.006 | 1.002 | 0.049 | 0.019 |
| burst: shortfall | 0.126 | 0.131 | 1.041 | 0.865 | 0.705 | 0.136 | 0.355 |
| burst: longest stall (ms) | 100 | 102 | 1.025 | 1.000 | 1.030 | 0.000 | 0.734 |
| burst: admission after / before the flush | 0.896 | 0.926 | 1.033 | 0.746 | 0.981 | 0.236 | 0.089 |
| burst+recovery: anon peak (MiB) | 42.6 | 43.1 | 1.012 | 0.977 | 0.991 | 0.058 | 0.086 |
| burst end: pool capacity | 546 | 897 | 1.643 | 3.584 | 0.917 | 1.373 | 0.241 |
| burst: pool created | 488 | 642 | 1.317 | 1.903 | 0.922 | 0.397 | 0.457 |
| run: memory.peak (MiB) | 45.5 | 45.1 | 0.990 | 1.008 | 0.985 | 0.036 | 0.093 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |

### 0.6 CPUs

| Measure | plan (6) | candidate (6) | candidate / plan | A/A plan | A/A candidate | spread plan | spread candidate |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 78550 | 78534 | 1.000 | 1.000 | 1.001 | 0.000 | 0.001 |
| steady: CPU ns/metric | 1881 | 2151 | 1.144 | 0.990 | 1.092 | 0.033 | 0.069 |
| steady: CPU (cores) | 0.148 | 0.169 | 1.142 | 0.989 | 1.094 | 0.034 | 0.070 |
| steady: throttled periods | 0.180 | 0.245 | 1.356 | 0.666 | 1.628 | 0.583 | 0.396 |
| steady: throttled ms/s | 123 | 176 | 1.431 | 0.822 | 1.382 | 0.155 | 0.312 |
| steady: shortfall | 0.000 | 0.000 | - | - | - | - | - |
| steady: anon (MiB) | 33.8 | 35.2 | 1.040 | 1.044 | 1.024 | 0.036 | 0.058 |
| burst: decoded/s | 307797 | 295634 | 0.960 | 1.003 | 0.909 | 0.005 | 0.079 |
| burst+recovery: CPU ns/metric | 1654 | 1826 | 1.104 | 0.997 | 1.129 | 0.002 | 0.111 |
| burst: throttled periods | 0.732 | 0.834 | 1.139 | 0.922 | 1.197 | 0.108 | 0.140 |
| burst: throttled ms/s | 825 | 980 | 1.187 | 0.953 | 1.244 | 0.037 | 0.177 |
| burst: shortfall | 0.000 | 0.000 | - | - | - | - | - |
| burst: longest stall (ms) | 300 | 200 | 0.667 | 0.671 | 0.980 | 0.247 | 0.390 |
| burst: admission after / before the flush | 1.014 | 1.027 | 1.013 | 1.032 | 1.034 | 0.106 | 0.062 |
| burst+recovery: anon peak (MiB) | 51.0 | 44.9 | 0.881 | 1.068 | 1.152 | 0.056 | 0.115 |
| burst end: pool capacity | 1204 | 1075 | 0.893 | 2.552 | 1.318 | 0.909 | 0.537 |
| burst: pool created | 1276 | 758 | 0.595 | 1.384 | 0.896 | 0.530 | 0.336 |
| run: memory.peak (MiB) | 52.9 | 53.7 | 1.014 | 1.061 | 1.057 | 0.055 | 0.043 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |

### 1.2 CPUs

| Measure | plan (6) | candidate (6) | candidate / plan | A/A plan | A/A candidate | spread plan | spread candidate |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 249774 | 249824 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |
| steady: CPU ns/metric | 1515 | 1493 | 0.986 | 0.985 | 1.026 | 0.012 | 0.026 |
| steady: CPU (cores) | 0.378 | 0.373 | 0.986 | 0.984 | 1.013 | 0.012 | 0.017 |
| steady: throttled periods | 0.258 | 0.261 | 1.010 | 0.753 | 1.006 | 0.240 | 0.325 |
| steady: throttled ms/s | 126 | 127 | 1.005 | 0.849 | 0.990 | 0.140 | 0.047 |
| steady: shortfall | 0.000 | 0.002 | - | - | 0.000 | - | 2.020 |
| steady: anon (MiB) | 37.1 | 35.6 | 0.959 | 1.065 | 1.019 | 0.055 | 0.024 |
| burst: decoded/s | 832173 | 817777 | 0.983 | 1.001 | 0.992 | 0.006 | 0.025 |
| burst+recovery: CPU ns/metric | 1427 | 1440 | 1.009 | 0.999 | 1.013 | 0.004 | 0.023 |
| burst: throttled periods | 0.964 | 0.965 | 1.000 | 1.000 | 0.988 | 0.000 | 0.009 |
| burst: throttled ms/s | 592 | 581 | 0.981 | 1.017 | 0.982 | 0.024 | 0.023 |
| burst: shortfall | 0.138 | 0.151 | 1.100 | 0.957 | 1.114 | 0.042 | 0.126 |
| burst: longest stall (ms) | 100 | 100 | 1.000 | 1.000 | 0.990 | 0.000 | 0.008 |
| burst: admission after / before the flush | 0.991 | 0.995 | 1.004 | 1.013 | 1.026 | 0.012 | 0.020 |
| burst+recovery: anon peak (MiB) | 61.4 | 59.7 | 0.972 | 0.849 | 0.973 | 0.222 | 0.067 |
| burst end: pool capacity | 1535 | 1458 | 0.950 | 1.419 | 0.560 | 0.823 | 0.810 |
| burst: pool created | 1637 | 1536 | 0.939 | 0.624 | 1.626 | 0.586 | 0.940 |
| run: memory.peak (MiB) | 63.2 | 62.9 | 0.996 | 0.851 | 1.011 | 0.202 | 0.051 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |
