Mode: paced. Images: main = saluki-images/agent-data-plane:main (a52c25dd52); plan = saluki-images/agent-data-plane:plan (eb1c4ee134)

### medium

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.3 | main | 0.998 | 11,842 | 0.104 (0.35) | 8,791 | 0.35 | 0.95 / 1.46 (0) | 788 | 77 / 72 (100) | 0, 0 | 1 | 28,080 | 1.44 | 210.4 / 208.7 | 0.37 |
| 0.3 | plan | 0.998 | 11,867 | 0.102 (0.34) | 8,567 | 0.33 | 0.95 / 1.69 (0) | 1 | 69 / 65 (100) | 0, 0 | 1 | 27,473 | 1.46 | 212.2 / 173.2 | 0.27 |

### heavy

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1.2 | main | 0.998 | 60,324 | 0.431 (0.36) | 7,148 | 0.27 | 0.95 / 1.28 (0) | 1 | 166 / 164 (250) | 0, 0 | 1 | 22,891 | 1.43 | 163.1 / 169.6 | 0.31 |
| 1.2 | plan | 0.998 | 60,316 | 0.407 (0.34) | 6,752 | 0.29 | 0.81 / 1.40 (0) | 1 | 130 / 127 (250) | 0, 0 | 1 | 22,514 | 1.50 | 143.5 / 138.3 | 0.31 |

### ultraheavy

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1.2 | main | 0.998 | 122,465 | 0.784 (0.65) | 6,398 | 0.64 | 0.93 / 1.21 (0) | 3423 | 365 / 306 (420) | 0, 0 | 1 | 21,297 | 1.46 | 150.4 / 113.4 | 0.35 |
| 1.2 | plan | 1.002 | 122,833 | 0.749 (0.62) | 6,099 | 0.61 | 0.86 / 1.46 (0) | 50 | 195 / 191 (420) | 0, 0 | 1 | 20,757 | 1.51 | 126.7 / 126.9 | 0.35 |

### plan against main

| tier | CPUs | decoded ratio | CPU/metric ratio | anon memory ratio | decoded spread (main, plan) | CPU/metric spread (main, plan) | instructions/metric ratio | instructions/metric spread (main, plan) |
|---|---|---|---|---|---|---|---|---|
| medium | 0.3 | 1.002 | 0.975 | 0.898 | 0.000, 0.000 | 0.000, 0.000 | 0.978 | 0.000, 0.000 |
| heavy | 1.2 | 1.000 | 0.945 | 0.775 | 0.000, 0.000 | 0.000, 0.000 | 0.984 | 0.000, 0.000 |
| ultraheavy | 1.2 | 1.003 | 0.953 | 0.623 | 0.000, 0.000 | 0.000, 0.000 | 0.975 | 0.000, 0.000 |
