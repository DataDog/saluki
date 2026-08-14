---
name: config-system
description: >
  Architecture and working rules for Saluki's configuration system and inventory. Read this when
  working with lib/agent-data-plane-config*, lib/datadog-agent/config*, GenericConfiguration,
  schema_overlay.yaml, SalukiConfiguration, DatadogConfiguration, or typed config migrations.
disable-model-invocation: false
---
# /config-system

Saluki is replacing direct reads from the raw `GenericConfiguration` map with a typed configuration
boundary. This skill explains the architecture, transitional state, and workflows.

Paths and type names can move. Notify the user when this skill needs an update.

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
| Hand-edited ADP support inventory for Datadog schema keys.      | `lib/datadog-agent/config/schema/schema_overlay.yaml`                      |
| Vendored Datadog JSON-schema (written in YAML)                  | `lib/datadog-agent/config/schema/core/`                                    |
| Overlay types, validation, `SALUKI_KEYS`, smoke-test metadata   | `lib/datadog-agent/config-overlay-model/`                                  |
| Config registry, `run_config_smoke_tests`, doc gen              | `lib/datadog-agent/config-testing/`                                        |
| Raw map (`GenericConfiguration`) and the by-key view            | `lib/saluki-config/`                                                       |
| Hand-written Datadog witness implementation                     | `lib/agent-data-plane-config-system/src/translators/datadog_translator.rs` |
| Saluki-only source model and `seed`                             | `lib/agent-data-plane-config-system/src/saluki_only.rs`                    |
| Saluki-only defaults                                            | `lib/agent-data-plane-config/src/defaults.rs`                              |
| Runtime loading and authority selection                         | `lib/agent-data-plane-config-system/src/loaded.rs`                         |
| Translation gate and update loop                                | `lib/agent-data-plane-config-system/src/system.rs`                         |
| Datadog env reader plus its Figment provider                    | `lib/datadog-agent/config/src/env_reader.rs`, `env_provider.rs`            |
| Saluki-only env reader (convention, no table)                   | `lib/agent-data-plane-config-system/src/saluki_env_overlay.rs`             |
| Provider carrying both key classes                              | `lib/agent-data-plane-config-system/src/env_provider.rs`                   |

Use `ConfigValue<T>` in `SalukiConfiguration` when a default must be detectable:
`Provenance::Default` vs `Provenance::Explicit`. See `dd_url`, `site`.

### Architectural dependency boundaries

The intended end state is:

- `agent-data-plane-config` depends on neither the raw map nor the Datadog source model.
- `agent-data-plane-config-system` bridges sources to the model and constructs no components.
- Components and runtime code do not access `GenericConfiguration` in the end state.
- `saluki-components` define their own input arguments and structs.
- `bin/agent-data-plane` hands each component what it requires.

Dedicated migration PRs remove temporary violations component by component.

### `saluki-components` mistake

Initially we wanted `saluki-components` to depend on `agent-data-plane-config` but we changed our
mind. We cannot immediately end this dependency because of `Live<T>` and `ConfigValue<T>`. However,
most migrations do not require these types. Migrations that do not require these types should *not*
depend on types in `agent-data-plane-config` and should, instead, rely on the binary to copy data
into fields or an args struct that they present.

The end state deletes `agent-data-plane-config` from the `saluki-components` `Cargo.toml`. Minimize
the impact of that change when migrating components. Some migrations predate this design change; do
not follow them, they are temporary exceptions.

## Datadog vs. Saluki keys

A key in the Datadog schema is a Datadog key; a key absent from the overlay is Saluki-only. Names
are no help: `data_plane.foo` can belong to either class.

## Datadog inventory and generated code

The vendored schema and overlay have different jobs:

- `schema/core/*.yaml` defines Datadog keys as JSON Schema rendered in YAML.
- `schema_overlay.yaml` classifies every schema leaf for ADP. Its shape is defined by ADP.

Under `inventory`, support is `full|partial|none|unknown`; `excluded` is a separate section.

Code is generated in-tree from the schema and overlay bearing `@generated` / `DO NOT EDIT` warnings.
Use the following command to regenerate it:

```bash
make build-schema-overlay
```

## Environment variables and key shape

The Datadog Agent does not derive a variable's name from its key path: `DD_PROXY_HTTP` reaches
`proxy.http` while `DD_DOGSTATSD_PORT` reaches the flat `dogstatsd_port`. No separator convention
reproduces this. Datadog keys read the environment through the generated tables. Saluki-only keys
have no table: the name is the canonical path, upper-cased, underscore-joined, `DD_`-prefixed. Both
readers serve both paths: the typed path via `apply_datadog_env` plus the Saluki-only reader, and
the by-key path via `EnvironmentProvider`, a legacy Figment provider wrapping those readers.

