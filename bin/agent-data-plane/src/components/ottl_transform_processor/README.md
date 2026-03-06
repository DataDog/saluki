# OTTL transform processor

The OTTL transform processor is a Saluki data-plane component that mutates span attributes using user-defined OTTL (OpenTelemetry Transformation Language) statements. It is intended to align with the behavior and configuration style of the [OpenTelemetry Collector Contrib transformprocessor](https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/transformprocessor). This document describes what the component does, how it compares to that reference implementation, and how to configure it.

## How it works

- The component runs as a **synchronous transform** in the pipeline: it receives an event buffer (e.g. traces) and mutates it in place.
- For each trace, it evaluates a list of OTTL **statements** against every span. Statements execute **sequentially**: later statements can read values set by earlier ones.
- Currently the only supported editor function is **`set`**, which assigns a value to a span attribute. Statements may include an optional **`where`** clause; the editor runs only when the condition matches (or when the `where` clause is absent).
- `attributes` (span-level) is **read-write**: `set` can create, overwrite, or remove attributes. `resource.attributes` (trace resource tags) is **read-only**: it can be used in `where` conditions but cannot be written to.
- Setting an attribute to `Nil` (e.g. referencing a non-existent attribute) **removes** the key from the span.
- Non-string values (integers, floats, booleans) are converted to their string representation when stored in span metadata.
- When a statement fails to execute (e.g. type error, unsupported path), behavior is controlled by **`error_mode`**: `ignore` (log and continue), `silent` (continue without logging), or `propagate` (stop processing further statements for the span, and log an error).

## Comparison with the OpenTelemetry transformprocessor

The OpenTelemetry Collector Contrib [transformprocessor](https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/transformprocessor) supports multiple telemetry types, contexts, and a rich set of editor/converter functions. The Saluki OTTL transform processor aims to follow the same configuration shape and semantics where applicable.

| Aspect | OpenTelemetry transformprocessor | Saluki OTTL transform processor |
|--------|----------------------------------|----------------------------------|
| **Traces — span statements** | Supported (`context: span`) | **Supported** (via `trace_statements`) |
| **Traces — span event statements** | Supported (`context: spanevent`) | **Not supported** |
| **Metrics** | Supported (`metric_statements`) | **Not supported** |
| **Logs** | Supported (`log_statements`) | **Not supported** |
| **Editor functions** | Full set (`set`, `delete_key`, `truncate_all`, `replace_match`, etc.) | **`set` only** |
| **Converter functions** | Full set (`Concat`, `Split`, `Int`, `IsMatch`, etc.) | **Not supported** |
| **`error_mode`** | `ignore`, `silent`, `propagate` (default: `propagate`) | **Supported** (same options and default) |
| **Statement execution order** | Sequential, each statement sees prior mutations | **Same** |
| **`where` clauses** | Supported | **Supported** |
| **OTTL paths (span context)** | Full [Span](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/pkg/ottl/contexts/ottlspan/README.md) context (e.g. `name`, `attributes`, `resource.attributes`, timestamps, status, etc.) | **Subset**: `attributes` (read-write), `resource.attributes` (read-only). Other Span fields (e.g. `name`, `start_time`, `end_time`, `status`) are **not** exposed yet. |
| **Config structure** | Nested (`context` + `statements` list) | Flat list of statement strings under `trace_statements` |

In short: **supported** today are span-level transformations via `trace_statements`, the `set` editor function, `where` clauses, the same `error_mode` behavior, and sequential execution semantics. **Not supported** are span events, metrics, logs, editor functions other than `set`, converter functions, and the full Span OTTL context (only `attributes` and `resource.attributes` are available).

## Configuration

Configuration is read from the data-plane generic configuration. The OTTL transform block uses a simplified version of the OpenTelemetry transformprocessor YAML structure.

### Configuration key

The component reads the transform config from the **`ottl_transform_config`** key at the top level of the data-plane configuration (e.g. in a Saluki or ADP config file).

### Structure

- **`error_mode`** (optional): `ignore` | `silent` | `propagate`. Default: `propagate`. Controls what happens when an OTTL statement throws an error (see [How it works](#how-it-works)).
- **`trace_statements`** (optional): list of OTTL statement strings. Each statement is an editor call (currently only `set`) with an optional `where` clause. Statements execute in order against every span.

### Example

```yaml
ottl_transform_config:
  error_mode: ignore
  trace_statements:
    - 'set(attributes["container.name"], "app_container_1")'
    - 'set(attributes["env"], "production") where resource.attributes["env"] == "staging"'
```

The `set` function takes two arguments: a target path and a value. The value can be:

- A **string literal**: `"value"`
- An **integer literal**: `42`
- A **float literal**: `3.14`
- A **boolean literal**: `true` / `false`
- An **attribute reference**: `attributes["other_key"]` or `resource.attributes["key"]`

Only **`attributes`** is a valid write target. **`resource.attributes`** is read-only and can be used in values and `where` conditions. For example:

- `set(attributes["key"], "value")` — set a span attribute to a string literal.
- `set(attributes["dst"], attributes["src"])` — copy one span attribute to another.
- `set(attributes["x"], "v") where resource.attributes["host.name"] == "localhost"` — conditionally set based on a resource attribute.

If `ottl_transform_config` is omitted, no transformation is applied (all spans pass through unchanged).

## References

- [OpenTelemetry Collector Contrib — transformprocessor](https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/release/v0.144.x/processor/transformprocessor)
- [OTTL — OpenTelemetry Transformation Language](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/pkg/ottl/README.md)
- [OTTL Span context](https://github.com/open-telemetry/opentelemetry-collector-contrib/blob/main/pkg/ottl/contexts/ottlspan/README.md) (reference for full Span field set; only a subset is implemented in this component)
