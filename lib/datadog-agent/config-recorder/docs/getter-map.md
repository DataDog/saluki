# Contract: getter map

This contract fixes which Agent getters the config recorder calls on a key and how each Go result is written as JSON in a record's `reads` (record.md §5.3). It says nothing
about how the Rust side uses those results.

Agent citations are at `281d921619d`. `getter.go` is `pkg/config/nodetreemodel/getter.go`;
`codegen` is `tasks/schema/codegen_init_settings.py`.

## 1. Selection

- The default getter list for a schema key must be chosen from the Go type of the key's
  default-layer value as the Agent holds it: `fmt.Sprintf("%T", v)` where `v` is the `Value` of
  the first element of `cfg.GetAllSources(key)` (`getter.go:112-126`), whose `Source` is
  `default` (`nodetreemodel/config.go:33-34`). When that element is missing, is not `default`,
  or holds `nil`, the key has no default and §1.1 applies. The harness must not re-derive
  codegen's type inference from the schema.
- The primary getter comes from the table below. A Go type not in it is a harness failure naming
  `%T`; adding a type bumps the record format.
- Schema tags may only add secondary getters, appended after the primary one:
  - `format: duration` on a key whose default is a `string` adds `GetDuration`;
  - `golang_type: duration` on a key whose default is an `int` or `float64` adds `GetDuration`.
- `env_parser` must not change the getters.
- The harness must read these tags, and which keys are schema keys and sections, from the Agent
  schema YAML in the Agent checkout it builds against, not from saluki's vendored copy.
- The harness must call `GetAllSources` only on schema keys, after the first snapshot is
  received (record.md §4.1), and must not record its result or warnings.
- A case may replace the list per key (case.md §6). Otherwise, a key not in the schema must use
  `[Get]`, and a key naming a section (an inner node) must use `[Get]`.

| Default-layer Go type (`%T`)                      | Getters                   |
|---------------------------------------------------|---------------------------|
| `bool`                                            | `GetBool`                 |
| `int`                                             | `GetInt`                  |
| `int64`                                           | `GetInt64`                |
| `float64`                                         | `GetFloat64`              |
| `string`                                          | `GetString`               |
| `time.Duration`                                   | `GetDuration`             |
| `[]string`                                        | `GetStringSlice`          |
| `[]int`                                           | `Get`, `GetStringSlice`   |
| `[]float64`                                       | `GetFloat64Slice`         |
| `[]interface {}`                                  | `Get`                     |
| `[]map[string]…` (any value type)                 | `Get`                     |
| `map[string]interface {}`                         | `GetStringMap`            |
| `map[string]string`                               | `GetStringMapString`      |
| `map[string][]string`                             | `GetStringMapStringSlice` |
| `map[string]float64`                              | `GetStringMap`            |
| `map[string]int`                                  | `GetStringMap`            |
| `<nil>` (nil default)                             | see §1.1                  |

