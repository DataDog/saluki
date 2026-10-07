# Go/Rust parity harness (CARD-10)

Evidence and tooling for the differential-correctness comparison between the Go
`anomalydetection-testbench` reference and the Saluki Rust port.

All heavyweight artifacts (the Go binary, the scenario recordings, every run
output) live under `/tmp/aad-parity/` and are **not** committed. This directory
commits the reproducible pieces: the Go instrumentation patch, the shared
configs, the comparator, and the artifact cross-checker.

## Provenance

| Field | Value |
|---|---|
| Go repo | `~/dd/datadog-agent` |
| Go HEAD SHA | `d6363dc6f6e70cdd7bea80b6b3e9d1498121bb00` (clean before and after; instrumentation reverted) |
| Go build command | `dda inv anomalydetection.build-testbench` from the Go repo root (**`dda inv` path used; no fallback needed**) |
| Underlying build | `go build -tags python,anomalydetectiontestbench -o bin/anomalydetection-testbench ./internal/qbranch/anomalydetection-testbench` |
| Unpatched Go binary | `/tmp/aad-parity/bin/anomalydetection-testbench`, sha256 `e602dfa57639b6932fa25eff046a5c929471ee3f9e150c7d8e41aa957b54ea3c` |
| Patched Go binary | `/tmp/aad-parity/bin/anomalydetection-testbench-patched`, sha256 `b400553a2555947e6bb888a943262eaaca6c6b12fefc087d368af9098f12ee01` |
| Rust commit (base) | `ce3957d23d1e25a496a0eb3826e69f618beae2b6` |
| Rust binary | `target/release/anomaly-detection-testbench`, sha256 `161581e8baebd3cd1d254b19855a4c00b2ecf9dfab98cd9e15b72b1fe3810bde` |
| Scenarios dir | `/home/bits/Documents/observer-scenarios` (both scenarios are Parquet **v1**) |

## Go instrumentation patch

`go-testbench-scorestate.patch` is a diagnostic-only patch against the pinned Go
HEAD. It adds a headless-only `--score-state-output <path>` flag that dumps the
scorer's accumulated per-second state, and extends `AnomalyScoreBucket` with
`input`, `raw_level`, and `delivered_level` fields. The Go scorer runs with
cooldown `0` in both parity configs, so `delivered_level == raw_level`; the
field is recorded explicitly so the comparator never has to recompute it.

Apply / revert:

```sh
cd ~/dd/datadog-agent
git apply /path/to/tools/aad-parity/go-testbench-scorestate.patch
dda inv anomalydetection.build-testbench
cp bin/anomalydetection-testbench /tmp/aad-parity/bin/anomalydetection-testbench-patched
git checkout -- comp/anomalydetection/observer/def/types.go \
                comp/anomalydetection/observer/impl/anomaly_scorer.go \
                internal/qbranch/anomalydetection-testbench/bench/bench.go \
                internal/qbranch/anomalydetection-testbench/main.go
```

**Proof the patch does not change analytical output:** on
`kafka-partition-saturation` (metrics-only, retained) the unpatched and patched
binaries produce byte-identical observer output:

```sh
cmp /tmp/aad-parity/go/_bytecheck/kafka-unpatched.json \
    /tmp/aad-parity/go/kafka-partition-saturation-metricsonly.json   # identical
```

The same config file (`configs/metrics-only.json`) was used for both binaries.

## Configs

* `configs/metrics-only.json` — README §13.1 metrics-only vertical slice
  (BOCPD + scorer only, all extractors/time_cluster/passthrough disabled,
  `--baseline-duration 0`), plus `max_buckets: 100000000` so the Go scorer keeps
  the full per-second history for the score-state dump.
* `configs/all-detectors.json` — all five metric detectors + scorer with the
  testbench-tuned warmups (`bocpd 40`, `holt 15/25`, `tukey 40/40`) written
  explicitly, extractors disabled, plus `max_buckets`. This is the explicit
  equivalent of each tool's default all-detector profile, so both sides resolve
  identical settings.

