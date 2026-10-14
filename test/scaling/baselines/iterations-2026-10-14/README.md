# First iterations after N1, 2026-10-14

The runs behind "What the first iterations showed" in `docs/design/programmable-metrics-next.md` and the findings in
`docs/design/adp-efficiency-findings.md`. Quick-loop runs: the focus cells, three slots in parallel (two cores for ADP
each), 30-second windows, and `perf stat` counts (`SCALING_PERF=1`). `plan-x` is the plan (`eb1c4ee134`) with the
experimental `SALUKI_EXPERIMENT_FUSE_ALL` switch; every hypothesis variant runs the same `plan-x` binary, differing only
in environment (`env-<label>.env`).

| Directory | What |
|---|---|
| `quick-aa/` | the quick loop as an A/A (main against main), to validate it |
| `quick-main-plan/` | the quick loop as main against plan, against the sequential N1 baseline |
| `profiles/` | main and plan with instruction sampling (`SCALING_RECORD=1`); `components.txt` breaks the samples down by component, columns in order: medium 0.3 main, plan; heavy 1.2 main, plan; ultraheavy 1.2 main, plan |
| `hyp-1/` | H1 (`cts16`), H9 (`tcache`), H2 (`fused`), and all three, against `ref`, at the focus cells |
| `hyp-2/` | H2 at four more cells |
| `sizeof-audit.txt` | `size_of` of the data model's types, from `tools/sizeof_audit.rs` run as a temporary `saluki-core` integration test |
| `tools/` | the scripts that folded the profiles (`perf script --no-inline -F comm,tid,period,ip,sym` output in): `components.py`, `hashers.py`, `fold.py` |

The `perf` data files, per-second series, and logs stayed on the measuring host.
