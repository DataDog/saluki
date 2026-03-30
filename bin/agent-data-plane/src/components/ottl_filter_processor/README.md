# OTTL filter processor

The OTTL filter processor is a Saluki data-plane component that drops spans when user-defined OTTL (OpenTelemetry Transformation Language) conditions evaluate to true. It is intended to align with the behavior and configuration style of the [OpenTelemetry Collector Contrib filterprocessor](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/release/v0.144.x/processor/filterprocessor/README.md). This document describes what the component does, how it compares to that reference implementation, and how to configure it.

## How it works

- The component runs as a **synchronous transform** in the pipeline: it receives an event buffer (for example, traces) and mutates it in place.
- For each trace, it evaluates a list of OTTL **span** conditions. Conditions are combined with logical OR: if **any** condition evaluates to `true` for a span, that span is **dropped** (removed from the trace).
- Evaluation is **read-only**: the OTTL context exposes span attributes and resource attributes for reading only; no telemetry is modified, only filtered out.
- When a condition fails to evaluate (for example, type error, missing path), behavior is controlled by **`error_mode`**: `ignore` (log and continue), `silent` (continue without logging), or `propagate` (treat as match and drop the span, and log an error).

## Comparison with the OpenTelemetry filterprocessor

The OpenTelemetry Collector Contrib [filterprocessor](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/release/v0.144.x/processor/filterprocessor/README.md) supports multiple telemetry types and OTTL contexts. The Saluki OTTL filter processor aims to follow the same configuration shape and semantics where applicable.

| Aspect | OpenTelemetry filterprocessor | Saluki OTTL filter processor |
|--------|-------------------------------|------------------------------|
| **Traces — span conditions** | Supported (`traces.span`, [Span](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/pkg/ottl/contexts/ottlspan/README.md) context) | **Supported** (`traces.span`) |
| **Traces — span event conditions** | Supported (`traces.spanevent`) | **Not supported** |
| **Metrics** | Supported (`metrics.metric`, `metrics.datapoint`) | **Not supported** |
| **Logs** | Supported (`logs.log_record`) | **Not supported** |
| **Profiles** | Supported (`profiles.profile`) | **Not supported** |
| **`error_mode`** | `ignore`, `silent`, `propagate` (default: `propagate`) | **Supported** (same options and default) |
| **Condition semantics** | Any condition true → drop (logical OR) | **Same** |
| **OTTL paths (span context)** | Full [Span](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/pkg/ottl/contexts/ottlspan/README.md) context (for example, `name`, `attributes`, `resource.attributes`, timestamps, status, etc.) | **Subset**: `attributes`, `resource.attributes` only. Other Span fields (for example, `name`, `start_time`, `end_time`, `status`) are **not** exposed yet. |
| **OTTL functions** | Full converter and condition function set | Depends on the Rust OTTL library in use; not all OTel functions may be available. |

In short: **supported** today are span-level filtering via `traces.span`, the same `error_mode` behavior, and the same OR semantics. **Not supported** are span events, metrics, logs, profiles, and the full Span OTTL context (only `attributes` and `resource.attributes` are available).

## Configuration

Configuration is read from the data-plane generic configuration. The OTTL filter block uses the same YAML structure as the OpenTelemetry filterprocessor under its `ottl` key.

### Configuration key

The component reads the filter config from the **`ottl_filter_config`** key at the top level of the data-plane configuration (for example, in a Saluki or ADP config file). Other paths (such as `data_plane.otlp.filter` or a Collector-style `processors.filter/ottl`) may be supported in the future for compatibility.

### Structure

- **`error_mode`** (optional): `ignore` | `silent` | `propagate`. Default: `propagate`. Controls what happens when an OTTL condition throws an error (see [How it works](#how-it-works)).
- **`traces`** (optional): trace-level filter config.
  - **`traces.span`** (optional): list of OTTL condition strings. If any condition evaluates to `true` for a span, that span is dropped.

### Example

```yaml
ottl_filter_config:
  error_mode: ignore
  traces:
    span:
      - 'attributes["container.name"] == "app_container_1"'
      - 'resource.attributes["host.name"] == "localhost"'
```

Only **`attributes`** and **`resource.attributes`** are valid paths in conditions with the current implementation. For example:

- `attributes["container.name"] == "app_container_1"` — span-level attribute.
- `resource.attributes["host.name"] == "localhost"` — resource-level attribute.

Conditions can use OTTL boolean expressions (`and`, `or`, parentheses) as supported by the OTTL parser. If `ottl_filter_config` is omitted, no filtering is applied (all spans are kept).

## References

- [OpenTelemetry Collector Contrib — filterprocessor](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/release/v0.144.x/processor/filterprocessor/README.md)
- [OTTL — OpenTelemetry Transformation Language](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/pkg/ottl/README.md)
- [OTTL Span context](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/pkg/ottl/contexts/ottlspan/README.md) (reference for full Span field set; only a subset is implemented in this component)
