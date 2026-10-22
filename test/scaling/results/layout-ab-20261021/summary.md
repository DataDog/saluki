cpus: 0.3 0.6; labels: plan-a plan-b fused-a fused-b isolated-a isolated-b; rounds: 3; native_image: saluki-images/agent-data-plane:legacy-b940bf1f8b (sha256:03109eef3d3ebe2b58796edd93010b79159a7f2547ab5b3733e37d83e1af16be); plan_image: saluki-images/agent-data-plane:plan-n1 (sha256:d0b13f91c1aa4b85e38196ca854b901d02824e2eb6b421a2ca7e59c77f85d373); plan_config: /Users/luke.steensen/code/saluki/test/scaling/adp/lanes-off.yaml; candidate_image: saluki-images/agent-data-plane:layout-ab (sha256:36df446bb2f84a3119b9985de809524d8ba18e32e8123cf86d66db0d0e6ffc55), config /Users/luke.steensen/code/saluki/test/scaling/adp/lanes-off.yaml; variants (candidate image): fused=adp/lanes-off.yaml isolated=adp/isolated.yaml; contexts: 3000 (context limit 31000), cache 500 Mb, transport unix_datagram; steady_rate_per_cpu: 60 MiB; burst_factor: 4; steady_rates (KiB/s): 0.3:23295 0.6:66020; warmup 20 s, steady 30 s, burst lead 3 s, burst 8 s, recovery 12 s; adp cpuset 0-1, 2 workers, 2 decoders; lading 2-3

### 0.3 CPUs

| Measure | plan (6) | fused (6) | isolated (6) | fused / plan | isolated / fused | isolated / plan | A/A plan | A/A fused | A/A isolated | spread plan | spread fused | spread isolated |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| steady: decoded/s | 27729 | 27733 | 27724 | 1.000 | 1.000 | 1.000 | 0.968 | 1.000 | 0.968 | 0.025 | 0.025 | 0.025 |
| steady: CPU ns/metric | 3175 | 3279 | 3506 | 1.033 | 1.069 | 1.104 | 1.049 | 0.905 | 1.200 | 0.193 | 0.176 | 0.144 |
| steady: CPU (cores) | 0.089 | 0.091 | 0.097 | 1.017 | 1.068 | 1.086 | 1.015 | 0.905 | 1.161 | 0.182 | 0.154 | 0.122 |
| steady: throttled periods | 0.315 | 0.272 | 0.325 | 0.865 | 1.194 | 1.032 | 1.048 | 0.800 | 1.403 | 0.184 | 0.195 | 0.264 |
| steady: throttled ms/s | 246 | 280 | 343 | 1.139 | 1.222 | 1.392 | 1.060 | 1.202 | 1.290 | 0.393 | 0.238 | 0.178 |
| steady: shortfall | 0.005 | 0.005 | 0.003 | 0.988 | 0.511 | 0.504 | 1.071 | - | 0.000 | 0.055 | 0.761 | 2.075 |
| steady: anon (MiB) | 30.4 | 31.2 | 31.1 | 1.027 | 0.997 | 1.024 | 0.991 | 0.988 | 1.043 | 0.024 | 0.017 | 0.032 |
| burst: decoded/s | 85929 | 84742 | 84912 | 0.986 | 1.002 | 0.988 | 1.018 | 1.020 | 0.967 | 0.028 | 0.023 | 0.029 |
| burst+recovery: CPU ns/metric | 3271 | 3267 | 3322 | 0.999 | 1.017 | 1.015 | 1.008 | 0.962 | 1.051 | 0.077 | 0.056 | 0.055 |
| burst: throttled periods | 0.959 | 0.965 | 0.959 | 1.006 | 0.994 | 1.000 | 0.976 | 1.000 | 1.013 | 0.022 | 0.000 | 0.013 |
| burst: throttled ms/s | 1448 | 1461 | 1426 | 1.009 | 0.976 | 0.985 | 1.004 | 1.000 | 0.991 | 0.023 | 0.040 | 0.017 |
| burst: shortfall | 0.148 | 0.154 | 0.150 | 1.046 | 0.973 | 1.018 | 0.814 | 0.976 | 1.270 | 0.238 | 0.056 | 0.179 |
| burst: longest stall (ms) | 102 | 99 | 100 | 0.966 | 1.015 | 0.980 | 2.030 | 1.010 | 1.000 | 0.762 | 0.041 | 0.007 |
| burst: admission after / before the flush | 0.837 | 0.882 | 0.880 | 1.053 | 0.998 | 1.051 | 0.929 | 0.843 | 0.834 | 0.128 | 0.281 | 0.266 |
| burst+recovery: anon peak (MiB) | 43.6 | 42.8 | 42.1 | 0.982 | 0.983 | 0.966 | 1.078 | 1.026 | 1.000 | 0.066 | 0.043 | 0.022 |
| burst end: pool capacity | 764 | 824 | 854 | 1.080 | 1.036 | 1.119 | 1.476 | 0.769 | 1.671 | 0.349 | 0.225 | 0.550 |
| burst: pool created | 444 | 414 | 600 | 0.934 | 1.449 | 1.352 | 0.389 | 0.734 | 1.770 | 0.979 | 0.565 | 0.771 |
| run: memory.peak (MiB) | 46.3 | 45.7 | 45.9 | 0.988 | 1.004 | 0.992 | 0.985 | 1.030 | 1.048 | 0.025 | 0.039 | 0.094 |
| flushes in burst | 1 | 1 | 1 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 | 0.000 |

