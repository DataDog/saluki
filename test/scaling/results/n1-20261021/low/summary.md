cpus: 0.05 0.3 0.6 1.2; labels: native-a native-b plan-a plan-b; rounds: 3; native_image: saluki-images/agent-data-plane:legacy-b940bf1f8b (sha256:03109eef3d3ebe2b58796edd93010b79159a7f2547ab5b3733e37d83e1af16be); plan_image: saluki-images/agent-data-plane:plan-n1 (sha256:d0b13f91c1aa4b85e38196ca854b901d02824e2eb6b421a2ca7e59c77f85d373); plan_config: /Users/luke.steensen/code/saluki/test/scaling/adp/lanes-off.yaml; contexts: 3000 (context limit 31000), cache 500 Mb, transport unix_datagram; steady_rate_per_cpu: 60 MiB; burst_factor: 4; steady_rates (KiB/s): 0.05:2947 0.3:23295 0.6:66020 1.2:209900; warmup 20 s, steady 30 s, burst lead 3 s, burst 8 s, recovery 12 s; adp cpuset 0-1, 2 workers, 2 decoders; lading 2-3

### 0.05 CPUs

| Measure | native (6) | plan (6) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 3522 | 3534 | 1.003 | 1.001 | 1.035 | 0.021 | 0.028 |
| steady: CPU ns/metric | 6363 | 5955 | 0.936 | 0.998 | 0.983 | 0.022 | 0.040 |
| steady: CPU (cores) | 0.022 | 0.021 | 0.948 | 0.994 | 1.010 | 0.008 | 0.027 |
| steady: throttled periods | 0.560 | 0.520 | 0.929 | 1.080 | 1.097 | 0.094 | 0.102 |
| steady: throttled ms/s | 566 | 478 | 0.846 | 0.901 | 0.938 | 0.078 | 0.073 |
| steady: shortfall | 0.006 | 0.006 | 1.041 | 0.923 | 0.854 | 0.820 | 0.141 |
| steady: anon (MiB) | 28.8 | 24.0 | 0.831 | 0.980 | 1.035 | 0.030 | 0.038 |
| burst: decoded/s | 9693 | 10447 | 1.078 | 0.932 | 0.856 | 0.080 | 0.149 |
| burst+recovery: CPU ns/metric | 5220 | 4767 | 0.913 | 0.977 | 1.083 | 0.066 | 0.075 |
| burst: throttled periods | 0.988 | 0.988 | 1.000 | 1.000 | 1.025 | 0.009 | 0.019 |
| burst: throttled ms/s | 1745 | 1672 | 0.958 | 1.031 | 1.002 | 0.038 | 0.024 |
| burst: shortfall | 0.213 | 0.217 | 1.022 | 1.157 | 1.836 | 0.413 | 0.560 |
| burst: longest stall (ms) | 500 | 250 | 0.500 | 1.000 | 1.500 | 0.150 | 0.400 |
| burst: admission after / before the flush | 0.654 | 0.832 | 1.272 | 2.365 | 0.769 | 0.562 | 0.213 |
| burst+recovery: anon peak (MiB) | 75.3 | 35.2 | 0.468 | 0.962 | 0.789 | 0.167 | 0.203 |
| burst end: pool capacity | 3721 | 129 | 0.035 | 1.660 | 0.490 | 0.487 | 0.779 |
| burst: pool created | 4396 | 1 | 0.000 | 0.993 | 0.007 | 0.281 | 100.500 |
| run: memory.peak (MiB) | 79.4 | 43.1 | 0.543 | 1.125 | 0.901 | 0.124 | 0.441 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |

### 0.3 CPUs

| Measure | native (6) | plan (6) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 27734 | 27733 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |
| steady: CPU ns/metric | 3716 | 3628 | 0.976 | 1.036 | 0.978 | 0.027 | 0.037 |
| steady: CPU (cores) | 0.103 | 0.102 | 0.992 | 1.036 | 1.012 | 0.027 | 0.035 |
| steady: throttled periods | 0.357 | 0.347 | 0.970 | 1.001 | 1.019 | 0.032 | 0.051 |
| steady: throttled ms/s | 311 | 314 | 1.011 | 1.140 | 0.978 | 0.274 | 0.054 |
| steady: shortfall | 0.005 | 0.000 | 0.000 | 1.013 | - | 0.037 | - |
| steady: anon (MiB) | 42.5 | 31.6 | 0.745 | 1.003 | 1.036 | 0.083 | 0.028 |
| burst: decoded/s | 87500 | 92121 | 1.053 | 0.939 | 0.995 | 0.052 | 0.014 |
| burst+recovery: CPU ns/metric | 3526 | 3294 | 0.934 | 1.036 | 1.027 | 0.028 | 0.022 |
| burst: throttled periods | 0.976 | 0.976 | 1.000 | 1.000 | 1.000 | 0.000 | 0.009 |
| burst: throttled ms/s | 1530 | 1475 | 0.964 | 1.004 | 0.966 | 0.024 | 0.032 |
| burst: shortfall | 0.201 | 0.148 | 0.736 | 1.400 | 1.354 | 0.266 | 0.234 |
| burst: longest stall (ms) | 100 | 0 | 0.000 | - | - | 0.750 | - |
| burst: admission after / before the flush | 1.039 | 1.020 | 0.981 | 0.771 | 0.779 | 0.207 | 0.283 |
| burst+recovery: anon peak (MiB) | 67.2 | 41.8 | 0.621 | 0.948 | 1.029 | 0.150 | 0.047 |
| burst end: pool capacity | 3118 | 733 | 0.235 | 0.628 | 1.025 | 0.504 | 0.264 |
| burst: pool created | 3146 | 832 | 0.264 | 0.893 | 0.331 | 0.358 | 0.973 |
| run: memory.peak (MiB) | 69.1 | 44.0 | 0.636 | 0.940 | 1.033 | 0.153 | 0.042 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |

### 0.6 CPUs

| Measure | native (6) | plan (6) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 78530 | 78550 | 1.000 | 1.001 | 1.000 | 0.000 | 0.000 |
| steady: CPU ns/metric | 2011 | 1945 | 0.967 | 1.032 | 1.024 | 0.068 | 0.049 |
| steady: CPU (cores) | 0.158 | 0.153 | 0.967 | 1.033 | 1.024 | 0.068 | 0.048 |
| steady: throttled periods | 0.267 | 0.263 | 0.983 | 0.621 | 0.621 | 0.304 | 0.295 |
| steady: throttled ms/s | 137 | 139 | 1.014 | 1.132 | 1.164 | 0.160 | 0.227 |
| steady: shortfall | 0.006 | 0.003 | 0.513 | 0.916 | - | 0.063 | 1.776 |
| steady: anon (MiB) | 56.0 | 35.1 | 0.627 | 0.929 | 1.102 | 0.100 | 0.088 |
| burst: decoded/s | 315167 | 315824 | 1.002 | 0.979 | 1.003 | 0.025 | 0.014 |
| burst+recovery: CPU ns/metric | 1803 | 1658 | 0.919 | 1.015 | 1.008 | 0.051 | 0.050 |
| burst: throttled periods | 0.867 | 0.784 | 0.904 | 0.920 | 0.962 | 0.061 | 0.063 |
| burst: throttled ms/s | 947 | 851 | 0.899 | 0.984 | 0.951 | 0.033 | 0.129 |
| burst: shortfall | 0.000 | 0.000 | - | - | - | - | - |
| burst: longest stall (ms) | 201 | 202 | 1.005 | 1.000 | 1.000 | 0.019 | 0.011 |
| burst: admission after / before the flush | 0.984 | 0.960 | 0.975 | 1.229 | 1.021 | 0.158 | 0.062 |
| burst+recovery: anon peak (MiB) | 77.2 | 48.1 | 0.623 | 0.950 | 1.062 | 0.043 | 0.055 |
| burst end: pool capacity | 4289 | 1294 | 0.302 | 0.864 | 1.185 | 0.174 | 0.164 |
| burst: pool created | 2320 | 1036 | 0.446 | 1.183 | 2.170 | 0.391 | 0.914 |
| run: memory.peak (MiB) | 79.8 | 51.5 | 0.645 | 0.980 | 1.071 | 0.023 | 0.055 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |

### 1.2 CPUs

| Measure | native (6) | plan (6) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 249903 | 249865 | 1.000 | 0.967 | 1.000 | 0.026 | 0.000 |
| steady: CPU ns/metric | 1591 | 1492 | 0.938 | 1.044 | 1.003 | 0.040 | 0.015 |
| steady: CPU (cores) | 0.398 | 0.373 | 0.936 | 1.008 | 1.003 | 0.017 | 0.015 |
| steady: throttled periods | 0.330 | 0.253 | 0.766 | 0.674 | 1.004 | 0.388 | 0.027 |
| steady: throttled ms/s | 134 | 123 | 0.914 | 0.941 | 1.011 | 0.095 | 0.061 |
| steady: shortfall | 0.000 | 0.000 | - | 0.000 | - | - | - |
| steady: anon (MiB) | 60.2 | 36.4 | 0.604 | 0.941 | 1.048 | 0.049 | 0.064 |
| burst: decoded/s | 766805 | 828898 | 1.081 | 1.036 | 0.982 | 0.028 | 0.015 |
| burst+recovery: CPU ns/metric | 1535 | 1425 | 0.928 | 0.977 | 1.009 | 0.018 | 0.009 |
| burst: throttled periods | 0.970 | 0.976 | 1.006 | 0.988 | 1.000 | 0.012 | 0.009 |
| burst: throttled ms/s | 596 | 605 | 1.016 | 0.948 | 0.995 | 0.054 | 0.035 |
| burst: shortfall | 0.209 | 0.149 | 0.713 | 0.886 | 1.056 | 0.100 | 0.041 |
| burst: longest stall (ms) | 0 | 0 | - | - | - | - | - |
| burst: admission after / before the flush | 0.965 | 0.996 | 1.032 | 1.027 | 1.025 | 0.051 | 0.036 |
| burst+recovery: anon peak (MiB) | 88.0 | 62.1 | 0.706 | 0.919 | 0.998 | 0.084 | 0.148 |
| burst end: pool capacity | 4582 | 1563 | 0.341 | 0.763 | 2.573 | 0.270 | 1.077 |
| burst: pool created | 3861 | 1565 | 0.405 | 0.810 | 2.440 | 0.346 | 0.925 |
| run: memory.peak (MiB) | 89.8 | 63.6 | 0.708 | 0.923 | 0.996 | 0.085 | 0.128 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |
