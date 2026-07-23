---
name: config-system
description: >
  Architecture and working rules for Saluki's typed configuration system and Datadog configuration
  inventory. Read this when working with lib/agent-data-plane-config*, lib/datadog-agent/config*,
  GenericConfiguration, schema_overlay.yaml, SalukiConfiguration, DatadogConfiguration, config
  loading or updates, or a component's configuration inputs.
disable-model-invocation: false
---
# /config-system

Saluki is replacing direct reads from the raw `GenericConfiguration` map with a typed configuration
boundary. This skill explains the architecture, the transitional state, and the workflows for
changing configuration without losing defaults, validation, reachability, or dynamic behavior.

## Why this system exists

`GenericConfiguration` leaks source-language details into components such as serde names, aliases,
parsing, and defaults. String-keyed reads hide dependencies from the compiler.

The typed system places a translation boundary between configuration sources and runtime code:

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

A generated witness trait creates a compile-time obligation to translate every supported Datadog key
into `SalukiConfiguration`.

## Architecture map

| Responsibility                                                  | Canonical location                                                         |
|-----------------------------------------------------------------|----------------------------------------------------------------------------|
| Source-agnostic typed model and `Live<T>`                       | `lib/agent-data-plane-config/`                                             |
| Loading, translation, authority, updates, compatibility         | `lib/agent-data-plane-config-system/`                                      |
| Datadog source model, witness, classifier, environment decoding | `lib/datadog-agent/config/`                                                |
| Hand-edited Datadog support inventory                           | `lib/datadog-agent/config/schema/schema_overlay.yaml`                      |
| Vendored Datadog JSON-schema (written in YAML)                  | `lib/datadog-agent/config/schema/core/`                                    |
| Overlay types, validation, schema generation                    | `lib/datadog-agent/config-overlay-model/`                                  |
| Legacy test registry, legacy smoke test support, doc gen        | `lib/datadog-agent/config-testing/`                                        |
| Hand-written Datadog witness implementation                     | `lib/agent-data-plane-config-system/src/translators/datadog_translator.rs` |
| Saluki-only source model and `seed`                             | `lib/agent-data-plane-config-system/src/saluki_only.rs`                    |
| Runtime loading and authority selection                         | `lib/agent-data-plane-config-system/src/loaded.rs`                         |
| Translation gate and update loop                                | `lib/agent-data-plane-config-system/src/system.rs`                         |

Paths and type names can move. Notify the user when this skill needs an update.

### Architectural dependency boundaries

The intended end state is:

- `agent-data-plane-config` depends on neither the raw map nor the Datadog source model.
- `agent-data-plane-config-system` bridges sources to the model and constructs no components.
- Components and runtime code do not access `GenericConfiguration` in the end state.
- `saluki-components` accepts domain/shared slices and `Live<T>`.
- `bin/agent-data-plane` hands each component only its required slices.

Dedicated migration PRs remove the temporary violations component by component.

## Datadog vs. Saluki keys

One rule determines a key's source class: whether it exists in the Datadog schema. The build
enforces that `schema_overlay.yaml` lists every schema leaf exactly once, so the overlay is a
convenient membership index. When resolving schema-update drift, inspect `schema/core/*.yaml`
directly.

A key present in the overlay is a Datadog key; an absent key is Saluki-only. Names do not decide:
`data_plane.foo` can belong to either class.

## Datadog inventory and generated code

The vendored schema and overlay have different jobs:

- `schema/core/*.yaml` defines the Datadog keys as JSON Schema rendered in YAML.
- `schema_overlay.yaml` classifies every schema leaf for ADP. Its shape is defined by ADP.

Under `inventory`, support is `full|partial|none|unknown`; `excluded` is a separate section. ADP
adds metadata used by tests and generated documentation.

Code is generated in-tree from the schema and overlay bearing `@generated` / `DO NOT EDIT` warnings.
Use the following command to regenerate it:

```bash
make build-schema-overlay
```

## Saluki-only values

Values absent from the Datadog schema reach `SalukiConfiguration` through the `SalukiOnly` source
model. Its field hierarchy matches the source path exactly; do not add serde aliases.

A `SalukiOnly` field and a `DatadogConfiguration` field mapping to the same destination is a bug.
Keep the authoritative Datadog path and delete the duplicate Saluki-only path.

## Defaults

**Defaults belong in the configuration layer**

Exactly one layer owns each default:

| Source class | Model type  | Default owner                         | Translation behavior             |
|--------------|-------------|---------------------------------------|----------------------------------|
| Saluki-only  | `Option<T>` | No default                            | `seed` preserves `None`          |
| Saluki-only  | `T`         | One declaration beside the model type | `seed` assigns configured values |
| Witnessed    | `T`         | Generated Datadog schema default      | `drive` always writes it         |
| Witnessed    | `Option<T>` | No default                            | `drive` preserves `None`         |

For a Saluki-only default, use `#[serde(default = "...")]` with a nearby constant or function. If
the component requires a value, model `T`; use `Option<T>` only when absence is meaningful to the
component, not to defer its default.

Push source parsing, defaults, and input validation to the configuration boundary. Components keep
only validation that is truly business logic.

## The transitional state

The migration proceeds in isolated changes, largely component by component. At any given commit, the
repository can contain:

- bootstrap and CLI paths that still use `GenericConfiguration` before a `ConfigurationSystem`
  exists;
- runtime topology that carries both `ConfigurationSystem` and its `raw_map()` compatibility view;
- components already built from typed slices; and
- components that still deserialize directly from the raw map.

Prefer typed config; do not introduce new component uses of `GenericConfiguration`.

### Runtime updates

Startup translation is strict. Live updates are translated against tentative state: success
atomically replaces the typed model; failure retains the last-known-good model. Every update still
reaches the compatibility map, so typed and raw views can diverge during migration.

## Workflows

### Add or change a witnessed key

1. Confirm its `schema_overlay.yaml` classification. If support becomes `full` or `partial`, provide
   the required metadata and expect a new witness method after generation.
2. Add or refine its destination in `control`, `shared`, or `domains` based on who consumes it.
3. Run `make build-schema-overlay`.
4. Implement the generated `consume_<key>` in `DatadogTranslator`. Find the right home for it in
   `SalukiConfiguration`.

An inventory key marked `none` or `unknown`, or a key under `excluded`, is deliberately absent from
the witnessed model.

### Add or change a Saluki-only key

1. Verify that the key is absent from `schema_overlay.yaml`.
2. Add its destination to the correct `SalukiConfiguration` slice.
3. Add the exact source hierarchy and a reliable parsing type to `SalukiOnly`.
4. Add one `seed` assignment to the destination.
5. (legacy): Keep `SALUKI_KEYS` consistent with the source key, type, and default.

### Migrate a raw consumer

1. List every key the consumer reads, including serde fields, flattened structs, aliases, direct
   accessors, watches, and stored raw maps.
2. Record current parsing, validation, fallback, normalization, default, and update behavior before
   deleting source code.
3. Classify each key by schema membership and check for a witnessed/Saluki-only duplicate.
4. Search all consumers. Put a value in `domains` when one domain owns it and in `shared` when
   multiple domains consume it. Components do not read `control`.
5. Add any missing model, witness, or seed path with the workflows above.
6. Change static construction to accept borrowed typed slices. For dynamic behavior, pass a narrow
   `Live<T>` and rebuild the reactive state after `changed()`.
7. Remove source serde, Datadog key names, raw-map access, key watches, parsing, and configuration
   defaults from the component.
8. Update topology call sites and tests. Preserve behavior tests using typed inputs; remove tests
   only when they tested legacy deserialization and nothing else.
   - Do *not* rename `from_configuration`. Just change its signature to take typed configuration.
9. Remove the component's `run_config_smoke_tests` invocation once it no longer deserializes from
   `GenericConfiguration`. Replace migrated structs in the `used_by` field with
   `TYPED_CONFIG_SYSTEM`.
10. Higher risk cutovers should be tested with correctness or integration tests that exercise the
    affected configurations.

A cutover should be behaviorally transparent. If the old behavior conflicts with the source schema
or typed-system invariants, surface the conflict rather than silently choosing one.

## Review checklist

For any configuration change, check the following risks:

- **Reachability:** every formerly configurable value still has a source-to-model-to-consumer path.
- **Defaults:** the effective default is unchanged and is stated once in the config layer.
- **Input handling:** parsing, coercion, normalization, and validation moved to the config layer.
- **Model boundary:** components receive only the domain/shared slices they need.
- **Dynamic behavior:** startup and update paths construct the same reactive state from typed input.
- **Test preservation:** deleting legacy deserialization coverage did not delete behavioral
  coverage.

## Check your work

Use the repository's Make targets; the Makefile is authoritative:

```bash
make build-schema-overlay       # regenerate code
make fmt                        # use this, not cargo fmt
cargo check --workspace --tests # quick compilation check
make check-all                  # required to pass CI
```
