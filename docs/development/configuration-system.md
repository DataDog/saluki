# Configuration system

This page explains the architecture of the configuration system and inventory, and the workflows for changing them.
Read it before working on `lib/agent-data-plane-config*`, `lib/datadog-agent/config*`, `schema_overlay.yaml`,
`SalukiConfiguration`, `DatadogConfiguration`, or typed configuration migrations.

Saluki configures its runtime through a typed boundary. An alternate method of reading configuration,
`GenericConfiguration`, exists in `saluki-config` for external clients who may want a raw map view, but
`agent-data-plane` doesn't use it. Don't delete it as dead code: it's library code that other projects may depend on.

Paths and type names can move. If you find this page out of date, update it in the same change.

## Why this system exists

`GenericConfiguration` previously leaked source-language details into components, such as serde names, aliases,
parsing, and defaults. The typed system places a translation boundary between configuration sources and runtime code:

```text
 Datadog schema config ──> DatadogConfiguration ──> witness drive ──┐
                                                                    │
 Saluki-only config ───────────────> SalukiOnly ───────> seed ──────┤
                                                                    v
                                                SalukiConfiguration
                                                { control, shared, domains }
                                                                    │
                                           typed slices and Live<T> │
                                                                    v
                                                         runtime components
```

A generated witness trait creates a compile-time obligation to translate every supported Datadog key into
`SalukiConfiguration`.

## Architecture map

| Responsibility | Canonical location |
|---|---|
| Source-agnostic typed model | `lib/agent-data-plane-config/` |
| `Live<T>`, a live view of a shared value, and the channel that publishes it | `lib/saluki-common/src/sync/live.rs` |
| Loading, translation, authority, updates, compatibility | `lib/agent-data-plane-config-system/` |
| Datadog source model, witness, classifier, environment decoding | `lib/datadog-agent/config/` |
| Hand-edited ADP support inventory for Datadog schema keys | `lib/datadog-agent/config/schema/schema_overlay.yaml` |
| Vendored Datadog JSON schema, written in YAML | `lib/datadog-agent/config/schema/core/` |
| Overlay types, validation, `SALUKI_KEYS`, smoke-test metadata | `lib/datadog-agent/config-overlay-model/` |
| Configuration registry, `run_config_smoke_tests`, documentation generation | `lib/datadog-agent/config-testing/` |
| Raw map and loader: a general-purpose library, unused by ADP | `lib/saluki-config/` |
| Hand-written Datadog witness implementation | `lib/agent-data-plane-config-system/src/translators/datadog_translator.rs` |
| Saluki-only source model and `seed` | `lib/agent-data-plane-config-system/src/saluki_only.rs` |
| Saluki-only defaults | `lib/agent-data-plane-config/src/defaults.rs` |
| Runtime loading and authority selection | `lib/agent-data-plane-config-system/src/loaded.rs` |
| Translation gate and update loop | `lib/agent-data-plane-config-system/src/system.rs` |
| Datadog environment reader and its Figment provider | `lib/datadog-agent/config/src/env_reader.rs`, `env_provider.rs` |
| Saluki-only environment reader (by convention, with no table) | `lib/agent-data-plane-config-system/src/saluki_env_overlay.rs` |
| Merged sources with per-value provenance | `lib/agent-data-plane-config-system/src/source.rs` |

Use `ConfigValue<T>` in `SalukiConfiguration` when a default must be detectable, as `Provenance::Default` versus
`Provenance::Explicit`. See `dd_url` and `site` for examples.

### Dependency boundaries

The intended end state is:

- `agent-data-plane-config` depends on neither the raw map nor the Datadog source model.
- `agent-data-plane-config-system` bridges sources to the model and constructs no components.
- `saluki-components` define their own input arguments and structs.
- `bin/agent-data-plane` hands each component what it requires.

### The `saluki-components` dependency

Components **SHOULD NOT** accept model types from `agent-data-plane-config`. The binary supplies values through
fields or argument structs defined by the component. For live values, use `Live<T>` from `saluki-common`.

Existing dependencies remain through model slices (`SharedConfiguration` and `domains::*`) and `defaults` constants.
These are exceptions, not patterns for new migrations. `ConfigValue<T>` occurs within those slices; model methods such
as `Endpoints::primary_endpoint` resolve provenance and return effective values.

## Datadog versus Saluki keys

A key in the Datadog schema is a Datadog key; a key absent from the overlay is Saluki-only. Names are no help:
`data_plane.foo` can belong to either class.

## Datadog inventory and generated code

The vendored schema and overlay have different jobs:

