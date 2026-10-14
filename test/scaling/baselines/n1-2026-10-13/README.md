# N1 baseline: ADP at production CPU limits, 2026-10-13

The runs behind N1's results in `docs/design/programmable-metrics-next.md` ("N1 results"): `run-limits.sh` at 0.05,
0.3, 0.6, and 1.2 CPUs, three rounds per cell, on a 16-core Graviton (aarch64) Linux host with Docker 28.5 and lading
0.33.0. `main` is `saluki-images/agent-data-plane:main` built from `a52c25dd52`; `plan` is built from `eb1c4ee134`.

| Directory | Command |
|---|---|
| `aa/` | `SCALING_AA=1 SCALING_TIERS="low heavy" make limits-runs` |
| `paced/` | `make limits-runs` (the rendered lading configurations are alongside) |
| `saturate/` | `SCALING_MODE=saturate SCALING_TIERS="low heavy" make limits-runs` |

Each holds `runs.csv` (one row per run) and `summary.md` (`summarize_limits.py` at the time of the run). The per-second
series and ADP's logs stayed on the measuring host; without them, `summarize_limits.py runs.csv` still prints every
column but the worst and peak seconds, the buffers in use, and the anonymous memory, which `summary.md` keeps.