**A modeled key arrives in the Agent's canonical shape.** A struct deserialized from
`GenericConfiguration` MUST read that shape. Do not add a `#[serde(rename)]` or `#[serde(alias)]`
and do not reintroduce a key-alias or environment-remapping table. A key that no model declares
still arrives flat from the `DD_` prefix-scanning provider, at lower precedence.

For deserialization paths, reserve `#[serde(flatten)]` for a struct that groups several *top-level*
Agent keys (for example, the forwarder's `forwarder_*` retry settings).

## Scalar leaf coercion

`DatadogConfiguration` scalar leaves accept what the Agent's permissive casting accepts. Codegen
attaches a `cast_de.rs` coercion per schema type. For example `dogstatsd_port: "8125.0"` is valid
and coerced to the int `8125`. `1` or `"T"` is coerced to a `bool`.

## Documented enum settings

A `string` setting whose schema documentation names a closed set of values becomes an enum in
`agent-data-plane-config` with `#[default]` and `FromStr`. Match Agent behavior; warn and fallback,
or record a `TranslateError`. Serialize using the `FromStr` spelling.

## Saluki-only values

Values absent from the Datadog schema reach `SalukiConfiguration` through the `SalukiOnly` source
model. Its field hierarchy matches the source path exactly; do not add serde aliases.

A `SalukiOnly` field and a `DatadogConfiguration` field mapping to the same destination is a bug.
Keep the authoritative Datadog path and delete the duplicate Saluki-only path.

## Defaults

**Defaults belong in the configuration layer**

Exactly one layer owns each default:

| Source class | Model type  | Default owner                             | Translation behavior              |
|--------------|-------------|-------------------------------------------|-----------------------------------|
| Saluki-only  | `Option<T>` | No default                                | `seed` preserves `None`           |
| Saluki-only  | `T`         | `agent-data-plane-config/src/defaults.rs` | `seed` assigns the resolved value |
| Witnessed    | `T`         | Generated Datadog schema default          | `drive` always writes it          |
| Witnessed    | `Option<T>` | No default                                | `drive` preserves `None`          |

Define each Saluki-only default once in `lib/agent-data-plane-config/src/defaults.rs`; source and
model defaults must reference that definition rather than restating its value. If the component
requires a value, model `T`; use `Option<T>` only when absence is meaningful, not to defer a
default.

Push source parsing, defaults, and input validation to the configuration boundary. Components keep
only validation that is business logic.

## The transitional state

The migration proceeds in isolated changes, largely component by component. At any commit, the
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

1. Verify the key is absent from `schema_overlay.yaml`.
2. Add its destination to the correct `SalukiConfiguration` slice.
3. If it has a default, define it once in `agent-data-plane-config/src/defaults.rs` and reference it
   from the model and source defaults.
4. Add the exact source hierarchy and a reliable parsing type to `SalukiOnly`. A **nested** key
   *requires* this even when its only consumer reads the by-key view: the reader discovers paths
   from `SalukiOnly`, so without a field the key is unreachable from the environment.
5. Add one `seed` assignment to the destination.
6. (legacy): Keep `SALUKI_KEYS` consistent with the source key, type, and default.

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
7. Remove source serde, Datadog key names, raw-map access, key watches, parsing, configuration
   defaults, and code made unused by the cutover. `#[allow(dead_code)]` is not an acceptable way to
   retain migration residue.
8. Update topology call sites and tests. Preserve behavior tests using typed inputs; remove tests
   only when they tested legacy deserialization and nothing else.
   - Do *not* rename `from_configuration`. Change its signature to take typed configuration.
9. Remove the component's `run_config_smoke_tests` invocation once it no longer deserializes from
   `GenericConfiguration`. Replace migrated structs in `used_by` with `TypedConfigSystem` in
   `schema_overlay.yaml`, or the string `TYPED_CONFIG_SYSTEM` in `SALUKI_KEYS`.
10. Higher risk cutovers should be tested with correctness or integration tests that exercise the
    affected configurations.

A cutover should be behaviorally transparent. If the old behavior conflicts with the source schema
or typed-system invariants, surface the conflict rather than silently choosing one. After cutover
artifacts of deserialization logic should not be left behind. For example, no `derive(Deserialize)`
and a held `Option<SomeType>` should collapse to a held `SomeType` if possible.

## Review checklist

For any configuration change, check the risks:

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