Both configs disable the baseline via `--baseline-duration 0`.

## Run commands

Recorded in `run_parity.sh`; both scenarios are run with `--retain-parquet`
(the recordings are unordered on disk, so README §13.1 requires retained mode on
both sides), `--include-detector-anomalies`, and `--baseline-duration 0`.

Go:
```sh
/tmp/aad-parity/bin/anomalydetection-testbench-patched --headless <scenario> \
  --scenarios-dir /home/bits/Documents/observer-scenarios \
  --config tools/aad-parity/configs/<config>.json \
  --baseline-duration 0 --include-detector-anomalies --retain-parquet \
  --output /tmp/aad-parity/go/<scenario>-<mode>.json \
  --score-state-output /tmp/aad-parity/go/<scenario>-<mode>-scorestate.json
```

Rust:
```sh
target/release/anomaly-detection-testbench --headless <scenario> \
  --scenarios-dir /home/bits/Documents/observer-scenarios \
  --config tools/aad-parity/configs/<config>.json \
  --baseline-duration 0 --include-detector-anomalies --retain-parquet \
  --output /tmp/aad-parity/rust/<scenario>-<mode>.json
```

`<mode>` is `metricsonly` (gated) or `default` (context).

## Comparator and cross-check

```sh
python3 tools/aad-parity/compare.py --root /tmp/aad-parity \
  --out-json /tmp/aad-parity/summary.json --out-md /tmp/aad-parity/summary.md
python3 tools/aad-parity/compare.py --self-test
python3 tools/aad-parity/crosscheck_artifacts.py --scenario <scenario> \
  --scenarios-dir /home/bits/Documents/observer-scenarios \
  --rust /tmp/aad-parity/rust/<scenario>-default.json \
  --out /tmp/aad-parity/comparison/<scenario>-crosscheck.json
```

`compare.py` exits non-zero when a gated case fails. `metricsonly` is gated
(exact emissions/severity, score ticks within `1e-10` abs / `1e-8` rel);
`default` is the non-gated context run and applies the README §8.3 mixed-curve
guardrails.

## Headline results

| scenario | mode | verdict | EWMA MAE | P95 | max | raw sev % | ledger go/rust | exact |
|---|---|---|---|---|---|---|---|---|
| kafka-partition-saturation | metricsonly | PASS | 1.6e-16 | 5.0e-16 | 8.9e-16 | 100.000% | 498/498 | yes |
| dns-upstream-outage | metricsonly | PASS | 1.6e-16 | 6.1e-16 | 1.3e-15 | 100.000% | 865/865 | yes |
| kafka-partition-saturation | default | PASS | 2.4e-16 | 8.9e-16 | 1.6e-15 | 100.000% | 5242/5242 | yes |
| dns-upstream-outage | default | PASS | 1.2e-04 | 6.5e-04 | 3.9e-03 | 100.000% | 17233/17237 | **no** |

**Finding (open):** on `dns-upstream-outage` (all five detectors) the Rust
`scanwelch` detector emits 4 extra changepoints (and 4 at different seconds)
versus Go: 5037 vs 5033, `go_only=4`, `rust_only=8`, confined to
`kubernetes.kubelet.pleg.last_seen` and `coredns.go.memstats.last_gc_time_seconds`.
Everything else (bocpd, scanmw, holt_residual, tukey_biweight) matches exactly,
severity agreement is 100%, and the score curve stays well inside the context
guardrails (MAE 1.2e-4, max 3.9e-3). The gated metrics-only BOCPD runs are exact.
The divergence is **deterministic**: two independent Rust runs of the same case
produce byte-identical ledgers (17237 anomalies each), so it is a ScanWelch port
discrepancy, not run-to-run noise. It is worth its own follow-up card; no
parameter was retuned to mask it.
