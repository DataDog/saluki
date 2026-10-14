Mode: paced. Images: fused = saluki-images/agent-data-plane:plan-x (862764b247); ref = saluki-images/agent-data-plane:plan-x (862764b247)

### low

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.3 | fused | 0.996 | 1,242 | 0.018 (0.06) | 14,636 | 0.02 | 0.94 / 1.92 (0) | 1 | 31 / 28 (60) | 0, 0 | 2 | 38,513 | 1.33 | 312.3 / 338.5 | 0.32 |
| 0.3 | ref | 0.996 | 1,242 | 0.018 (0.06) | 14,488 | 0.02 | 0.94 / 1.91 (0) | 1 | 31 / 28 (60) | 0, 0 | 2 | 38,389 | 1.33 | 389.1 / 307.0 | 0.31 |

### medium

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 1.2 | fused | 0.998 | 11,870 | 0.097 (0.08) | 8,192 | 0.00 | 0.95 / 1.92 (0) | 1 | 68 / 65 (100) | 0, 0 | 2 | 27,509 | 1.50 | 173.9 / 183.6 | 0.41 |
| 1.2 | ref | 0.998 | 11,870 | 0.097 (0.08) | 8,183 | 0.00 | 0.95 / 1.91 (0) | 1 | 67 / 65 (100) | 0, 0 | 2 | 27,496 | 1.50 | 241.5 / 167.1 | 0.26 |

### heavy

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.3 | fused | 0.700 | 42,195 | 0.300 (1.00) | 7,102 | 1.00 | 0.44 / 0.96 (8) | 19 | 125 / 123 (250) | 0, 0 | 2 | 24,328 | 1.50 | 176.2 / 140.5 | 0.35 |
| 0.3 | ref | 0.715 | 43,046 | 0.300 (1.00) | 6,975 | 1.00 | 0.46 / 0.98 (8.5) | 26 | 127 / 125 (250) | 0, 0 | 2 | 24,030 | 1.50 | 168.9 / 135.2 | 0.33 |

### ultraheavy

| CPUs | label | send | decoded/s | cores (share) | CPU ns/metric | throttled | worst / peak second (stalled s) | pool | peak / anon MiB (bound) | drops: agg, unresolved | rounds | instructions/metric | IPC | L1D / L2D refills per metric | back-end stalled share |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 0.6 | fused | 0.774 | 95,202 | 0.600 (1.00) | 6,303 | 1.00 | 0.48 / 0.99 (4.5) | 20 | 189 / 186 (420) | 0, 0 | 2 | 22,009 | 1.53 | 150.9 / 127.3 | 0.33 |
| 0.6 | ref | 0.804 | 98,873 | 0.600 (1.00) | 6,069 | 1.00 | 0.48 / 1.04 (2) | 58 | 195 / 191 (420) | 0, 0 | 2 | 21,753 | 1.55 | 145.3 / 121.2 | 0.33 |

### fused against ref

| tier | CPUs | decoded ratio | CPU/metric ratio | anon memory ratio | decoded spread (ref, fused) | CPU/metric spread (ref, fused) | instructions/metric ratio | instructions/metric spread (ref, fused) |
|---|---|---|---|---|---|---|---|---|
| low | 0.3 | 1.000 | 1.010 | 0.976 | 0.000, 0.000 | 0.003, 0.001 | 1.003 | 0.000, 0.001 |
| medium | 1.2 | 1.000 | 1.001 | 1.010 | 0.000, 0.000 | 0.002, 0.012 | 1.000 | 0.003, 0.001 |
| heavy | 0.3 | 0.980 | 1.018 | 0.984 | 0.007, 0.018 | 0.006, 0.017 | 1.012 | 0.003, 0.003 |
| ultraheavy | 0.6 | 0.963 | 1.039 | 0.973 | 0.011, 0.003 | 0.010, 0.003 | 1.012 | 0.005, 0.003 |
