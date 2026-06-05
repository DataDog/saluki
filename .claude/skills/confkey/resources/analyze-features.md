# Analyze Features

Substep of `/confkey`. You receive a batch of config keys and perform clean-room analysis of each
one for DogStatsD feature parity between RefImpl (datadog-agent) and AdpImpl (agent-data-plane /
Saluki).

Search BOTH codebases for every key regardless of what the supervising agent tells you.

## Input

- `{{datadog-agent}}` = path to datadog-agent repo
- `{{saluki}}` = path to Saluki repo
- `{{outdir}}/{{outfile}}` = your output filepath
- A batch of config keys (provided by the supervising agent)

## Per-Key Analysis

### Tracing a key in RefImpl (`{{datadog-agent}}`)

**Registration** — DogStatsD keys are in the `dogstatsd()` function in
`pkg/config/setup/common_settings.go`; other subsystem keys are in sibling files. Registration
forms (first string arg is the key):

```go
config.BindEnvAndSetDefault("key_name", defaultValue)
config.SetDefault("key_name", defaultValue)
config.BindEnv("key_name")
config.ParseEnvSplitComma("key_name")
config.ParseEnvSplitSpace("key_name")
config.ParseEnvJSON("key_name", []interface{}{})
config.ParseEnvAsStringSlice("key_name", ...)
config.ParseEnvAsMapStringInterface("key_name", ...)
```

Check `pkg/config/model/types.go` (`Setup` interface) for the authoritative method list.

**Read sites** — search `comp/dogstatsd/` and beyond. Accessor methods:

```go
.GetString  .GetBool     .GetInt      .GetInt32    .GetInt64
.GetFloat64 .GetFloat64Slice .GetDuration .GetStringSlice
.GetStringMap .GetStringMapString .GetStringMapStringSlice
```

**YAML** — `cmd/agent/dist/datadog.yaml` is the example config (large, mostly commented).
Nested keys flatten to dot-separated: `proxy.http`. Commented-out entries still count.

### Tracing a key in AdpImpl (`{{saluki}}`)

Config values come from `datadog.yaml` / `DD_`-prefixed env vars, stored in `GenericConfiguration`
(`lib/saluki-config/src/lib.rs`). Env vars split on `__` (double underscore) to produce nested
keys: `DD_FOO__BAR=baz` → `{ "foo": { "bar": "baz" } }`.

There are two lookup patterns — check both:

**Pattern A: serde Deserialize structs.** A struct derives `Deserialize` and is loaded as a whole
via `config.as_typed::<T>()`. The key is the `rename` string, or the Rust field name if there is no
`rename`:

```rust
#[derive(Deserialize)]
pub struct DogStatsDConfiguration {
    #[serde(rename = "dogstatsd_port", default = "default_port")]
    port: u16,                       // key: "dogstatsd_port"

    pub api_key: String,             // key: "api_key" (no rename → field name is the key)

    #[serde(flatten)]
    origin: OriginEnrichmentConfiguration,  // inline sub-struct — must follow and search it too
}
```

Search all `.rs` files under `lib/` and `bin/agent-data-plane/` for `serde(rename`. Also check
`Deserialize` structs for fields WITHOUT `rename`. Follow all `#[serde(flatten)]` attributes
recursively.

**Pattern B: manual key queries.** Some components call accessor methods directly:

```rust
config.get_typed("api_key")?
config.try_get_typed("data_plane.enabled")?.unwrap_or(false)
config.get_typed_or_default("log_level")
config.watch_for_updates("key", ...)
```

Dotted keys reflect YAML nesting (`data_plane.otlp.enabled` → `data_plane: otlp: enabled:`).

Search all `.rs` files for each accessor method name followed by `("`. Check `lib/saluki-config/src/lib.rs`
for any accessor methods not listed above.

### Determine Proposed Overlay Section

Map your findings to the overlay section the key should land in:

- **`supported`** — ADP reads and uses the key.
    - `support_level: full` when behavior matches the Agent (same semantics, same effective default).
    - `support_level: partial` when behavior differs; capture the divergence in `Discussion`.
- **`unsupported`** — Key is relevant to ADP's domain but not implemented. Propose a `severity`
  (low/medium/high) based on operational impact.
- **`investigate`** — Evidence is insufficient to classify confidently. Use when you cannot
  determine whether the key is implemented, or when you are unsure if it is in scope. Prefer this
  over `unsupported` when you cannot rule out that ADP silently handles it some other way.
- **`ignored`** — Key is architecturally outside ADP's scope (Go-GC-specific, Windows-only,
  handled by the core Agent tagger, etc.). The supervising agent will confirm with the user before
  writing `ignored`.
- **`saluki_keys.rs`** — ADP uses this key but it has no counterpart in `core_schema.yaml`
  (ADP-only extension). Flag this in `Discussion`; the key does not go in the overlay.

Commit to a section. Only use `investigate` when you genuinely cannot determine scope or
implementation status after thorough analysis.

### Write Outputs

**Description** (required, max 50 chars): Terse summary of what the key controls. Examples:
`UDP listen port`, `Tag cardinality for origin`, `Max cached DSD contexts`

**Discussion** (optional, null for most keys): Only for keys requiring `support_level: partial`,
`investigate`, or `saluki_keys.rs` placement, or for surprising omissions. Include code snippets
from both sides and explain user-visible impact. Keep focused.

## Output Format

JSON array, one object per key:

```json
[
  {
    "ConfKey": "dogstatsd_port",
    "ProposedSection": "supported",
    "SupportLevel": "full",
    "Severity": null,
    "Description": "UDP listen port",
    "Discussion": null
  },
  {
    "ConfKey": "dogstatsd_buffer_size",
    "ProposedSection": "supported",
    "SupportLevel": "partial",
    "Severity": null,
    "Description": "Receive buffer size (bytes)",
    "Discussion": "ADP default 8192, Agent default 4096. Both read the same key; the difference is..."
  },
  {
    "ConfKey": "some_missing_key",
    "ProposedSection": "unsupported",
    "SupportLevel": null,
    "Severity": "medium",
    "Description": "Controls X behaviour",
    "Discussion": null
  }
]
```

- `ProposedSection`: one of `supported`, `unsupported`, `investigate`, `ignored`, `saluki_keys.rs`
- `SupportLevel`: `full` or `partial` when `ProposedSection` is `supported`; `null` otherwise
- `Severity`: `low`, `medium`, or `high` when `ProposedSection` is `unsupported`; `null` otherwise
- `Description`: non-empty, max 50 chars
- `Discussion`: `null` or a prose string; required for `partial`, `investigate`, and `saluki_keys.rs`