- `schema/core/*.yaml` defines Datadog keys. It **MUST** stay a pristine upstream copy, never edited by hand. Raise
  any proposed or existing hand edits with the change's author.
- `schema_overlay.yaml` classifies every schema leaf for ADP. ADP defines its shape.

Code generated in-tree from the schema and overlay carries `@generated` and `DO NOT EDIT` warnings. Regenerate it with:

```shell
make build-schema-overlay
```

### Support classifications

Under `inventory`, support is `full`, `partial`, `none`, or `unknown`. `excluded` is a separate section:

- `full`: ADP reads the key and matches the Agent's behavior.
- `partial`: ADP reads the key, but differs in some cases. Describe those differences.
- `none`: ADP doesn't support the key. Choose a severity based on the effect on users, and state whether support is
  planned. Link an issue when work should be tracked.
- `unknown`: compatibility hasn't been established. Record what still needs investigation.
- `excluded`: ADP silently ignores the key and omits it from compatibility documentation. Use this for features ADP
  doesn't currently support, not to hide gaps that need tracking.

An investigation toward intended support is `planned: true`. A documentation-only issue doesn't mean support is
planned.

Date exclusion notes and name the feature: `YYYY-MM-DD: ADP does not currently support <feature>.` Avoid generic
reasons such as "Core Agent-only" or claims that support can never be added.

## Environment variables and key shape

The Datadog Agent doesn't derive a variable's name from its key path: `DD_PROXY_HTTP` reaches `proxy.http`, while
`DD_DOGSTATSD_PORT` reaches the flat `dogstatsd_port`. No separator convention reproduces this. Datadog keys read the
environment through the generated tables, and Saluki-only keys are derived through a serde workaround. The environment
is read by `apply_datadog_env` and by the Saluki-only reader.

**A modeled key arrives in the Agent's canonical shape.** A source model **MUST** read that shape: don't add a
`#[serde(rename)]` or `#[serde(alias)]`. Unmodeled environment variables aren't read.

## Scalar leaf coercion

`DatadogConfiguration` scalar leaves accept what the Agent's permissive casting accepts. Code generation attaches a
`cast_de.rs` coercion to each schema type. For example, `dogstatsd_port: "8125.0"` is valid and coerced to the integer
`8125`, and `1` or `"T"` is coerced to a `bool`.

## Documented enum settings

A `string` setting whose schema documentation names a closed set of values becomes an enum in `agent-data-plane-config`
with `#[default]` and `FromStr`. Match the Agent's behavior: warn and fall back, or record a `TranslateError`. Serialize
using the `FromStr` spelling.

## Saluki-only values

Values absent from the Datadog schema reach `SalukiConfiguration` through the `SalukiOnly` source model. Its field
hierarchy matches the source path exactly: don't add serde aliases.

A `SalukiOnly` field and a `DatadogConfiguration` field that map to the same destination is a bug. Keep the
authoritative Datadog path and delete the duplicate Saluki-only path.

## Defaults

Defaults belong in the configuration layer, and exactly one layer owns each default:

| Source class | Model type | Default owner | Translation behavior |
|---|---|---|---|
| Saluki-only | `Option<T>` | No default | `seed` preserves `None` |
| Saluki-only | `T` | `agent-data-plane-config/src/defaults.rs` | `seed` assigns the resolved value |
| Witnessed | `T` | Generated Datadog schema default | `drive` always writes it |
| Witnessed | `Option<T>` | No default | `drive` preserves `None` |

Define each Saluki-only default once in `lib/agent-data-plane-config/src/defaults.rs`. Source and model defaults must
reference that definition, rather than restating its value. If the component requires a value, model it as `T`: use
`Option<T>` only when absence is meaningful, not to defer a default.

Push source parsing, defaults, and input validation to the configuration boundary. Components keep only validation that
is business logic.

## Merged sources and the raw view

`SourceTree` is the merged configuration: every value an input supplied, each with the provenance of the input that
supplied it. `ConfigurationSystem` retains the tree it translated from, which is what the startup compatibility gate,
the privileged `/config` route, and the `runtime_config_dump.yaml` flare artifact read. Those readers need the keys
that the typed model doesn't carry, so they read the sources rather than the model.

### Runtime updates

Startup translation is strict. Live updates are translated against tentative state. On success, the update task
replaces the retained sources and publishes the typed model before releasing the sources lock. Separate reads of the
model and the sources can still observe different versions; they don't form a combined snapshot. On failure, the task
keeps the last-known-good model and leaves the sources as they were.

