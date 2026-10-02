# Contract: case file

A case file describes one test run of the [config recorder](../README.md), the Go program that
saves Agent configuration behavior for Rust compatibility tests. It supplies startup inputs (env,
YAML, fleet policy, CLI overrides), runtime updates, and the settings to observe. The recorder
loads those inputs through the Agent's code, captures its config stream, and calls its
configuration methods to record their results.

This contract defines the file's syntax and how the recorder applies each field.

Agent citations are at `281d921619d`.

## 1. File

- A case file must be YAML, UTF-8, one case per file, named `<name>.yaml`.
- Hand-written cases (group `behavior`) live in `lib/datadog-agent/config-recorder/cases/` in
  saluki. Generated cases must not be checked in; the generator builds them in memory or in a
  temporary directory.
- The harness must parse case files with `gopkg.in/yaml.v3` and must reject (non-zero exit, no
  records) any file with an unknown top-level field, a missing required field, or a field of the
  wrong type.

## 2. Fields

| Field          | Type                              | Req. | Meaning                                              |
|----------------|-----------------------------------|------|------------------------------------------------------|
| `name`         | string, `^[a-z0-9][a-z0-9-]*$`    | yes  | Unique across the corpus; equals the file stem.      |
| `group`        | enum, §3                          | yes  | Coverage group.                                      |
| `why`          | list of catalog ids               | §3   | Behavior catalog `id`s (§3).                         |
| `env`          | map string → string               | no   | The whole process environment (§4.2).                |
| `yaml`         | string                            | no   | Verbatim content of `datadog.yaml` (§4.3).           |
| `fleet_policy` | string                            | no   | Verbatim content of the fleet `datadog.yaml` (§4.4). |
| `cli`          | list of `{key, value}`            | no   | Startup CLI overrides (§4.5).                        |
| `secrets`      | map string → string               | no   | Secret handle → plaintext (§4.6). Optional feature.  |
| `updates`      | list of update objects            | no   | Applied after the first snapshot (§5).               |
| `keys`         | list of key entries               | yes  | Keys to record, at least one (§6).                   |

## 3. Groups

`group` must be one of: `baseline`, `breadth`, `depth`, `unsupported`, `excluded`, `unknown`,
`behavior`.

- `unknown` means the recorded keys are not in the schema.
- `behavior` cases are hand-written and must have a non-empty `why`. `why` ids name entries in a
  catalog of behavior ids that is maintained outside this repository; the recorder never reads it.
  Each hand-written case carries a top-of-file comment saying what it shows. A separate
  traceability check outside this repository verifies that every `why` id exists and every catalog
  entry has a case.
- Other groups may carry `why`; it defaults to `[]`.

### 3.1 Batching

- Generated cases (every group but `behavior`) may, and should, list many keys in one case: one
  case per source (breadth) or per variant (depth), not one case per key. The corpus cap
  (record.md §1) cannot be met with one case line per key.
- Hand-written `behavior` cases must stay small: only the keys their `why` entries need.
- A breadth case may give YAML inputs for its keys and also one `set` update per key (§5). Its
  `snapshot` read then records the YAML path and its `final` read the `Set` path, so `Set`
  breadth needs no case of its own.
- Batching costs fidelity: keys in one process can interact (data-plane fix-ups, conflicting
  options, `site` with `dd_url`, the proxy keys). So the driver must fall back to isolation, by
  the rules below. A single-key case is recorded as it is; its outcome is data.
- The source a case sets a key from is `environment-variable` for `env`, `file` for `yaml`,
  `fleet-policies` for `fleet_policy` and `cli` for `cli`. When several of a case's inputs set a
  key, the expected source is the highest-priority one, by the Agent's own source order
  (`model.Source.IsGreaterThan`). For a key the case does not set, the expected source is the
  key's streamed source in the baseline process (usually `default`; the proxy keys get
  `config-post-init` from the Agent even with no inputs).
