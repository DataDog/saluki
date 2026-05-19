# Find AdpImpl Configs

This is a substep of the `/confkey` skill. Your job is to discover the configuration keys (ConfKeys)
that are used by agent-data-plane and its components at runtime.

## Input

- `{{saluki}}` = the path to the root of the Saluki codebase repository
- `{{tmp}}` = your output directory

## System Overview

Configuration values originate from `datadog.yaml` and/or `DD_`-prefixed env vars, stored in a map
structure accessed via `GenericConfiguration` (defined in `lib/saluki-config`).

Environment variable names are split on `__` (double underscore) to create nested objects. For
example, `DD_FOO__BAR=baz` becomes `{ "foo": { "bar": "baz" } }`. This is why some components use
manual `try_get_typed("dotted.key")` calls instead of serde structs -- it avoids requiring users to
type awkward env var names like `DD_DATA_PLANE__OTLP__ENABLED`.

## Step 1: Discover the Config API Surface

Read `GenericConfiguration` in `lib/saluki-config/src/lib.rs` and build a complete list of every
public method that takes a config key string argument (e.g. `get_typed`, `try_get_typed`,
`get_typed_or_default`, `as_typed`, `watch_for_updates`).

Also search for wrapper functions that delegate to `GenericConfiguration` -- these may be the actual
call sites in component code.

## Step 2: Search for Configuration Keys

There are two families of patterns. You must search for both.

### Pattern A: Serde rename on Deserialize structs

Config structs derive `Deserialize` and use `#[serde(rename = "...")]` to map fields to config key
names. The struct is loaded as a whole via `config.as_typed::<T>()`.

```rust
#[derive(Deserialize)]
pub struct DogStatsDConfiguration {
    #[serde(rename = "dogstatsd_port", default = "default_port")]
    port: u16,

    #[serde(rename = "dogstatsd_buffer_size", default = "default_buffer_size")]
    buffer_size: usize,

    // flatten embeds another struct's keys into this one
    #[serde(flatten)]
    origin_enrichment: OriginEnrichmentConfiguration,
}
```

**The config key is the string inside `rename = "..."`.**

If a field has NO `rename` attribute, the Rust field name itself is the key. For example
`pub api_key: String` without a rename means the key is `api_key`.

**Important:** `#[serde(flatten)]` means a sub-struct's fields are inlined. You must follow these to
find all keys -- they won't appear in the outer struct.

**Search:** Grep all `.rs` files under `lib/` and `bin/agent-data-plane/` for `serde(rename`. For
each match, extract the string literal as the ConfKey and record file:line. Also check `Deserialize`
structs for fields WITHOUT `rename` -- the Rust field name is the key in those cases.

### Pattern B: Manual key queries

Some config structs don't derive `Deserialize`. Instead, their `from_configuration()` method calls
accessor functions on `GenericConfiguration` with string literal key names.

```rust
// Examples using the methods known at time of writing:
config.get_typed("api_key")?
config.try_get_typed("data_plane.enabled")?.unwrap_or(false)
config.get_typed_or_default("log_level")
```

**The config key is the string literal passed to the function.**

Dotted keys like `data_plane.otlp.enabled` represent YAML nesting:
```yaml
data_plane:
  otlp:
    enabled: true
```

**Search:** Using the full method list you discovered in Step 1, grep all `.rs` files under `lib/`
and `bin/agent-data-plane/` for each method name followed by `("`. For each match, extract the
string literal as the ConfKey and record file:line as the location.

## Step 3: Validate Completeness

- Grep for any remaining string literals passed to `GenericConfiguration` methods not already
  captured.
- Spot-check 2-3 component `mod.rs` or `config.rs` files for patterns you haven't accounted for.
- If you find a new pattern, search for it comprehensively.

### What NOT to include

- Test files (`#[cfg(test)]` modules, files in `tests/` directories)
- Keys that appear only in comments or doc strings
- Internal framework keys that are not user-facing configuration

## Output

Write to `{{tmp}}/adpimpl-config-keys.csv`. Each line is a quoted ConfKey and its file:line
location, relative to `{{saluki}}`:

```csv
"dogstatsd_buffer_size","lib/saluki-components/src/sources/dogstatsd/mod.rs:157"
"dogstatsd_port","lib/saluki-components/src/sources/dogstatsd/mod.rs:175"
"data_plane.enabled","bin/agent-data-plane/src/config.rs:35"
```

If the same ConfKey appears in multiple locations, include the most authoritative one -- prefer the
declaration site (serde rename or struct definition) over a secondary read site.