- The schema file (the Agent's `merge_schema.py` output at the pin, by `--schema`) must describe
  exactly the Agent binary's key set: its leaves must equal `AllKeysLowercased()` of a
  no-input config. The probe and the baseline process check this; a difference is a harness
  failure.

### 1.1 Keys with no default

A schema key has no default when the first element of `GetAllSources(key)` is missing, its
`Source` is not `default`, or its `Value` is `nil` (`%T` is `<nil>`). The Go type then says
nothing about how consumers read the key, so the getter list must come from the key's declared
schema `type` instead, by this fixed table. It maps declared types to getters; it must not infer
a Go type the way codegen does.

| Schema `type` (and element type)                        | Getters                            |
|---------------------------------------------------------|------------------------------------|
| `boolean`                                               | `Get`, `GetBool`                   |
| `integer`                                               | `Get`, `GetInt`                    |
| `number`                                                | `Get`, `GetFloat64`                |
| `string`                                                | `Get`, `GetString`                 |
| `array`, `items: string`                                | `Get`, `GetStringSlice`            |
| `array`, `items: number`                                | `Get`, `GetFloat64Slice`           |
| `array`, any other or no `items`                        | `Get`                              |
| `object`, `additionalProperties: string`                | `Get`, `GetStringMapString`        |
| `object`, `additionalProperties: array of string`       | `Get`, `GetStringMapStringSlice`   |
| `object`, any other or no `additionalProperties`        | `Get`, `GetStringMap`              |
| no `type`                                               | `Get`                              |

- `Get` comes first so the record shows the stored value next to the typed read.
- The secondary-getter tag rules above apply by declared type: `format: duration` on a `string`,
  or `golang_type: duration` on an `integer` or `number`, appends `GetDuration`.
- This is not a failure: the harness must not fail on a missing or nil default. It must fail
  only when a schema key has a non-nil default whose `%T` is not in the table above.
- The recorder's schema probe (run over every schema leaf) must report how many keys take this
  path, by row.

## 2. Getters

Only these names are allowed in a getter list. Each is a method of the Agent's config; line is
its definition in `getter.go`.

| Name                      | Line | Go return                |
|---------------------------|------|--------------------------|
| `Get`                     | 101  | `interface{}`            |
| `GetString`               | 132  | `string`                 |
| `GetBool`                 | 146  | `bool`                   |
| `GetInt`                  | 160  | `int`                    |
| `GetInt32`                | 174  | `int32`                  |
| `GetInt64`                | 188  | `int64`                  |
| `GetFloat64`              | 202  | `float64`                |
| `GetFloat64Slice`         | 216  | `[]float64`              |
| `GetDuration`             | 241  | `time.Duration`          |
| `GetStringSlice`          | 255  | `[]string`               |
| `GetStringMap`            | 269  | `map[string]interface{}` |
| `GetStringMapString`      | 283  | `map[string]string`      |
| `GetStringMapStringSlice` | 297  | `map[string][]string`    |
| `GetSizeInBytes`          | 316  | `uint`                   |

`GetSource` (`getter.go:321`) is always called and recorded separately (record.md §5.3); it must
not appear in a getter list. There is no `GetIntSlice` at the pin.

### 2.1 Explicit-only reads

Some Agent code reads a setting through something other than a typed getter. The reads below are
allowed only in an explicit `getters` list; §1 selection never picks them. Each one is a real Agent
function or method of (config, key) that returns a value §3 can encode, and each names the Agent
code that reads settings this way. No other name may be added without that citation.

| Name                | Harness call                                              | Go return                |
|---------------------|-----------------------------------------------------------|--------------------------|
| `ReadConfigSection` | `configcheck.ReadConfigSection(cfg, key).ToStringMap()`   | `map[string]interface{}` |
| `IsConfigured`      | `cfg.IsConfigured(key)` (`nodetreemodel/config.go:878`)   | `bool`                   |

`ReadConfigSection` (`comp/otelcol/otlp/configcheck/configcheck.go:18`) is how the Agent's OTLP
pipeline reads its receiver section (`comp/otelcol/otlp/config.go:49`). It reads the section with
`Get` and keeps only the leaves that are configured, plus sections the user declared with a nil
value (`configcheck_common.go:54-90`). So schema defaults do not reach the collector, and no getter
can show this.

- Its result is the nested map of `ToStringMap`, encoded by §3. An empty section is `{}`. A result
  that is not a map is a harness failure.
- The function exists only under the `otlp` build tag (`configcheck_no_otlp.go` has none). So the
  harness must be built, vetted and tested with `-tags otlp`. At the pin that tag changes the file
  set of no Agent package other than `comp/otelcol/otlp/configcheck` in the recorder's dependency
  graph. Regeneration must check this, and fail if it stops being true.

`IsConfigured` gates Agent behavior on whether the user set a key, not on its value: for example
the `dd_url`/`site` endpoints (`pkg/config/utils/endpoints.go`), the forwarder retry-queue sizes
(`comp/forwarder/defaultforwarder/impl/default_forwarder.go`) and the DogStatsD buckets
(`comp/dogstatsd/server/impl/server.go:903`). Recording it keeps Rust from re-deriving the rule
from the streamed source and value.

Warnings from both are recorded as for any getter (record.md §7).

## 3. Result encoding

The harness must encode each result with its own encoder, by the Go value's dynamic type,
recursively. It must not pass results through `float64` or `encoding/json`'s generic path.

| Go value                                   | JSON                                                        |
|--------------------------------------------|-------------------------------------------------------------|
| `nil` (untyped, or nil slice or map)       | `null`                                                      |
| `bool`                                     | `true` / `false`                                            |
| `string`                                   | JSON string; invalid UTF-8 is a harness failure             |
| `int`, `int8`…`int64`, `uint`…`uint64`     | exact decimal integer, no `.`, no exponent, any magnitude   |
| `time.Duration`                            | exact decimal integer: nanoseconds                          |
| finite `float32`, `float64`                | Go `encoding/json` float form, plus `.0` if it has no `.`, `e`, `E` |
| NaN, +Inf, −Inf                            | `{"$float":"NaN"}`, `{"$float":"+Inf"}`, `{"$float":"-Inf"}` |
| `[]T`, `[N]T`                              | array, elements encoded by this table; empty is `[]`        |
| `map[string]T`                             | object, keys sorted byte-wise, values by this table; empty is `{}` |
| `map[interface{}]interface{}`              | object, keys rendered with `fmt.Sprintf("%v")` as `sanitizeMapForJSON` does (`comp/core/configstream/impl/configstream.go:362-373`); a key collision is a harness failure |
| anything else (`[]byte`, structs, pointers)| harness failure naming `%T`                                 |

- Integers and floats are therefore always distinguishable, at any depth.
- A result map whose only key is `"$float"` is a harness failure, so the non-finite form is
  unambiguous.
- `nil` and empty stay distinct: `GetStringMap` of a missing key returns `maps.Clone(nil)`
  (`getter.go:279`), encoded `null`; `GetStringMapStringSlice` always returns a non-nil map
  (`getter.go:309-313`).

## 4. Schema type to getters (non-normative)

This table is orientation only; §1 is the rule. It shows what §1 yields for each schema shape,
given the Go default `codegen` emits for it. Keys at pin counts the vendored schema at
`lib/datadog-agent/config/schema/core/` (commit `281d921619d`).

| # | Schema type and tags                                   | Go default (`codegen` line)                   | Getters                         | Result JSON (§3)                  | Keys at pin |
|---|--------------------------------------------------------|-----------------------------------------------|---------------------------------|-----------------------------------|-------------|
| 1 | `boolean`                                              | `bool` (165-166)                              | `GetBool`                       | boolean                           | 869 |
| 2 | `integer`                                              | `int` (168-171)                               | `GetInt`                        | integer                           | 694 |
| 3 | `integer`, `golang_type:int64`                         | `int64` (169-170)                             | `GetInt64`                      | integer, exact beyond 2^53        | 22 |
| 4 | `integer`, `golang_type:duration`                      | `int` (tag ignored, 168-171)                  | `GetInt`, `GetDuration`         | integer; integer ns               | 0 |
| 5 | `number` (with or without `golang_type:float64`)       | `float64` (173-180)                           | `GetFloat64`                    | float                             | 163 |
| 6 | `number`, `golang_type:duration`                       | `float64` (tag ignored)                       | `GetFloat64`, `GetDuration`     | float; integer ns                 | 0 |
| 7 | `string`                                               | `string` (188-189)                            | `GetString`                     | string                            | 504 |
| 8 | `string`, `format: duration`, default matches the duration regex (73-104) | `time.Duration` (183-187) | `GetDuration`         | integer ns                        | 83 (80 also tagged `golang_type:duration`) |
| 9 | `string`, `format: duration`, default not a duration (for example `""`, `"10"`) | `string` (188-189) | `GetString`, `GetDuration`          | string; integer ns                | 0 |
| 10 | any, with `platform_default`                          | the value `getPlatformDefault` picks (158-160; `pkg/config/setup/config.go:1057-1069`) | by §1 from that value's type | as that row | 18 |
| 11 | `array`, `items: string`                              | `[]string` (206-207)                          | `GetStringSlice`                | array of string                   | 165 |
| 12 | `array`, `items: integer`, or tag `golang_type:[]int` | `[]int` (206-207, 216-217)                    | `Get`, `GetStringSlice`         | generic; array of string          | 1 |
| 13 | `array`, `items: number`                              | `[]float64` (206-207)                         | `GetFloat64Slice`               | array of float, or `null`         | 0 |
| 14 | `array`, no `items`                                   | `[]interface{}` (198-199, 207)                | `Get`                           | generic                           | 6 |
| 15 | `array`, `items: object` (any `additionalProperties`) | `[]map[string]<V>` (207, 209)                 | `Get`                           | generic                           | 44 |
| 16 | `object`, no `additionalProperties` (incl. `golang_type:map[string]interface{}`) | `map[string]interface{}` (209, 198-199) | `GetStringMap` | object, generic values or `null` | 3 |
| 17 | `object`, `additionalProperties: string`              | `map[string]string` (209)                     | `GetStringMapString`            | object of string, or `null`       | 23 |
| 18 | `object`, `additionalProperties: array of string`     | `map[string][]string` (209)                   | `GetStringMapStringSlice`       | object of array of string         | 18 |
| 19 | `object`, `additionalProperties: number` (incl. `golang_type:map[string]float64`) | `map[string]float64` (209) | `GetStringMap`           | object, generic values or `null`  | 1 |
| 20 | `object`, `additionalProperties: integer`             | `map[string]int` (209)                        | `GetStringMap`                  | object, generic values or `null`  | 0 |
| 21 | `object` or `array` with `boolean` element type       | none: `dict_to_gotype` has no `boolean` case (196-209) | whatever `%T` the Agent holds; a type outside §1 fails | — | 0 |
| 22 | not in schema                                         | none                                          | `Get`, unless overridden        | generic                           | — |
| 23 | section (inner node) named in `keys`                  | none                                          | `Get`                           | generic (`getter.go:81-96` builds a map) | — |

"Generic" means §3 applied to the dynamic value. For rows 4, 6, 9 and 12 the schema implies one
consumer reading and the default type implies another; both are recorded
(`schema-duration-tag-on-integer-is-int`, `stream-duration-as-nanoseconds`).

## 5. Example

The two S1/S1b results as `reads.snapshot.getters` elements:

- `additional_endpoints`: row 18, `object` with `additionalProperties: array of string`.
  `GetStringMapStringSlice` returned `map[string][]string{"https://x.test":[]string{"k"}}` (S1),
  encoded:

  ```json
  {"getter":"GetStringMapStringSlice","result":{"https://x.test":["k"]}}
  ```

- `logs_enabled`: row 1, `boolean`. `GetBool` returned `false` and logged one warning (S1b):

  ```json
  {"getter":"GetBool","result":false,"warnings":[{"level":"WARN","message":"failed to get configuration value for key \"logs_enabled\": strconv.ParseBool: parsing \"yes\": invalid syntax"}]}
  ```

A `time.Duration` of 10 s from row 8 encodes as `10000000000`; an `int64` of 2^53+1 from row 3 as
`9007199254740993`; a `float64` 1 from row 5 as `1.0`.

## 6. Decisions

1. Rows 4 and 6 stay, for any future integer or number with `golang_type:duration`. The harness
   selects getters by §1 from the default layer's Go type and the Agent schema's tags, never from
   the §4 table, which is non-normative.
2. Row 12 (`[]int`) records `Get` and `GetStringSlice`; which getter Go consumers use is an audit
   question for the fidelity audit.
3. Rows 16, 19, 20 record `GetStringMap` only; `go_type` covers the stored top-level type.
4. Counts are orientation only.
