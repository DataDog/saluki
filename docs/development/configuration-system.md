# Configuration system

This page explains the architecture of the configuration system and inventory, and the workflows for changing them.
Read it before working on `lib/agent-data-plane-config*`, `lib/datadog-agent/config*`, `schema_overlay.yaml`,
`SalukiConfiguration`, `DatadogConfiguration`, or typed configuration migrations. Its
[review checklist](#review-checklist) also covers `lib/saluki-config` and `lib/datadog-agent/runtime`.

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
                      binary adapters: component values and Live<T> │
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
| Adapters from the model to component inputs | `bin/agent-data-plane/src/component_inputs/` |
| Loading, translation, authority, updates, compatibility | `lib/agent-data-plane-config-system/` |
| Datadog source model, witness, classifier, environment decoding | `lib/datadog-agent/config/` |
| Hand-edited ADP support inventory for Datadog schema keys | `lib/datadog-agent/config/schema/schema_overlay.yaml` |
| Vendored Datadog JSON schema, written in YAML | `lib/datadog-agent/config/schema/core/` |
| Overlay types, validation, `SALUKI_KEYS` | `lib/datadog-agent/config-overlay-model/` |
| Documentation generation | `lib/datadog-agent/config-docs/` |
| Raw map and loader: a general-purpose library, unused by ADP | `lib/saluki-config/` |
| Hand-written Datadog witness implementation | `lib/agent-data-plane-config-system/src/translators/datadog_translator.rs` |
| Saluki-only source model and `seed` | `lib/agent-data-plane-config-system/src/saluki_only.rs` |
| Saluki-only defaults | `lib/agent-data-plane-config/src/defaults.rs` |
| Runtime loading and authority selection | `lib/agent-data-plane-config-system/src/loaded.rs` |
| Translation gate and update loop | `lib/agent-data-plane-config-system/src/system.rs` |
| Datadog environment reader | `lib/datadog-agent/config/src/env_reader.rs` |
| Saluki-only environment reader (by convention, with no table) | `lib/agent-data-plane-config-system/src/saluki_env_overlay.rs` |
| Merged sources with per-value provenance | `lib/agent-data-plane-config-system/src/source.rs` |

Use `ConfigValue<T>` in `SalukiConfiguration` when a default must be detectable, as `Provenance::Default` versus
`Provenance::Explicit`. See `dd_url` and `site` for examples.

### Dependency boundaries

- `agent-data-plane-config` is a pure data model that does not depend on any other workspace crates.
- `agent-data-plane-config-system` bridges sources to the model and constructs no components.
- `saluki-components` and `bin/agent-data-plane/src/components/` never depend on `agent-data-plane-config`, not even
  in tests.
- The rest of `bin/agent-data-plane` adapts the model into the inputs each component defines.

### Component inputs

Components define their inputs: concrete values, component-owned types, or a `Live<T>` of a component-owned `T`.
They **MUST NOT** name model types, in production code or tests.

The binary converts the model with free functions in `bin/agent-data-plane/src/component_inputs/`, because the orphan
rule forbids `From` implementations between the two crates' types. An adapter copies fields and calls model methods
that resolve provenance, such as `Endpoints::primary_endpoint` and `Compression::effective_zstd_level`, so components
receive effective values and never see `ConfigValue<T>`. Adapters add no defaults, parsing, or validation. Test each
adapter with a distinct value in every field, so that a swapped field fails the test.

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
- `unknown`: compatibility hasn't been established. Record what still needs investigation. With a `severity`, the
  startup check applies the key to every pipeline.
- `excluded`: ADP silently ignores the key and omits it from compatibility documentation. Use this for a key whose
  feature doesn't affect what ADP does, such as logs processing, which ADP doesn't currently support. Don't use it to
  hide a gap that needs tracking: a key that ADP must honor, but doesn't, is `none`.

An investigation toward intended support is `planned: true`. A documentation-only issue doesn't mean support is
planned.

Date exclusion notes and name the feature: `YYYY-MM-DD: ADP does not currently support <feature>.` Avoid generic
reasons such as "Core Agent-only" or claims that support can never be added.

## Environment variables and key shape

The Datadog Agent doesn't derive a variable's name from its key path: `DD_PROXY_HTTP` reaches `proxy.http`, while
`DD_DOGSTATSD_PORT` reaches the flat `dogstatsd_port`. No separator convention reproduces this. Datadog keys read the
environment through the generated tables, and Saluki-only keys are derived through a serde workaround. The environment
is read by `apply_datadog_env_vars` and by the Saluki-only reader.

**A modeled key arrives in the Agent's canonical shape.** A source model **MUST** read that shape: don't add a
`#[serde(rename)]` or `#[serde(alias)]`. Unmodeled environment variables aren't read.

## Deprecated key names

A schema setting can list former names under `renamed_from`, each with the Agent version that deprecated it. ADP
reads former names as the Agent does:

- **Environment**: for a setting without explicit `env_vars`, each former name's derived variable is read before the
  current one, oldest deprecation first. `DD_STATSD_METRIC_BLOCKLIST` therefore beats `DD_METRIC_FILTERLIST`, and is
  decoded as `metric_filterlist`.
- **`datadog.yaml`**: a former name resolves like any other leaf, including its case and dotted spellings. Once the
  file is read, its value replaces the current name's and the former name is dropped, so the former name wins when
  both are written. A null former name clears the current one. `datadog_yaml.rs` documents the details.
- **Configuration stream**: an Agent older than the rename streams the former name as a separate setting.
  `overlay_agent_layer` in `system.rs` resolves it as that Agent does. A test fails if a renamed setting
  has no rule there.

Precedence between sources applies across names: a current name in the environment beats a former name in the file.
Former names never reach the sources that the compatibility check, `/config`, and the flare read. Once logging starts,
ADP logs a warning for each former name the file or environment used. A `no-env` setting has no former names, because
the Agent's codegen declares it with `SetDefault`.

Code generation emits the derived variables into `DATADOG_ENV_KEYS` and the former paths into
`datadog_renamed_keys`. It fails the build if a former name overlaps a schema path or another former name, or if a
modeled setting has both explicit `env_vars` and former names. `env_var_names` and `former_names` in the overlay model's
`schema_gen` hold these rules.

A new rename needs config recorder cases: `renamed_settings_have_recorder_cases` in `datadog-agent-config-corpus`
fails until `renamed-key-*` cases record the setting and its former names, by env and by YAML.

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

A support change needs only these steps. Do not record the corpus or add replay bookkeeping for
it: the corpus decides which replay checks exist, and a newly supported key gets value comparisons
once a later recording includes it. A replay check that fails after a support change means a
recorded outcome changed: fix ADP, or edit the matching expectation deliberately. Removing support
from a key the baseline case recorded fails coverage and may leave value-check expectations
unused; handle that as deliberate corpus maintenance.

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
6. Keep `SALUKI_KEYS` consistent with the source key and default.

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
   - Count every value the caller supplies, including runtime wiring the caller can't read out of configuration.
   - Fewer than four values: `new(...)` taking concrete values, with private fields. Existing `with_*` wiring methods
     stay.
   - Four or more values: make the fields `pub` and let the caller build the struct by field, with no constructor.
     Wiring becomes a `pub` field too: see `aggregate_context_snapshot_channel` and
     `AggregateConfiguration::context_snapshot_receiver`. Avoid introducing a new arguments or settings struct, and
     don't implement `Default` or `Deserialize` on the configuration struct.
   - With `pub` fields, compute derived values and run input handling in `build()` or a private accessor, once per
     build. Keep a `with_*` method only when it does more than set one field.
   - Keep a constructor, whatever the count, when it does more than assign fields: it builds private types,
     compiles or validates input, or decides which endpoints apply. Name it for what it builds.
   - Values that change at runtime: pass a `Live<T>` of a component-owned `T`, and rebuild the reactive state after
     `changed()`. Use `project` when the model value already has the right type, `map` when it must convert, and
     `into_stream` for a stream of changed values without an initial value. Values that must change together belong
     in one `Live<T>`: separate views advance independently.
7. Remove source serde, Datadog key names, raw-map access, key watches, parsing, configuration defaults, and code made
   unused by the cutover. `#[allow(dead_code)]` isn't an acceptable way to keep migration residue.
8. Update topology call sites and tests. Preserve behavior tests by using typed inputs, and remove tests only when they
   tested legacy deserialization and nothing else.
9. Test higher-risk cutovers with correctness or integration tests that exercise the affected configurations.

A cutover should be behaviorally transparent. If the old behavior conflicts with the source schema or the typed
system's invariants, surface the conflict rather than silently choosing one. Don't leave deserialization artifacts
behind after a cutover: for example, drop `derive(Deserialize)`, and collapse a held `Option<SomeType>` to a held
`SomeType` where possible.

### Update the vendored schema

Use the `config-schema-update` skill, which walks through comparing, vendoring, and classifying a new upstream schema.

## Review checklist

These lists are the review rules for configuration changes. Each list is also a scoped `codereview_guideline.md`
file, or part of one. The AI reviewer loads a file when a pull request changes a file under its directory. So each
list stands alone, and repeats what it needs from the earlier sections. Before you open a pull request, check your
change against the lists for the directories that it changes.

The lists also cover `lib/saluki-config` and `lib/datadog-agent/runtime`. The runtime opens the Core Agent
configuration stream and converts its settings, so its rules live here, next to the stream rules.

The architecture tests in `test/architecture` enforce the dependency boundaries of the configuration crates, so these
lists don't repeat them. No test covers the boundary of the runtime, so its list does.

### Datadog schema and inventory

In `lib/datadog-agent/config/codereview_guideline.md`:

<!-- review:config-schema -->
Apply these rules to files under `lib/datadog-agent/config`. This crate holds the vendored Datadog schema, its
overlay, the code generators, and the decoders for Datadog keys.

- Files under `schema/core/` and `schema/otel/` are vendored upstream copies. A vendor update replaces them with the
  files of one upstream commit, and also changes `_version.txt` to that commit. Don't flag the changes of a vendor
  update. Flag a change to these files that doesn't update `_version.txt`.
- Don't flag a hand edit or a stale file under `src/generated/`. The `check-schema-overlay` CI job regenerates these
  files and fails on any difference.
- The generated `DatadogConfiguration` reads each key in the Agent's canonical shape. Flag a generator change that emits
  a serde `rename` or `alias`, for example for a `renamed_from` name. The environment and file readers, and
  `overlay_agent_layer` for the stream, resolve former names before deserialization.
- Check each `support` value and each `excluded` entry in `schema/schema_overlay.yaml` against what ADP does with the
  key:
  - `full`: ADP reads the key, accepts each value that the Agent accepts, and does what the Agent does. If ADP rejects
    some values, such as TLS versions below 1.2, or differs in some cases, the key is `partial`. Its `documentation`
    describes the difference.
  - `none`: setting the key changes what users observe, and ADP doesn't do the same. Choose `severity` from that effect,
    and give an experimental key `low` at most. ADP doesn't start when an input sets a `high` key for an active
    pipeline.
  - `unknown`: ADP's compatibility with the key isn't known yet. With a `severity`, the startup check applies the key to
    every pipeline. So ADP doesn't start when an input sets an `unknown` key with `severity: high`.
  - `excluded`: the feature behind the key doesn't affect what ADP does. A `none` entry for such a key shows operators
    an incompatibility warning that isn't true. Flag a key under `excluded` that ADP reads or must honor.
- Scalar leaves use the `cast_de` coercions for every source, including the Core Agent stream. Flag scalar coercion that
  only one source gets. Don't flag a per-key `env_parser` transform, such as `comma_separated`: as in the Agent, it
  applies only to environment strings.
- A decoder accepts each form that the Agent accepts for the key, and a test covers each form. The stream sends some
  forms of its own. A map that the Agent read from the environment arrives as a JSON string. An empty collection arrives
  as `null`, and a duration as integer nanoseconds. Other forms include:
  - a scalar or a list
  - a quoted number, a `0x` integer, or a leading-zero octal integer
  - a byte size such as `12mb` or `12MB`
  - a duration as a Go duration string such as `10s`, or as a bare number of nanoseconds in a string
<!-- /review -->

### Configuration model

In `lib/agent-data-plane-config/codereview_guideline.md`:

<!-- review:config-model -->
Apply these rules to files under `lib/agent-data-plane-config`, the typed configuration model
(`SalukiConfiguration`).

- A field whose meaning depends on whether an input set it is a `ConfigValue<T>`, and its readers call
  `is_explicit()`. A model method, such as `Endpoints::primary_endpoint`, resolves the provenance, so that components
  get only the effective value. Flag code that compares a value with its default to decide whether someone set it.
- Use `Option<T>` only when absence has a meaning, not to defer a default. Prefer `ConfigValue<T>` to
  `ConfigValue<Option<T>>`.
- Define each Saluki-only default once, in `src/defaults.rs`, and use that constant everywhere that the default
  appears. A witnessed field's `Default` holds only a placeholder, because translation always writes the default from
  the generated schema. Flag a Datadog schema default that the model restates.
- A Saluki-only default is a value that its consumer accepts. For example, a connect target has no default port `0`.
  If no default works, use `Option<T>`, and report an error when the value is missing.
- A field that a consumer divides by, or uses as a window or bucket width, has a type that rejects `0`, such as
  `NonZeroU64`. A number in a fixed unit has the unit in its name, such as `window_duration_seconds`.
<!-- /review -->

### Loading and translation

In `lib/agent-data-plane-config-system/codereview_guideline.md`, together with the
[Core Agent configuration stream](#core-agent-configuration-stream) rules:

<!-- review:config-translation -->
Apply these rules to files under `lib/agent-data-plane-config-system`. This crate loads the local file and the
environment, layers the Core Agent stream over them, and translates the result into `SalukiConfiguration`.

- A recorded `TranslateError` is a startup error, and it also rejects the whole live update. To recover from a bad
  value, log a warning and use what the Agent uses, such as the default.
- A key in the vendored schema is a Datadog key, whatever its prefix. The translator gets its value through its
  `consume_<key>` method, and its `support` in `lib/datadog-agent/config/schema/schema_overlay.yaml` says what ADP does
  with it. Any other key is a `SalukiOnly` field. Flag a `SalukiOnly` field for a schema key. Also flag a change to what
  ADP does with a Datadog key that doesn't update its `support`.
- `SalukiOnly` field names and nesting match the key path exactly. A mismatch doesn't fail: the value doesn't arrive,
  and the model keeps its default. Flag `#[serde(rename)]` and `#[serde(alias)]` on these fields. Also flag a new field
  without a test that sets the real key and checks the model field.
- A `SALUKI_KEYS` entry in `lib/datadog-agent/config-overlay-model/src/saluki_keys.rs` has the same path as its
  `SalukiOnly` field. Its displayed default is a string, not the constant. Check that it matches the effective default,
  such as the constant in `lib/agent-data-plane-config/src/defaults.rs`. Flag a change to the path or the default of a
  key that doesn't update its entry.
- A change to the source or model type of a key keeps each input form that the last release accepted, and each effective
  default. A move of a key between `SalukiOnly` and the schema does too. Ask the author to list the input forms, and
  check that a test covers each one.
- In connected mode, ADP layers the Core Agent stream over the local sources. In the local sources, the environment
  overrides `datadog.yaml`. A value that the stream marks as a default doesn't hide a value that a local input set. Flag
  a change to this order that the pull request doesn't explain.
- Corpus replay compares each modeled setting with what the recorded Agent read. When a change makes a replay check
  fail, fix ADP, or add an entry to `src/corpus_replay/expected.rs`. The `Cause` of the entry says why ADP differs. Flag
  an `intentional` entry for an ADP bug, and an entry whose `Cause` doesn't explain the difference. Flag a new recording
  of the corpus that only makes an ADP change pass. Don't flag a missing tracking issue on a `known_bug` entry: the
  replay test fails without one.
<!-- /review -->

### Core Agent configuration stream

In the guideline files for `lib/agent-data-plane-config-system` and `lib/datadog-agent/runtime`:

<!-- review:config-stream -->
Apply these rules to files under `lib/agent-data-plane-config-system` and `lib/datadog-agent/runtime` that read the
Core Agent configuration stream or its values.

- The Core Agent streams every key that it knows. A key that nobody set arrives with its default, which for many string
  keys is an empty string that the Agent treats as unset. `setting_to_config_setting` gives each `ConfigSetting` a
  `Provenance`, and marks the Agent sources in `AGENT_UNSET_SOURCES` as defaults. For a Datadog schema key, flag code
  that decides from presence, such as `is_some()` or `contains_key`. Also flag code that treats `Some("")` as a
  configured value.
- A Saluki-only key isn't in the Agent's schema, so the Agent streams it only when an input set it. `SalukiOnly::seed`
  copies each value that is present, which is correct for these keys. Don't flag it.
- When two keys set one underlying value, the stream sends both keys, even if nobody set one of them. An Agent older
  than the rename of a key also streams the key under its former name. `overlay_agent_layer` in
  `lib/agent-data-plane-config-system/src/system.rs` resolves the pair. Flag a fallback that fails or picks the wrong
  value when both keys arrive. Choose the key by provenance.
- A setting key is flat and can be dotted, and its value is the whole value of one setting. Expand the key into
  sections, and insert the value whole. A map key in the value, such as an intake URL in `additional_endpoints`, can
  contain periods.
- In connected mode, `ConfigurationSystem::connected` in `lib/agent-data-plane-config-system/src/system.rs` waits for
  the first snapshot from the stream, and translates it. `RemoteAgent::start` doesn't return until then. Flag a change
  that removes this wait, or that returns a configuration before the first snapshot translates.
<!-- /review -->

### Remote agent runtime

In `lib/datadog-agent/runtime/codereview_guideline.md`, together with the
[Core Agent configuration stream](#core-agent-configuration-stream) rules:

<!-- review:remote-agent-runtime -->
Apply these rules to files under `lib/datadog-agent/runtime`. This crate is the shared runtime for each process that
runs as a remote agent of the Datadog Agent, such as ADP.

- The runtime serves more than one binary. Its configuration scope stops at the `ConfigUpdate` stream and the traits in
  `src/facade/config.rs`. Flag any ADP-specific references, such as ADP crates or mentions of "Agent Data Plane" or ADP.
  Code that depends on details about the current data plane binary the code is used within should get those details from
  `saluki_metadata`.
- `LocalConfiguration` holds the values from the file and the environment. It supplies only the values that the
  process needs before the stream starts: standalone mode, the client configuration, and the advertised secure API
  address. The runtime uses them only to choose the mode, to register, and to open the stream. After
  `RemoteAgent::start`, code reads the `AuthoritativeConfiguration`, which can hold different values. Flag code that
  uses a local value after startup. Also flag a new value in `LocalConfiguration` that the authoritative
  configuration can supply.
- A Core Agent stream can start with a snapshot and send its events to subscribers that arrive later, as the
  autodiscovery broadcaster does. Such a stream opens only when its first subscriber exists. A subscriber that
  arrives after the stream opens misses the snapshot. Flag a broadcaster that opens such a stream when its worker
  starts. Don't flag a stream with one consumer that exists before the stream opens, such as the tagger and
  workloadmeta streams.
<!-- /review -->

### Configuration library

In `lib/saluki-config/codereview_guideline.md`:

<!-- review:config-library -->
Apply these rules to files under `lib/saluki-config`, a general-purpose configuration library.

- ADP uses only part of this crate, such as the stream types in `dynamic`, the `yaml` module, and `DurationString`.
  `GenericConfiguration`, `ConfigurationLoader`, and the by-key dynamic view, such as `FieldUpdateWatcher`, have no
  consumer in this repository, but they serve external clients. Don't flag them as unused. Flag a change that
  removes them, or that removes their unit tests, because those tests are their only coverage.
- A setting key is flat and can be dotted. Expand it into nested maps, as `upsert` does, and insert the value whole.
<!-- /review -->

## Check your work

The `Makefile` is authoritative:

```shell
make build-schema-overlay       # regenerate code
make fmt                        # use this, not cargo fmt
cargo check --workspace --tests # quick compilation check
make check-all                  # required to pass CI
```

A change to a deserializer, the environment reader, translation, or the support of a key also needs the corpus replay
tests:

```shell
cargo nextest run --lib -p agent-data-plane-config-system corpus_replay
```

A failing check means that a recorded outcome changed. In the same change, fix ADP, or edit the matching expectation
in `lib/agent-data-plane-config-system/src/corpus_replay/expected.rs` on purpose. Never regenerate the corpus to make
an ADP change pass.
