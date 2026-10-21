cpus: 0.05 0.3 0.6 1.2; labels: native-a native-b plan-a plan-b; rounds: 3; native_image: saluki-images/agent-data-plane:legacy-b940bf1f8b (sha256:03109eef3d3ebe2b58796edd93010b79159a7f2547ab5b3733e37d83e1af16be); plan_image: saluki-images/agent-data-plane:plan-n1 (sha256:d0b13f91c1aa4b85e38196ca854b901d02824e2eb6b421a2ca7e59c77f85d373); plan_config: /Users/luke.steensen/code/saluki/test/scaling/adp/lanes-off.yaml; contexts: 100000 (context limit 1001000), cache 2000 Mb, transport unix_datagram; steady_rate_per_cpu: 60 MiB; burst_factor: 4; steady_rates (KiB/s): 0.05:739 0.3:11840 0.6:43470 1.2:166110; warmup 20 s, steady 30 s, burst lead 3 s, burst 8 s, recovery 12 s; adp cpuset 0-1, 2 workers, 2 decoders; lading 2-3

### 0.05 CPUs

| Measure | native (6) | plan (6) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 828 | 831 | 1.003 | 0.973 | 1.029 | 0.021 | 0.023 |
| steady: CPU ns/metric | 50001 | 48166 | 0.963 | 1.055 | 0.982 | 0.053 | 0.041 |
| steady: CPU (cores) | 0.042 | 0.040 | 0.962 | 1.027 | 1.035 | 0.030 | 0.033 |
| steady: throttled periods | 0.887 | 0.851 | 0.959 | 0.996 | 1.037 | 0.023 | 0.048 |
| steady: throttled ms/s | 1429 | 1358 | 0.951 | 1.006 | 1.026 | 0.033 | 0.029 |
| steady: shortfall | 0.052 | 0.053 | 1.026 | 2.005 | 0.352 | 0.655 | 0.730 |
| steady: anon (MiB) | 65.9 | 67.7 | 1.027 | 0.953 | 0.986 | 0.037 | 0.016 |
| burst: decoded/s | 1212 | 1069 | 0.882 | 0.650 | 0.847 | 0.335 | 0.154 |
| burst+recovery: CPU ns/metric | 48235 | 52985 | 1.098 | 1.103 | 1.097 | 0.090 | 0.160 |
| burst: throttled periods | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.000 | 0.018 |
| burst: throttled ms/s | 1845 | 1800 | 0.975 | 1.014 | 0.993 | 0.011 | 0.040 |
| burst: shortfall | 0.561 | 0.671 | 1.198 | 0.957 | 1.075 | 0.079 | 0.087 |
| burst: longest stall (ms) | 1101 | 551 | 0.501 | 1.443 | 1.200 | 0.317 | 0.184 |
| burst: admission after / before the flush | 0.353 | 0.881 | 2.498 | 2.028 | 0.898 | 1.728 | 0.275 |
| burst+recovery: anon peak (MiB) | 111.5 | 105.0 | 0.941 | 1.011 | 0.980 | 0.025 | 0.016 |
| burst end: pool capacity | 1081 | 128 | 0.118 | 1.435 | 1.000 | 0.459 | 0.000 |
| burst: pool created | 934 | 0 | 0.000 | 1.479 | - | 0.597 | - |
| run: memory.peak (MiB) | 113.3 | 107.3 | 0.947 | 1.009 | 0.993 | 0.065 | 0.038 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |

### 0.3 CPUs

| Measure | native (6) | plan (6) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 14169 | 14171 | 1.000 | 1.033 | 1.034 | 0.033 | 0.033 |
| steady: CPU ns/metric | 11530 | 10844 | 0.941 | 1.014 | 1.003 | 0.030 | 0.034 |
| steady: CPU (cores) | 0.164 | 0.156 | 0.955 | 0.981 | 1.003 | 0.038 | 0.033 |
| steady: throttled periods | 0.617 | 0.590 | 0.956 | 0.981 | 1.042 | 0.058 | 0.081 |
| steady: throttled ms/s | 576 | 542 | 0.941 | 1.005 | 0.988 | 0.106 | 0.037 |
| steady: shortfall | 0.006 | 0.005 | 0.839 | 1.245 | - | 0.215 | 0.754 |
| steady: anon (MiB) | 337.4 | 317.9 | 0.942 | 0.984 | 0.968 | 0.018 | 0.063 |
| burst: decoded/s | 37007 | 36156 | 0.977 | 0.974 | 1.034 | 0.060 | 0.069 |
| burst+recovery: CPU ns/metric | 8820 | 8638 | 0.979 | 0.994 | 0.938 | 0.065 | 0.086 |
| burst: throttled periods | 0.909 | 0.889 | 0.978 | 0.972 | 0.998 | 0.036 | 0.002 |
| burst: throttled ms/s | 1418 | 1307 | 0.922 | 0.918 | 1.011 | 0.076 | 0.047 |
| burst: shortfall | 0.250 | 0.310 | 1.239 | 1.032 | 0.978 | 0.181 | 0.178 |
| burst: longest stall (ms) | 597 | 600 | 1.004 | 0.997 | 1.015 | 0.250 | 0.014 |
| burst: admission after / before the flush | 0.637 | 0.468 | 0.735 | 1.144 | 1.148 | 0.260 | 0.188 |
| burst+recovery: anon peak (MiB) | 463.7 | 393.0 | 0.847 | 0.969 | 1.019 | 0.036 | 0.028 |
| burst end: pool capacity | 6995 | 418 | 0.060 | 0.765 | 0.417 | 0.260 | 2.061 |
| burst: pool created | 6336 | 260 | 0.041 | 0.894 | 0.293 | 0.147 | 1.601 |
| run: memory.peak (MiB) | 467.0 | 396.0 | 0.848 | 0.969 | 1.017 | 0.037 | 0.028 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |

### 0.6 CPUs

| Measure | native (6) | plan (6) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 51986 | 51158 | 0.984 | 1.032 | 0.999 | 0.033 | 0.001 |
| steady: CPU ns/metric | 3195 | 3184 | 0.997 | 1.083 | 0.997 | 0.061 | 0.047 |
| steady: CPU (cores) | 0.166 | 0.164 | 0.988 | 1.046 | 1.011 | 0.040 | 0.040 |
| steady: throttled periods | 0.321 | 0.349 | 1.088 | 1.015 | 0.931 | 0.063 | 0.055 |
| steady: throttled ms/s | 123 | 161 | 1.305 | 1.022 | 0.887 | 0.397 | 0.259 |
| steady: shortfall | 0.005 | 0.005 | 0.936 | 1.140 | 0.973 | 0.132 | 0.063 |
| steady: anon (MiB) | 318.2 | 293.7 | 0.923 | 0.976 | 1.001 | 0.020 | 0.005 |
| burst: decoded/s | 181865 | 181145 | 0.996 | 0.960 | 1.018 | 0.044 | 0.091 |
| burst+recovery: CPU ns/metric | 2785 | 2648 | 0.951 | 1.084 | 0.969 | 0.091 | 0.126 |
| burst: throttled periods | 0.841 | 0.799 | 0.950 | 1.023 | 1.022 | 0.063 | 0.023 |
| burst: throttled ms/s | 1005 | 892 | 0.887 | 1.127 | 0.982 | 0.131 | 0.083 |
| burst: shortfall | 0.001 | 0.002 | 1.164 | - | 0.000 | 5.275 | 40.387 |
| burst: longest stall (ms) | 500 | 594 | 1.187 | 0.998 | 1.012 | 0.138 | 0.117 |
| burst: admission after / before the flush | 0.791 | 0.744 | 0.941 | 1.068 | 1.219 | 0.080 | 0.319 |
| burst+recovery: anon peak (MiB) | 495.1 | 349.7 | 0.706 | 0.996 | 0.997 | 0.193 | 0.016 |
| burst end: pool capacity | 18851 | 1180 | 0.063 | 0.952 | 1.005 | 0.571 | 0.139 |
| burst: pool created | 17133 | 514 | 0.030 | 1.141 | 0.856 | 0.729 | 1.043 |
| run: memory.peak (MiB) | 497.7 | 353.2 | 0.710 | 0.996 | 0.993 | 0.192 | 0.018 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |

### 1.2 CPUs

| Measure | native (6) | plan (6) | plan / native | A/A native | A/A plan | spread native | spread plan |
|---|---|---|---|---|---|---|---|
| steady: decoded/s | 195987 | 198891 | 1.015 | 1.026 | 1.032 | 0.021 | 0.033 |
| steady: CPU ns/metric | 2062 | 1914 | 0.928 | 0.970 | 1.014 | 0.024 | 0.033 |
| steady: CPU (cores) | 0.403 | 0.382 | 0.947 | 0.994 | 1.030 | 0.005 | 0.031 |
| steady: throttled periods | 0.326 | 0.369 | 1.131 | 0.959 | 0.915 | 0.088 | 0.170 |
| steady: throttled ms/s | 142 | 132 | 0.930 | 0.977 | 1.004 | 0.024 | 0.065 |
| steady: shortfall | 0.005 | 0.005 | 0.990 | 1.153 | 0.954 | 0.122 | 0.045 |
| steady: anon (MiB) | 331.4 | 301.8 | 0.911 | 0.992 | 0.995 | 0.008 | 0.009 |
| burst: decoded/s | 535148 | 571251 | 1.067 | 0.989 | 0.999 | 0.018 | 0.012 |
| burst+recovery: CPU ns/metric | 2018 | 1897 | 0.940 | 0.988 | 0.998 | 0.015 | 0.013 |
| burst: throttled periods | 0.928 | 0.938 | 1.011 | 1.000 | 1.000 | 0.009 | 0.008 |
| burst: throttled ms/s | 622 | 572 | 0.921 | 0.973 | 1.038 | 0.036 | 0.050 |
| burst: shortfall | 0.214 | 0.163 | 0.761 | 1.003 | 1.008 | 0.020 | 0.085 |
| burst: longest stall (ms) | 500 | 450 | 0.899 | 1.004 | 0.794 | 0.003 | 0.227 |
| burst: admission after / before the flush | 0.656 | 0.706 | 1.076 | 1.084 | 0.950 | 0.067 | 0.059 |
| burst+recovery: anon peak (MiB) | 600.3 | 354.6 | 0.591 | 1.163 | 0.969 | 0.143 | 0.024 |
| burst end: pool capacity | 28250 | 2676 | 0.095 | 1.376 | 0.758 | 0.305 | 0.214 |
| burst: pool created | 25195 | 1338 | 0.053 | 1.404 | 0.837 | 0.322 | 0.866 |
| run: memory.peak (MiB) | 603.4 | 359.2 | 0.595 | 1.160 | 0.980 | 0.141 | 0.017 |
| flushes in burst | 1 | 1 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 |
