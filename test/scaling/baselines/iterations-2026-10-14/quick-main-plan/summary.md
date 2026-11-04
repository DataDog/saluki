Mode: paced. Images: main = saluki-images/agent-data-plane:main (a52c25dd52); plan = saluki-images/agent-data-plane:plan (eb1c4ee134)

### medium

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.3 | main | 0.998 | 11,860 | 0.099 (0.33) | 8,371 | 0.32 | 0.89 / 1.50 (0) | 1 | 77 / 72 (100) | 0, 0 | 3 | 27,864 | 1.48 | 218.8 / 176.6 | 0.31 |
| 0.3 | plan | 0.998 | 11,870 | 0.100 (0.33) | 8,421 | 0.33 | 0.89 / 1.44 (0) | 1 | 67 / 64 (100) | 0, 0 | 3 | 27,576 | 1.49 | 187.8 / 151.7 | 0.36 |

### heavy

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.6 | main | 0.998 | 60,178 | 0.399 (0.67) | 6,636 | 0.65 | 0.88 / 1.20 (0) | 3487 | 204 / 201 (250) | 0, 0 | 3 | 22,793 | 1.48 | 165.3 / 135.0 | 0.37 |
| 0.6 | plan | 0.997 | 60,083 | 0.391 (0.65) | 6,508 | 0.65 | 0.79 / 1.35 (0) | 1 | 130 / 126 (250) | 0, 0 | 3 | 22,215 | 1.50 | 150.5 / 125.6 | 0.31 |
| 1.2 | main | 0.998 | 60,322 | 0.407 (0.34) | 6,753 | 0.31 | 0.95 / 1.30 (0) | 1 | 187 / 184 (250) | 0, 0 | 3 | 22,925 | 1.47 | 187.9 / 133.8 | 0.34 |
| 1.2 | plan | 0.998 | 60,318 | 0.396 (0.33) | 6,566 | 0.22 | 0.95 / 1.40 (0) | 1 | 131 / 129 (250) | 0, 0 | 3 | 22,403 | 1.49 | 151.7 / 145.2 | 0.31 |

### ultraheavy

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1.2 | main | 1.002 | 122,232 | 0.775 (0.65) | 6,341 | 0.63 | 0.91 / 1.26 (0) | 1212 | 331 / 290 (420) | 0, 0 | 3 | 21,568 | 1.48 | 155.4 / 128.3 | 0.30 |
| 1.2 | plan | 0.997 | 122,764 | 0.723 (0.60) | 5,886 | 0.61 | 0.81 / 1.45 (0) | 26 | 192 / 188 (420) | 0, 0 | 3 | 20,743 | 1.53 | 145.3 / 114.6 | 0.31 |

### plan against main

| tier | CPUs | decoded ratio | CPU/metric ratio | anon memory ratio | decoded spread (main, plan) | CPU/metric spread (main, plan) | instructions/metric ratio | instructions/metric spread (main, plan) |
|---|---|---|---|---|---|---|---|---|
| medium | 0.3 | 1.001 | 1.006 | 0.898 | 0.001, 0.000 | 0.017, 0.013 | 0.990 | 0.003, 0.025 |
| heavy | 0.6 | 0.998 | 0.981 | 0.630 | 0.004, 0.001 | 0.032, 0.018 | 0.975 | 0.010, 0.003 |
| heavy | 1.2 | 1.000 | 0.972 | 0.701 | 0.000, 0.000 | 0.037, 0.024 | 0.977 | 0.001, 0.004 |
| ultraheavy | 1.2 | 1.004 | 0.928 | 0.650 | 0.001, 0.001 | 0.011, 0.017 | 0.962 | 0.001, 0.005 |
