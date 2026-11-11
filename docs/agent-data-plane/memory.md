# Memory Management

ADP takes a different approach to memory management than the Core Agent, which relies on Go's
garbage collector to reclaim memory and uses runtime GC tuning as backpressure. ADP instead uses
explicit, static memory accounting and a process-level limit.

## Configuration

| Config Key              | Description                                              |
| ----------------------- | -------------------------------------------------------- |
| `memory_limit`          | Process memory limit (bytes, or SI string, for example `512MB`) |
| `memory_slop_factor`    | Fraction of `memory_limit` held back as headroom         |
| `memory_mode`           | Bounds validation and global limiter behavior            |
| `enable_global_limiter` | Toggle dynamic RSS-based backpressure (default: `true`)  |

### `memory_mode`

| Value        | Bounds validation failure | Global limiter active?                |
| ------------ | ------------------------- | ------------------------------------- |
| `disabled`   | Skipped (default)         | No                                    |
| `permissive` | Non-fatal (logged)        | Yes, if `memory_limit` is set         |
| `strict`     | Fatal (ADP won't start)   | Yes, if `memory_limit` is set         |

### `memory_slop_factor`

`memory_slop_factor` is applied as a reduction to `memory_limit` before bounds validation.
A factor of `0.25` withholds 25 % of the limit. For example, a 100 MB limit becomes an
effective 75 MB budget. This accounts for allocations that ADP's static accounting doesn't
track.

## How it works

ADP enforces memory bounds through three complementary mechanisms:

- **Static bounds**: components declare their memory footprint at startup via `MemoryBounds`.
  ADP validates that declared bounds fit within the effective limit (`memory_limit` ×
  (1 − `memory_slop_factor`)). Under `strict` mode, ADP refuses to start if bounds are
  exceeded, preventing over-commitment before any traffic arrives. This covers string
  interning caches (context strings, tag strings, OTLP strings), aggregation state, and
  other bounded allocations.
- **Dynamic limiting**: a `MemoryLimiter` polls the process RSS every 250 ms and throttles
  ingestion tasks as usage approaches `memory_limit`. The slop factor doesn't apply here.
  Throttling starts above 85 % of `memory_limit` with a 1 ms delay per call, and the delay
  grows to 25 ms at 95 %. Once started, throttling continues until usage falls to 80 %, so
  usage that hovers around 85 % doesn't toggle it on and off. Disable with
  `enable_global_limiter: false`.
- **Structural backpressure**: bounded channels between components provide back-pressure
  independently of memory monitoring.

## Observing throttling

ADP logs a warning when the memory limiter starts throttling and an info message when it
stops. Throttled calls include DogStatsD reads and OTLP requests. The `Memory Limiter` section
of the Agent status page shows whether the limiter is enabled and currently throttling, and how
much it has throttled since ADP started. These internal metrics report the same values:

| Metric                                            | Description                                    |
| ------------------------------------------------- | ---------------------------------------------- |
| `adp.memory_limiter.current_backoff_secs`         | Delay currently applied to each throttled call |
| `adp.memory_limiter.throttled_calls_total`        | Calls delayed by the memory limiter            |
| `adp.memory_limiter.throttled_nanoseconds_total`  | Total time that calls were delayed             |

If throttling is frequent, raise `memory_limit` or reduce the load on ADP.

## Comparison with Core Agent

The Core Agent exposes `dogstatsd_mem_based_rate_limiter.*` settings to apply backpressure when
the Go process approaches its memory limit. Those settings work by manipulating Go's garbage
collector (`debug.SetGCPercent`, `debug.FreeOSMemory`), allocating a large heap ballast to
adjust GC heuristics, and blocking goroutines to slow packet ingestion. None of these mechanisms
have an equivalent in Rust, and ADP does not use a Go runtime. All 11
`dogstatsd_mem_based_rate_limiter.*` keys are ignored by ADP.
