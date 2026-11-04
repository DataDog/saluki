Mode: paced. Images: main = saluki-images/agent-data-plane:main (a52c25dd52); plan = saluki-images/agent-data-plane:plan-flow (7c7631a4bb)

### heavy

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.3 | main | 0.720 | 43,470 | 0.300 (1.00) | 6,897 | 1.00 | 0.54 / 1.04 (0) | 1442 | 140 / 137 (250) | 0, 0 | 2 |
| 0.3 | plan | 0.748 | 45,153 | 0.300 (1.00) | 6,641 | 1.00 | 0.54 / 1.07 (0) | 28 | 126 / 123 (250) | 0, 0 | 2 |
| 0.6 | main | 0.999 | 60,328 | 0.381 (0.64) | 6,316 | 0.68 | 0.55 / 1.26 (0) | 1 | 142 / 139 (250) | 0, 0 | 2 |
| 0.6 | plan | 1.001 | 60,359 | 0.370 (0.62) | 6,127 | 0.64 | 0.57 / 1.37 (0) | 1 | 133 / 130 (250) | 0, 0 | 2 |
| 1.2 | main | 0.999 | 60,356 | 0.379 (0.32) | 6,276 | 0.29 | 0.76 / 1.39 (0) | 1 | 138 / 135 (250) | 0, 0 | 2 |
| 1.2 | plan | 0.999 | 60,357 | 0.360 (0.30) | 5,964 | 0.26 | 0.85 / 1.30 (0) | 1 | 134 / 130 (250) | 0, 0 | 2 |

### ultraheavy

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 1.2 | main | 1.000 | 123,122 | 0.709 (0.59) | 5,757 | 0.56 | 0.78 / 1.21 (0) | 1485 | 203 / 200 (420) | 0, 0 | 2 |
| 1.2 | plan | 1.000 | 123,114 | 0.692 (0.58) | 5,624 | 0.58 | 0.77 / 1.16 (0) | 24 | 193 / 190 (420) | 0, 0 | 2 |

### plan against main

| tier | CPUs | decoded ratio | CPU/metric ratio | anon memory ratio | decoded spread (main, plan) | CPU/metric spread (main, plan) |
|---|---|---|---|---|---|---|
| heavy | 0.3 | 1.039 | 0.963 | 0.896 | 0.001, 0.009 | 0.001, 0.010 |
| heavy | 0.6 | 1.001 | 0.970 | 0.933 | 0.001, 0.000 | 0.004, 0.009 |
| heavy | 1.2 | 1.000 | 0.950 | 0.965 | 0.000, 0.000 | 0.022, 0.009 |
| ultraheavy | 1.2 | 1.000 | 0.977 | 0.953 | 0.000, 0.000 | 0.015, 0.013 |
