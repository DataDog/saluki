Mode: paced. Images: main-a = saluki-images/agent-data-plane:main (a52c25dd52); main-b = saluki-images/agent-data-plane:main (a52c25dd52)

### medium

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.3 | main-a | 0.998 | 11,870 | 0.100 (0.33) | 8,449 | 0.32 | 0.95 / 1.45 (0) | 951 | 77 / 72 (100) | 0, 0 | 3 | 27,740 | 1.47 | 199.2 / 191.9 | 0.32 |
| 0.3 | main-b | 0.998 | 11,862 | 0.101 (0.34) | 8,512 | 0.33 | 0.90 / 1.44 (0) | 1 | 77 / 74 (100) | 0, 0 | 3 | 27,815 | 1.45 | 240.4 / 179.3 | 0.31 |

### heavy

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.6 | main-a | 1.007 | 60,459 | 0.416 (0.69) | 6,894 | 0.64 | 0.90 / 1.25 (0) | 9440 | 223 / 220 (250) | 0, 0 | 3 | 22,823 | 1.44 | 172.1 / 141.9 | 0.33 |
| 0.6 | main-b | 1.002 | 60,096 | 0.403 (0.67) | 6,704 | 0.63 | 0.90 / 1.27 (0) | 2798 | 202 / 196 (250) | 0, 0 | 3 | 22,866 | 1.48 | 168.9 / 134.7 | 0.33 |
| 1.2 | main-a | 0.998 | 60,324 | 0.410 (0.34) | 6,795 | 0.29 | 0.95 / 1.26 (0) | 1 | 181 / 179 (250) | 0, 0 | 3 | 23,056 | 1.47 | 154.7 / 141.8 | 0.37 |
| 1.2 | main-b | 0.998 | 60,320 | 0.407 (0.34) | 6,753 | 0.28 | 0.95 / 1.28 (0) | 1 | 195 / 192 (250) | 0, 0 | 3 | 23,034 | 1.48 | 177.3 / 165.0 | 0.36 |

### ultraheavy

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1.2 | main-a | 0.998 | 122,489 | 0.760 (0.63) | 6,210 | 0.57 | 0.87 / 1.19 (0) | 3475 | 300 / 295 (420) | 0, 0 | 3 | 21,312 | 1.49 | 144.4 / 131.3 | 0.34 |
| 1.2 | main-b | 0.998 | 122,051 | 0.770 (0.64) | 6,299 | 0.57 | 0.93 / 1.30 (0) | 5076 | 328 / 322 (420) | 0, 0 | 3 | 21,348 | 1.47 | 152.8 / 139.6 | 0.35 |

### main-b against main-a

| tier | CPUs | decoded ratio | CPU/metric ratio | anon memory ratio | decoded spread (main-a, main-b) | CPU/metric spread (main-a, main-b) | instructions/metric ratio | instructions/metric spread (main-a, main-b) |
|---|---|---|---|---|---|---|---|---|
| medium | 0.3 | 0.999 | 1.007 | 1.029 | 0.000, 0.001 | 0.012, 0.009 | 1.003 | 0.004, 0.006 |
| heavy | 0.6 | 0.994 | 0.972 | 0.893 | 0.005, 0.003 | 0.020, 0.007 | 1.002 | 0.006, 0.005 |
| heavy | 1.2 | 1.000 | 0.994 | 1.074 | 0.000, 0.000 | 0.019, 0.026 | 0.999 | 0.004, 0.002 |
| ultraheavy | 1.2 | 0.996 | 1.014 | 1.091 | 0.003, 0.001 | 0.025, 0.005 | 1.002 | 0.004, 0.006 |
