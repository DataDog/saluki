# otlp-log-agent Binary Size Analysis

## Table of Contents

- [Dependency Span Tree](#dependency-span-tree)
- [Build Profiles](#build-profiles)
  - [Build commands](#build-commands)
  - [Profile definitions](#profile-definitions-workspace-cargotoml)
  - [What each setting does](#what-each-setting-does)
- [Why .text is 4.0 MiB but the binary is 7.5 MB](#why-text-is-40-mib-but-the-binary-is-75-mb)
- [Binary Size Breakdown (.text section = 4.0 MiB of 7.5 MB file)](#binary-size-breakdown-text-section--40-mib-of-75-mb-file)
  - [Crypto / TLS — 829 KiB (20.7%)](#crypto--tls--829-kib-207-of-text)
  - [Saluki crates — 876 KiB (21.9%)](#saluki-crates--876-kib-219-of-text)
  - [Regex / DNS — 400 KiB (10.0%)](#regex--dns--400-kib-100-of-text)
  - [Networking / HTTP — 265 KiB (6.6%)](#networking--http--265-kib-66-of-text)
  - [Async runtime — 87 KiB (2.2%)](#async-runtime--87-kib-22-of-text)
  - [Compression — 88 KiB (2.2%)](#compression--88-kib-22-of-text)
  - [Tracing / Logging — 84 KiB (2.1%)](#tracing--logging--84-kib-21-of-text)
  - [Stdlib + Unattributed — 1.05 MiB (26.3%)](#stdlib--unattributed--105-mib-263-of-text)
- [Size Reduction Opportunities](#size-reduction-opportunities)

---

## Dependency Span Tree

Each node is labelled `crate [own KiB | subtotal KiB]`.
`own` = code attributed directly to that crate by cargo-bloat.
`subtotal` = own + all children in that sub-tree.
Sizes reflect the `.text` section (4.0 MiB total) of the `size-optimized-release` build.

```
otlp-log-agent [28 KiB | 4,096 KiB ≈ 4.0 MiB .text]
│
├─┬─ [Stdlib + fat-LTO unknown] ──────────────────────── 1,051 KiB (25.7%)
│ ├── std                               [546 KiB]
│ └── [Unknown]  (fat-LTO inlined)      [505 KiB]
│
├─┬─ [Saluki crates] ─────────────────────────────────── 879 KiB (21.5%)
│ ├─┬─ saluki_components                [472 KiB own]
│ │ ├── common::datadog                   [49 KiB]  HTTP forwarder I/O loop
│ │ ├── sources::otlp                     [34 KiB]  OTLP gRPC/HTTP source
│ │ ├── common::otlp                      [24 KiB]  OTLP→DD log translation
│ │ ├── forwarders::datadog + encoders     [<1 KiB]
│ │ └── (monomorphized generics)         [271 KiB]  tracing/hyper/figment stamps
│ ├── saluki_io                          [159 KiB]  I/O, codecs, transports
│ ├── saluki_core                         [84 KiB]  topology, event model, traits
│ ├── saluki_app                          [45 KiB]  bootstrap / lifecycle
│ ├── saluki_context                      [31 KiB]  tag & context resolution
│ ├── saluki_config                       [29 KiB]  config loading (Figment)
│ ├── otlp_log_agent (main)              [28 KiB]
│ ├── saluki_common                       [13 KiB]
│ ├── memory_accounting                    [5 KiB]
│ ├── saluki_env                           [7 KiB]
│ ├── saluki_health                        [5 KiB]
│ └── saluki_tls + saluki_metrics          [<2 KiB]
│
├─┬─ [Crypto / TLS] ──────────────────────────────────── 842 KiB (20.6%)
│ ├── aws_lc_sys          (C library)    [548 KiB]  ← single largest crate
│ ├── rustls                             [248 KiB]  TLS implementation
│ ├── webpki                              [22 KiB]  X.509 cert validation
│ ├── rustls_native_certs                 [11 KiB]  OS cert store
│ ├── aws_lc_rs                           [10 KiB]  Rust bindings to aws-lc-sys
│ └── rustls_pki_types                     [3 KiB]
│
├─┬─ [DNS + Regex]  ← entirely transitive ───────────── 460 KiB (11.2%)
│ ├── regex_automata                     [182 KiB]  DFA/NFA engine
│ ├── hickory_proto                       [86 KiB]  DNS wire protocol
│ ├── regex_syntax                        [80 KiB]  regex parser
│ ├── hickory_resolver                    [57 KiB]  async DNS resolver
│ └── aho_corasick                        [55 KiB]  multi-pattern search
│     (pulled in by hickory-resolver + tracing-subscriber::EnvFilter)
│
├─┬─ [HTTP / Networking] ─────────────────────────────── 267 KiB (6.5%)
│ ├── tonic                               [72 KiB]  gRPC (OTLP ingestion)
│ ├── h2                                  [63 KiB]  HTTP/2
│ ├── hyper                               [33 KiB]  HTTP/1.1 client
│ ├── http                                [33 KiB]  HTTP types
│ ├── hyper_util                          [24 KiB]  connection utilities
│ ├── axum + axum_core                    [17 KiB]  HTTP routing
│ ├── hyper_hickory                       [14 KiB]  DNS connector
│ ├── hyper_http_proxy                     [6 KiB]  proxy support
│ ├── hyper_rustls                         [3 KiB]  TLS connector
│ └── httparse                             [2 KiB]  HTTP/1 parser
│
├─┬─ [Misc dependencies] ─────────────────────────────── 246 KiB (6.0%)
│ ├── figment                             [43 KiB]  config deserialization
│ ├── chrono                              [33 KiB]  date/time
│ ├── url + idna                          [35 KiB]  URL parsing
│ ├── serde_json                          [20 KiB]
│ ├── otlp_protos + datadog_protos        [14 KiB]  protobuf generated code
│ ├── bytes + memchr                      [14 KiB]
│ ├── hashbrown                           [16 KiB]  hash map internals
│ ├── crossbeam_channel                   [11 KiB]
│ ├── metrics + metrics_util              [11 KiB]
│ ├── moka + quick_cache                   [8 KiB]
│ ├── stringtheory                         [7 KiB]
│ └── prost + other small crates          [34 KiB]
│
├─┬─ [Compression] ───────────────────────────────────── 96 KiB (2.3%)
│ ├── zstd_sys             (C library)    [79 KiB]
│ ├── compression_codecs                   [8 KiB]
│ ├── miniz_oxide                          [7 KiB]  deflate (zlib)
│ └── async_compression + zstd             [2 KiB]
│
├─┬─ [Async runtime] ─────────────────────────────────── 86 KiB (2.1%)
│ ├── tokio                               [83 KiB]  multi-thread scheduler
│ └── tokio_util + tokio_rustls            [3 KiB]
│
└─┬─ [Tracing / Logging] ─────────────────────────────── 80 KiB (2.0%)
  ├── tracing_subscriber                  [64 KiB]  formatting, EnvFilter
  ├── tracing_core                         [6 KiB]
  ├── tracing_appender                     [5 KiB]  file output
  └── tracing_rolling_file + log + serde   [5 KiB]
```

> **Note:** `[Unknown]` (505 KiB) is code that fat LTO inlined so aggressively across
> crate boundaries that DWARF debug attribution was lost. It is real compiled code,
> not overhead — cargo-bloat simply cannot attribute it to a source crate.



## Build Profiles

Three profiles are relevant, each building on the previous:

| Profile | `opt-level` | LTO | Codegen units | Debug | Strip | File size |
|---|---|---|---|---|---|---|
| `optimized-release` | `3` | fat | 1 | true | no | 15 MB |
| `size-optimized-release` (before debug/strip) | `z` | fat | 1 | true | no | 12.5 MB |
| `size-optimized-release` (final) | `z` | fat | 1 | false | symbols | 7.5 MB |

### Build commands

```bash
# Maximum runtime performance (fat LTO, no size tuning) — 15 MB
cargo build --profile optimized-release --package otlp-log-agent

# Minimum binary size (fat LTO + opt-level=z + no debug + stripped) — 7.5 MB
cargo build --profile size-optimized-release --package otlp-log-agent

# Inspect per-crate .text contribution (requires: cargo install cargo-bloat)
cargo bloat --profile size-optimized-release --package otlp-log-agent --crates -n 200
```

### Profile definitions (workspace `Cargo.toml`)

```toml
[profile.optimized-release]
inherits = "release"
lto = "fat"
codegen-units = 1

[profile.size-optimized-release]
inherits = "optimized-release"
opt-level = "z"
debug = false
strip = "symbols"
```

### What each setting does

- **`lto = "fat"`** — whole-program link-time optimization: LLVM sees all crates as one unit, enabling cross-crate inlining and dead code elimination.
- **`codegen-units = 1`** — forces single-threaded codegen so LLVM has global visibility for optimization passes.
- **`opt-level = "z"`** — instructs LLVM to minimize code size over speed (avoids loop unrolling, aggressive inlining). Cuts ~2 MB vs `opt-level = 3`.
- **`debug = false`** — omits DWARF debug info. Accounts for ~5 MB of the original 12.5 MB file.
- **`strip = "symbols"`** — removes symbol table from final binary. Useful for deployed artifacts; keeps `debug = false` output lean.

---

## Why .text is 4.0 MiB but the binary is 7.5 MB

`cargo bloat` only measures the `.text` section (compiled machine code). The rest of
the binary consists of sections the linker always emits:

```
7.5 MB binary (size-optimized-release, debug=false, strip=symbols)
├── .text         (compiled code)           ~4.0 MB  ← what cargo-bloat covers
├── __DATA_CONST  (GOT, static data)        ~1.6 MB  linker overhead, vtables
├── __const       (string literals, tables) ~0.9 MB  static read-only data
├── __eh_frame    (stack unwind info)       ~0.66 MB  per-call-frame metadata
├── __gcc_except_tab (panic unwind tables)  ~0.35 MB  per-function unwind data
└── __unwind_info (compact unwind)          ~0.12 MB  macOS unwind encoding
                                            ────────
                                            ~7.6 MB
```

`strip = "symbols"` removed the DWARF debug info (which was ~5 MB), but it does not
touch these structural sections — they are required for correct stack unwinding and
dynamic linking at runtime.

To shrink the non-`.text` portion further:

| Section | How to reduce | Estimated savings |
|---|---|---|
| `__gcc_except_tab` + `__eh_frame` | Add `panic = "abort"` to the profile | ~1 MB |
| `__DATA_CONST` | No practical knob; fixed linker overhead | — |
| `__const` | Reduce static string data / lookup tables | Marginal |

Adding `panic = "abort"` to `size-optimized-release` would eliminate all unwind tables
since the runtime never needs to walk the stack on panic. The trade-off is that panics
abort the process immediately with no unwinding (no `Drop` impls run on the panic path).

## Binary Size Breakdown (`.text` section = 4.0 MiB of 7.5 MB file)

Analysis produced with `cargo bloat --profile size-optimized-release --package otlp-log-agent --crates -n 200`.

> Note: `cargo bloat` numbers are approximate. The `.text` column reflects actual compiled code; the file size includes constants, unwind tables, and dynamic linking metadata.

### Crypto / TLS — 829 KiB (20.7% of .text)

| Crate | Size | Role |
|---|---|---|
| `aws_lc_sys` | 548 KiB | AWS libcrypto fork (C library, compiled in wholesale) |
| `rustls` | 248 KiB | TLS implementation |
| `rustls_native_certs` | 11 KiB | OS certificate store loading |
| `webpki` | 22 KiB | X.509 certificate path validation |
| `aws_lc_rs` | 10 KiB | Rust bindings to `aws-lc-sys` |

`aws_lc_sys` is the single largest crate in the binary — larger than all saluki crates combined. It is a transitive dependency via `rustls → aws-lc-rs → aws-lc-sys`. Being a C library, it is unaffected by `opt-level = "z"` (which only applies to Rust codegen).

### Saluki crates — 876 KiB (21.9% of .text)

| Crate | Size | Role |
|---|---|---|
| `saluki_components` | 472 KiB | All pipeline components (sources, transforms, encoders, forwarders) |
| `saluki_io` | 159 KiB | I/O primitives, codecs, network transports |
| `saluki_core` | 84 KiB | Topology engine, event model, component traits |
| `saluki_app` | 45 KiB | Application bootstrap / lifecycle |
| `saluki_context` | 31 KiB | Tag and context resolution |
| `saluki_config` | 29 KiB | Configuration loading (Figment-based) |
| `otlp_log_agent` | 28 KiB | The binary's own `main` + wiring |
| `saluki_common` | 13 KiB | Shared utility types |
| `saluki_env` | 7 KiB | Host/environment metadata detection |
| `saluki_health` | 5 KiB | Health checking |
| `saluki_tls` + `saluki_metrics` | <2 KiB | TLS helpers, internal metrics |

`saluki_components` pulls in all component implementations — DogStatsD, OTLP metrics/traces, transforms, etc. — even though this binary only uses the OTLP log source and Datadog log forwarder. Fat LTO eliminates unused functions, but monomorphized generic instantiations (futures, hyper connections, tracing spans) still contribute code size.

Sub-module breakdown within `saluki_components`:

| Sub-module | Size | Description |
|---|---|---|
| `common::datadog` | 49 KiB | HTTP forwarder I/O loop, `TransactionForwarder` |
| `sources::otlp` | 34 KiB | OTLP gRPC/HTTP source `run` closure |
| `common::otlp` | 24 KiB | OTLP → Datadog log translation |
| `forwarders::datadog` | <1 KiB | Datadog forwarder wiring |
| `encoders::datadog` | <1 KiB | Datadog encoder wiring |
| monomorphized/inlined generics | 271 KiB | `tracing::Instrumented<T>::poll`, `hyper_util::Connection<I,S,E>::poll`, `figment` deserializers, etc. |

### Regex / DNS — 400 KiB (10.0% of .text)

| Crate | Size | Role |
|---|---|---|
| `regex_automata` | 182 KiB | DFA/NFA regex engine |
| `hickory_proto` | 86 KiB | DNS wire protocol |
| `regex_syntax` | 80 KiB | Regex parser |
| `hickory_resolver` | 57 KiB | Async DNS resolver |
| `aho_corasick` | 55 KiB | Multi-pattern string search |

This entire stack is a transitive dependency — the binary has no direct regex usage. It enters via:
- `hickory-resolver` (DNS resolution for forwarding to Datadog intake)
- `tracing-subscriber` (`EnvFilter` parses log directives with regex)

### Networking / HTTP — 265 KiB (6.6% of .text)

| Crate | Size | Role |
|---|---|---|
| `tonic` | 72 KiB | gRPC framework (OTLP ingestion) |
| `h2` | 63 KiB | HTTP/2 |
| `hyper` | 33 KiB | HTTP/1.1 (outbound to Datadog) |
| `http` | 33 KiB | HTTP type definitions |
| `hyper_util` | 24 KiB | Hyper connection utilities |
| `axum` + `axum_core` | 17 KiB | HTTP routing (OTLP HTTP endpoint) |
| `hyper_hickory` | 14 KiB | DNS connector for hyper |
| `hyper_http_proxy` | 6 KiB | Proxy support |
| `hyper_rustls` | 3 KiB | TLS connector for hyper |

### Async runtime — 87 KiB (2.2% of .text)

| Crate | Size | Role |
|---|---|---|
| `tokio` | 83 KiB | Async runtime (multi-thread scheduler) |
| `tokio_util` + `tokio_rustls` | 4 KiB | Tokio codec/TLS extensions |

### Compression — 88 KiB (2.2% of .text)

| Crate | Size | Role |
|---|---|---|
| `zstd_sys` | 79 KiB | zstd C library (compiled in) |
| `compression_codecs` + `async_compression` | 9 KiB | Rust compression wrappers |

### Tracing / Logging — 84 KiB (2.1% of .text)

| Crate | Size | Role |
|---|---|---|
| `tracing_subscriber` | 64 KiB | Log formatting, filtering (EnvFilter) |
| `tracing_appender` + `tracing_rolling_file` | 7 KiB | File-based log output |
| `tracing_core`, `tracing_log`, `tracing_serde` | 13 KiB | Core tracing primitives |

### Stdlib + Unattributed — 1.05 MiB (26.3% of .text)

| Entry | Size | Note |
|---|---|---|
| `std` | 546 KiB | Rust standard library |
| `[Unknown]` | 505 KiB | Code fat LTO inlined so aggressively that DWARF lost source attribution |

The `[Unknown]` bucket grows with fat LTO aggressiveness — when LLVM inlines across crate boundaries, the resulting machine code no longer maps cleanly to any original source location.

---

## Size Reduction Opportunities

| Opportunity | Estimated savings | Trade-off |
|---|---|---|
| Switch `rustls` TLS backend from `aws-lc` to `ring` | ~300–400 KiB | `ring` has a smaller C footprint; may affect FIPS compliance |
| Remove or feature-gate `hickory-resolver` + regex | ~400 KiB | Would require an alternative DNS resolver or static IP forwarding |
| Narrow `saluki_components` feature flags to OTLP+logs only | ~100–200 KiB | Requires feature-gating unused component modules |
| `panic = "abort"` in profile | ~35 KiB | Eliminates `__gcc_except_tab` / unwind tables; no unwinding on panic |