### 0.6 CPUs

| Measure | plan (6) | fused (6) | isolated (6) | fused / plan | isolated / fused | isolated / plan | A/A plan | A/A fused | A/A isolated | spread plan | spread fused | spread isolated |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| steady: decoded/s | 78549 | 78545 | 78558 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.000 | 0.001 | 0.025 |
| steady: CPU ns/metric | 2027 | 2015 | 1923 | 0.994 | 0.955 | 0.949 | 0.976 | 0.954 | 1.046 | 0.138 | 0.078 | 0.083 |
| steady: CPU (cores) | 0.159 | 0.158 | 0.154 | 0.994 | 0.970 | 0.964 | 0.976 | 0.956 | 1.047 | 0.138 | 0.077 | 0.074 |
| steady: throttled periods | 0.267 | 0.263 | 0.258 | 0.987 | 0.979 | 0.965 | 1.009 | 1.066 | 1.090 | 0.039 | 0.058 | 0.205 |
| steady: throttled ms/s | 165 | 167 | 148 | 1.014 | 0.883 | 0.896 | 1.034 | 0.923 | 1.274 | 0.466 | 0.275 | 0.306 |
| steady: shortfall | 0.005 | 0.005 | 0.004 | 1.146 | 0.699 | 0.801 | 1.340 | 0.981 | 0.000 | 0.933 | 0.057 | 1.350 |
| steady: anon (MiB) | 34.8 | 32.6 | 33.3 | 0.937 | 1.019 | 0.955 | 0.936 | 1.010 | 1.002 | 0.083 | 0.068 | 0.044 |
| burst: decoded/s | 298967 | 298516 | 299566 | 0.998 | 1.004 | 1.002 | 0.981 | 1.003 | 1.008 | 0.021 | 0.043 | 0.023 |
| burst+recovery: CPU ns/metric | 1767 | 1760 | 1682 | 0.996 | 0.956 | 0.952 | 1.058 | 0.961 | 1.006 | 0.074 | 0.055 | 0.087 |
| burst: throttled periods | 0.787 | 0.815 | 0.757 | 1.035 | 0.930 | 0.963 | 1.018 | 0.964 | 1.044 | 0.023 | 0.061 | 0.104 |
| burst: throttled ms/s | 914 | 896 | 833 | 0.980 | 0.930 | 0.912 | 1.032 | 1.009 | 1.017 | 0.093 | 0.081 | 0.102 |
| burst: shortfall | 0.000 | 0.000 | 0.000 | - | - | - | - | - | - | - | - | - |
| burst: longest stall (ms) | 202 | 204 | 201 | 1.010 | 0.985 | 0.995 | 1.000 | 1.000 | 1.010 | 0.011 | 0.007 | 0.379 |
| burst: admission after / before the flush | 0.971 | 0.911 | 0.969 | 0.938 | 1.064 | 0.998 | 1.059 | 1.111 | 1.001 | 0.044 | 0.090 | 0.056 |
| burst+recovery: anon peak (MiB) | 51.6 | 50.5 | 46.3 | 0.979 | 0.916 | 0.897 | 0.935 | 0.984 | 0.988 | 0.090 | 0.045 | 0.017 |
| burst end: pool capacity | 808 | 753 | 816 | 0.933 | 1.084 | 1.011 | 0.867 | 1.043 | 0.485 | 0.438 | 1.426 | 0.421 |
| burst: pool created | 548 | 1062 | 628 | 1.940 | 0.591 | 1.146 | 1.507 | 1.206 | 1.901 | 0.760 | 0.633 | 0.433 |
| run: memory.peak (MiB) | 53.8 | 52.0 | 48.4 | 0.966 | 0.931 | 0.900 | 0.939 | 0.983 | 0.983 | 0.084 | 0.047 | 0.027 |
| flushes in burst | 1 | 1 | 1 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 1.000 | 0.000 | 0.000 | 0.000 |