- A key is clean when its streamed `source` in the case's first snapshot equals the expected
  source. A key absent from the case's first snapshot is clean only when it is also absent from
  the baseline's.
- The `baseline` group is one case, never split: it has no inputs, so its keys cannot interact.
  `behavior` cases are never split either: they are recorded as written. Every other group is
  split by these rules, whether its case was generated or hand-written.
- Splitting a case with more than one key. Call the case as loaded the *root*:
  1. Run the root. If it started and every key is clean, record it.
  2. If it started and some keys are not clean, *peel* them: each becomes a single-key part.
     Remove them from the root and go back to step 1 with the remaining keys, still under the
     root's name. If no key remains, the root writes no records.
  3. If it failed to start, find the *culprits*. Split its keys in key order into a first half of
     `n/2` keys (rounded down) and the rest, and run each half. A half that starts is discarded,
     since it only served the search. A half that fails is searched the same way, until it holds
     one key. That key is a culprit and becomes a single-key part. Remove the culprits from the
     root and go back to step 1. If the search finds no culprit (the failure needs keys from both
     halves), every key of the root becomes a single-key part.

  Single-key parts are recorded as they are. So the recorded cases are the root (with the keys
  that behave in the batch), if any keys remain, and one part per peeled key or culprit. Names
  depend only on which keys misbehave, not on where they sit in the batch.
- Each input goes to the case or part holding the key it sets:
  - an `env` name goes with every key the schema binds it to;
  - `yaml` and `fleet_policy` are split by pruning the YAML node tree to the paths of the kept
    keys, so a kept scalar keeps its style and tag;
  - `cli` entries and updates go by their `key`, keeping their relative order.

  An input that sets none of the case's keys, or YAML that does not parse or uses anchors,
  aliases or merge keys, is a harness failure when the case must be split.
- A part keeps the root's `group` and `why`. A single-key part for key `k` is named
  `<root>--<k'>`, where `<k'>` is `k` lowercased, with every character outside `[a-z0-9]`
  replaced by `-`. A part name equal to another case's name is a harness failure. The halves run
  during a culprit search record nothing.
- Every process's scratch files (case file, run directory, result) must be private to it, even
  when two processes share a case name (a root and its rerun, a half and a part). The case file
  is still named `<name>.yaml`, since case.md §1 requires it: the directory holding it is what
  differs.
- A catalog entry about an interaction between keys must keep its own isolated `behavior` case.

### 3.2 Generated groups

The generator reads the merged schema at the pin and the ADP overlay
(`lib/datadog-agent/config/schema/schema_overlay.yaml`: only each `inventory` entry's `support`
and the `excluded` key names). Keys are compared lowercased. A schema key is a leaf with
`node_type: setting`. *Modeled* keys have `support` `full` or `partial`; *unsupported* keys have
`none` or `unknown`.

| Group         | Keys                                                                 | Sources              |
|---------------|----------------------------------------------------------------------|----------------------|
| `baseline`    | every modeled schema key, in one case                                | none                 |
| `breadth`     | every modeled schema key                                             | env; YAML plus `set` |
| `unsupported` | every unsupported schema key                                         | env; YAML            |
| `excluded`    | per Go default type (`%T` of the default), the byte-first excluded key | env; YAML          |
| `unknown`     | the fixed set in §3.3                                                | YAML; env for `DD_*` |
| `depth`       | per class, one representative modeled key; variants by rule (§3.2.1) | env; YAML; `set`     |

- A key gets an env case only if the Agent binds an env var for it. The env name is the first
  name the schema gives, and it must be in the Agent's `GetEnvVars()`; otherwise it is a harness
  failure.
- Batches are split by top-level schema section (the first path component). Top-level leaves
  (keys without a dot) are not sections: they are batched by first character, as section
  `top-<c>`. A section with more than 40 keys is split by its second path component: its direct
  leaves stay in `<section>`, and each sub-section becomes `<section>-<sub>`. There is no
  further splitting and no count-based chunking, so a batch name depends only on key paths.
- A generated case is named `<group>-<source>-<section>` (the baseline case is
  `baseline-default`). Names never depend on run order.
- Overlay keys that are not schema keys are not a generated group: the Agent reads them, if at
  all, as part of a section. A hand-written `behavior` case covers them.
- Generated values are chosen by rule from the key's schema type and default, never at random. A
  breadth YAML value differs from the default; its `set` value differs from the YAML value and,
  when the type has more than two values, from the default. A `number` value always has a
  fractional part, so it is recorded as a float (§7).

### 3.2.1 Depth

Depth records the input shapes breadth does not: empty, null, wrong-shape and alternate
spellings, one modeled key per class.

- **Class.** For YAML and `set` variants, a modeled key's class is its default-layer Go type
  (`%T` of the default, as getter selection uses, getter-map.md §1). For env variants it is that
  type and the key's schema `env_parser` (none counts as a value), since the parser only acts on
  env input. A nil-default key's type is `<nil>:<schema type>`. A class's *representative* is
  its pinned key (`go/gen/depth_reps.go`), fixed by review at the Agent pin; the byte-first
  modeled key (the byte-first env-bound key for an env class) only proposes a representative
  while the pin is derived. A class with no env-bound key gets no env variants. A schema bump
  that adds a key sorting earlier does not move a class's rows to it, and a representative removed from the model
  or moved to another type, a new class, and a pin entry whose class no longer exists all fail
  generation with what to update. The type's *kind* is `list`
  (`[]…`), `map` (`map[…]…`) or `scalar` (every other type).
- **Cases.** One case per variant, named `depth-<source>-<variant>`, holding every class's
  representative that the variant applies to. The splitting rules of §3.1 apply, so a variant
  whose keys always stream the baseline source (an empty env var, a YAML null) ends as
  single-key parts `depth-<source>-<variant>--<k'>`. That is expected, not a failure.
- **Getters.** Each depth key entry lists the key's default getters, then `Get` if it is not
  already there (§6), so every record shows the stored value next to the typed read. (A string
  read of a collection is recorded once, in a behavior case, not per variant.)
- **Budget.** The depth group must be at most 80,000 bytes of corpus lines (target 60,000). A
  corpus test checks it and, on failure, lists bytes per variant. The generator never drops a
  variant by itself: a person edits this table, whose rows are in priority order (cut from the
  bottom; the secondary-name variant is never cut).
- **Variants.** `v` is the key's breadth Input value and `w` its breadth Set value (§3.2 value
  rules). YAML variants write the value at the key's path; `set` variants are updates only (no
  other inputs), with source `agent-runtime`; env variants set the key's env name.

  | Variant                  | Kinds        | Input |
  |--------------------------|--------------|-------|
  | `yaml-empty-list`        | all          | YAML `[]` |
  | `yaml-empty-map`         | all          | YAML `{}` |
  | `yaml-null`              | list, map    | YAML `~` |
  | `env-empty`              | list, map    | env `""` |
  | `set-empty-list`         | list, map    | set `[]` |
  | `yaml-mixed-string`      | list, map    | YAML string `cr-a cr-b,cr-c` |
  | `set-mixed-string`       | list         | set `"cr-a cr-b,cr-c"` |
  | `env-mixed-string`       | list         | env `cr-a cr-b,cr-c` |
  | `env-json-list`          | list         | env `["cr-a","cr-b"]` |
  | `env-bool-words`         | `bool`       | env `on` |
  | `set-bool-words`         | `bool`       | set `"on"` |
  | `set-empty-string`       | `bool`, `int`, `float64` | set `""` |
  | `env-not-json`           | map          | env `cr_a:cr-a` |
  | `set-json-string`        | map          | set the JSON text of `w` as a string |
  | `yaml-map-int-values`    | map          | YAML `{cr_a: 1}` |
  | `yaml-float`             | `int`, `int64` | YAML `v + 0.5` |
  | `env-float`              | `int`, `int64` | env `v + 0.5` as text |
  | `env-hex`                | `int`, `int64` | env `0x10` |
  | `yaml-int`               | `float64`, `time.Duration` | YAML `floor(v) + 1` as an integer; `30` for a duration |
  | `env-exponent`           | `float64`    | env `1e3` |
  | `env-bool-digit`         | `bool`       | env `1` |
  | `yaml-quoted`            | scalar       | YAML `v` as a double-quoted string |
  | `yaml-one-item-list`     | scalar, map  | YAML `[v]` for a scalar, `[cr-a]` for a map |
  | `set-string`             | scalar       | set `w` as a string (`fmt` `%v`) |

- A YAML null and an empty env var are ignored for every known key before its type is consulted
  (`yaml-null-known-key-ignored`, `env-empty-value-treated-as-unset`), so those two variants
  cover only the collection kinds, where an ADP reader could receive a null.
- **Secondary env names.** Variant `env-secondary-name`: every modeled key whose schema lists
  more than one env name, set only by its last name, to `v` as env text. (No modeled key has a
  deprecated name at the pin; §3.3 covers `renamed_from`.)
- A variant that would equal a breadth input for the same key is still written; depth does not
  remove duplicates against breadth.

### 3.3 Fixed unknown keys

An unknown top-level YAML key, an unknown YAML key under a modeled section, an unknown `DD_*` env
var, and the byte-first deprecated name (`renamed_from`) of a modeled key, set in YAML and by env.

## 4. Construction

### 4.1 Process and platform

- Each case must run in its own harness process. Nothing may be shared between cases.
- Before any case, the driver must run one baseline process: a case with no inputs, whose first
  snapshot is the reference for `side_effects` and the header (record.md §2, §3.2).
- Regeneration must run in the digest-pinned `golang:1.26.7` Linux container.
- The harness must build the config through the Agent's production `fx` path:
  `fxutil.OneShot` with `fx.Supply(config.NewAgentParams(path,
  opts...))`, `config.Module()`, `comp/core/secrets/fx-noop`, `comp/core/delegatedauth/fx-noop`,
  `comp/core/telemetry/fx-noop`, `logimpl.NewTemporaryLoggerWithoutInit()` and
  `configstreamfx.Module()`. It must not call `SetTestOnlyDynamicSchema` or any test-only API.
  This runs `LoadDatadog`, `MergeFleetPolicy`, the data-plane fix-ups and the CLI overrides
  (`comp/core/config/setup.go:53-94`).
- Before anything logs, the harness must route the Agent's global logger into an in-process
  recorder with `pkglog.SetupLogger(ddslog.NewWrapper(handler), "debug")`
  (`pkg/util/log/log.go:67`, `pkg/util/log/slog/wrapper.go:40`).

### 4.2 `env`

- The process must be started as `env -i` plus exactly the entries of `env`. The harness itself
  must take all its own parameters from flags or files, never from environment variables.
- Names must match `^[A-Za-z_][A-Za-z0-9_]*$`. Values are strings; YAML scalars that are not
  strings must be rejected, so authors must quote `"1"`, `"true"`, `"10"`.
- An empty string is a legal value and must be set as an empty variable, not dropped
  (`env-empty-value-treated-as-unset`).
- `DOCKER_DD_AGENT` is an ordinary entry. The container alone must not make
  `env.IsContainerized()` true (`pkg/config/env/environment.go:17`).

### 4.3 `yaml`

- The harness must write `yaml` byte-for-byte to `<workdir>/datadog.yaml` and pass that path to
  `NewAgentParams`. When `yaml` is absent it must write an empty file, so `LoadDatadog` always
  takes its full path (`pkg/config/setup/config.go:411-416`, `:527`).
- `<workdir>` must be a fresh directory per case. The file name must be `datadog.yaml`, because
  the stream's `origin` is its base name (`comp/core/configstream/impl/configstream.go:352-358`).
- Invalid YAML is a legal input. A startup failure is an outcome (record.md §4.1).

### 4.4 `fleet_policy`

- When present, the harness must write it byte-for-byte to `<workdir>/fleet/datadog.yaml` and
  pass `config.WithFleetPoliciesDirPath("<workdir>/fleet")` (`comp/core/config/params.go:136`).
  The Agent merges it after `LoadDatadog` (`comp/core/config/setup.go:70-89`).
- When absent, the harness must not pass `WithFleetPoliciesDirPath`; the Agent then falls back to
  the `fleet_policies_dir` setting (`setup.go:70-72`).

### 4.5 `cli`

- Each entry is `{key: string, value: <typed value>}`. The harness must pass
  `config.WithCLIOverride(key, v)` (`comp/core/config/params.go:144`), with `v` decoded by §7.
- Keys must be unique within `cli`. The Agent stores overrides in a map and applies them in map
  order (`params.go:146`, `setup.go:92`), so the case must not depend on their relative order.

### 4.6 `secrets` (optional)

- Maps each `ENC[...]` handle's inner text to its plaintext. Until a stub resolver exists, the
  harness must reject any case with a non-empty `secrets` with an error naming the field, and
  write no records. A harness must never silently ignore `secrets`.

## 5. Updates

Updates run after the first snapshot has been received and the `snapshot` reads (record.md §5)
are done. They run in list order, one at a time: the harness must apply update *i*, then wait
for its events (record.md §4.2), then apply update *i+1*.

| Field    | Type                       | Req.        | Meaning                           |
|----------|----------------------------|-------------|-----------------------------------|
| `op`     | `set` \| `unset`           | no (`set`)  | Operation.                        |
| `key`    | string                     | yes         | Key as passed to the Agent.       |
| `value`  | typed value (§7)           | `set` only  | Must be absent for `unset`.       |
| `source` | `model.Source` string      | yes         | Layer written or cleared.         |

- `set` must call `cfg.Set(key, v, model.Source(source))` (`pkg/config/model/types.go:229`),
  with `v` decoded by §7. A string `value` models the CLI `config set` path; a typed value models
  remote-config and other in-process writers.
- `unset` must call `cfg.UnsetForSource(key, model.Source(source))` (`types.go:231`).
- `source` must be one of these `model.Source` strings (`types.go:37-57`): `infra-mode`, `file`,
  `environment-variable`, `fleet-policies`, `config-post-init`, `secret`, `local-config-process`,
  `agent-runtime`, `remote-config`, `cli`. `default`, `schema`, `unknown` and `provided` must be
  rejected.
- `environment-variable` is allowed on purpose: outside tests `Set` only guards it with
  `panicInTest` and proceeds (`pkg/config/nodetreemodel/config.go:230-232`;
  `env-var-set-guard-is-test-only`).
- `key` may be unknown or name a section; the Agent's handling of these is the outcome
  (`config.go:237-251`; `set-on-inner-node-or-unknown-key-dropped`).
- Out of scope, and the harness must not offer them: subscriber backpressure or delays (C7,
  live tier only) and `[]byte` values (C8; no JSON value decodes to `[]byte`).

## 6. Keys

Each entry of `keys` is either a string (the key) or `{key: string, getters: [name, ...]}`.

- Keys must be unique within a case, and must be lowercase dotted paths as the Agent flattens
  them. The harness must reject a case with a key containing an uppercase letter. Keys not in the
  schema are allowed.
- `getters`, when given, replaces the default getter list from `getter-map.md` for that key, and
  must be non-empty. Each name must be one listed in `getter-map.md` §2.
- A key not in the schema and without `getters` uses the unknown-key default in `getter-map.md`.

Note: a getter read at the `snapshot` checkpoint on an unknown key records it in the Agent's
`unknownKeys` (`getter-unknown-key-joins-keyset`, `pkg/config/nodetreemodel/config.go:637-645`).
That does not make it known to `Set`, which checks `knownKeys` only (`config.go:236`), so a later
update on the key is still dropped (recorded in `unknown-key-getter-then-set`).