`ConfigurationSystem` holds the read-only `LiveSource` of a `live::channel`, and the update task owns its only
`LivePublisher`. Tests that need a changing value create their own channel and publish replacements through it.

## Workflows

### Add or change a witnessed key

1. Confirm its `schema_overlay.yaml` classification. If support becomes `full` or `partial`, provide the required
   metadata, and expect a new witness method after generation.
2. Add or refine its destination in `control`, `shared`, or `domains`, based on who consumes it.
3. Run `make build-schema-overlay`.
4. Implement the generated `consume_<key>` in `DatadogTranslator`, and find the right home for it in
   `SalukiConfiguration`.

An inventory key marked `none` or `unknown`, or a key under `excluded`, is absent from the witnessed model.

### Add or change a Saluki-only key

1. Verify that the key is absent from `schema_overlay.yaml`.
2. Add its destination to the correct `SalukiConfiguration` slice.
3. If it has a default, define it once in `agent-data-plane-config/src/defaults.rs`, and reference it from the model
   and source defaults.
4. Add the exact source hierarchy and a reliable parsing type to `SalukiOnly`. A **nested** key requires this: the
   Saluki-only environment reader discovers paths from `SalukiOnly`, so without a field, the key is unreachable from
   the environment.
5. Add one `seed` assignment to the destination.
6. Keep the legacy `SALUKI_KEYS` consistent with the source key, type, and default.

### Migrate a raw consumer

1. List every key the consumer reads, including serde fields, flattened structs, aliases, direct accessors, watches,
   and stored raw maps.
2. Record current parsing, validation, fallback, normalization, default, and update behavior before deleting source
   code.
3. Classify each key by schema membership, and check for a duplicate between witnessed and Saluki-only paths.
4. Search all consumers. Put a value in `domains` when one domain owns it, and in `shared` when multiple domains consume
   it. Components don't read `control`.
5. Add any missing model, witness, or seed path with the workflows above.
6. Delete `from_configuration`, and pick its replacement by the shape of the `*Configuration` struct:
   - Fewer than four fields: `new(...)` taking concrete values.
   - Four or more fields: make them `pub` and let the caller build the struct by field, with no constructor. Avoid
     introducing a new arguments or settings struct, and don't implement `Default` or `Deserialize` on the
     configuration struct.
   - Runtime wiring the caller can't read out of configuration: have the caller create it and assign what the
     component needs to a `pub` field. See `aggregate_context_snapshot_channel` and
     `AggregateConfiguration::context_snapshot_receiver`.
   - Values that change at runtime: pass a narrow `Live<T>`, and rebuild the reactive state after `changed()`. Values
     that must change together belong in one `Live<T>`: separate views advance independently.
7. Remove source serde, Datadog key names, raw-map access, key watches, parsing, configuration defaults, and code made
   unused by the cutover. `#[allow(dead_code)]` isn't an acceptable way to keep migration residue.
8. Update topology call sites and tests. Preserve behavior tests by using typed inputs, and remove tests only when they
   tested legacy deserialization and nothing else.
9. Remove the component's `run_config_smoke_tests` invocation once it no longer deserializes from
   `GenericConfiguration`. Replace migrated structs in `used_by` with `TypedConfigSystem` in `schema_overlay.yaml`, or
   with the string `TYPED_CONFIG_SYSTEM` in `SALUKI_KEYS`.
10. Test higher-risk cutovers with correctness or integration tests that exercise the affected configurations.

A cutover should be behaviorally transparent. If the old behavior conflicts with the source schema or the typed
system's invariants, surface the conflict rather than silently choosing one. Don't leave deserialization artifacts
behind after a cutover: for example, drop `derive(Deserialize)`, and collapse a held `Option<SomeType>` to a held
`SomeType` where possible.

### Update the vendored schema

Use the `config-schema-update` skill, which walks through comparing, vendoring, and classifying a new upstream schema.

## Review checklist

For any configuration change, check the risks:

- **Reachability**: every formerly configurable value still has a path from source to model to consumer.
- **Defaults**: the effective default is unchanged, and is stated once in the configuration layer.
- **Input handling**: parsing, coercion, normalization, and validation moved to the configuration layer.
- **Model boundary**: components receive only the domain and shared slices they need.
- **Dynamic behavior**: startup and update paths construct the same reactive state from typed input.
- **Test preservation**: deleting legacy deserialization coverage didn't delete behavioral coverage.

## Check your work

The `Makefile` is authoritative:

```shell
make build-schema-overlay       # regenerate code
make fmt                        # use this, not cargo fmt
cargo check --workspace --tests # quick compilation check
make check-all                  # required to pass CI
```