## 7. Typed values

`cli[].value` and `updates[].value` must be decoded to Go as follows before the Agent call. The
YAML node's resolved tag decides:

| YAML node (`yaml.v3` tag)        | JSON equivalent                   | Go value                 |
|--------------------------------|-----------------------------------|--------------------------|
| `!!str`                        | string                            | `string`                 |
| `!!bool`                       | `true`/`false`                    | `bool`                   |
| `!!int`                        | number without `.`, `e`, `E`      | `int` (64-bit)           |
| `!!float`                      | number with `.`, `e` or `E`       | `float64`                |
| `!!null`                       | `null`                            | `nil`                    |
| sequence                       | array                             | `[]interface{}`          |
| mapping, all keys `!!str`      | object                            | `map[string]interface{}` |

- Decoding must recurse into sequences and mappings with the same table.
- An `!!int` outside the int64 range, a non-finite `!!float` (`.nan`, `.inf`), a mapping with a
  non-string key, and any other tag (`!!binary`, `!!timestamp`, custom) must be rejected.
- The JSON column is normative: the same value in a record's case line (record.md §3) must decode
  to the same Go value by the same rules, so a case line can be replayed without the YAML file.
- Values inside `env`, `yaml` and `fleet_policy` are text and are not decoded by the harness.
- A sequence decodes to `[]interface{}`, so a `Set` on a `[]string` key logs the Agent's "converting
  value from []interface {} to []string" warning. That warning belongs to this input shape: a caller
  passing `[]string` would not log it. The stored and streamed value is the same either way.

## 8. Example

The `additional_endpoints` env case from S1, as a hand-written case:

```yaml
name: additional-endpoints-env
group: behavior
why: [env-map-raw-string]
env:
  DD_ADDITIONAL_ENDPOINTS: '{"https://x.test": ["k"]}'
keys: [additional_endpoints]
```

The `logs_enabled: "yes"` YAML case from S1b:

```yaml
name: logs-enabled-yes-yaml
group: behavior
why: [getter-bool-string-strict-parsebool, yaml-type-mismatch-scalar-leaf-keeps-raw]
yaml: |
  logs_enabled: "yes"
keys: [logs_enabled]
```

A case using the other fields (not from a spike; shows syntax only):

```yaml
name: dogstatsd-port-layers
group: behavior
why: [fleet-policies-outranked, unset-always-notifies-even-if-unchanged]
fleet_policy: |
  dogstatsd_port: 8130
cli:
  - {key: cmd_port, value: "5099"}
updates:
  - {key: dogstatsd_port, value: 8131, source: remote-config}
  - {op: unset, key: dogstatsd_port, source: remote-config}
keys:
  - dogstatsd_port
  - {key: cmd_port, getters: [GetInt, GetString]}
```

## 8a. Readings settled during implementation

1. `secrets` is rejected only when non-empty; an empty map or `null` is accepted.
2. An integer literal beyond the uint64 range (which `yaml.v3` tags `!!float`) is rejected as an
   out-of-range integer, since its JSON form is an integer.
3. A typed-value mapping with a duplicate key is rejected (`yaml.v3` does not check this inside a
   node).
4. A `getters` override that lists the same getter twice is rejected.

## 9. Decisions

1. `secrets` (C4): reserved and rejected. Revisit with the live tier or when the catalog entry
   `stream-secret-plaintext` is worked; the case would use the Agent's real secrets component with
   a generated backend script, not a stub resolver.
2. `unset` for any allowed source is permitted: the Agent API accepts it, and the outcome is data.
3. Only `datadog.yaml` is supported.
4. Generated cases batch many keys, with bisection back to isolation (§3.1); `Set` breadth
   folds into the YAML breadth case as updates.
5. Uppercase keys are rejected, not recorded (§6).
