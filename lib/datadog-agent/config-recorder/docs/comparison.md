# Contract: comparison rules

This contract fixes how a typed agent-data-plane configuration leaf is compared with a getter result
the corpus recorded (record.md §5.3, getter-map.md §3). The rules are per (leaf kind, getter), never
per key. The implementation is `lib/agent-data-plane-config-system/src/corpus_replay/compare.rs`.

## 1. Scope

- Compared: one `LeafValue` read from `DatadogConfiguration` through `LEAVES` against one recorded
  `GetterRead` of the same key, at one checkpoint (`reads.snapshot` or `reads.final`).
- Not compared here: values agent-data-plane computes from settings, such as its stop timeout or a
  byte size parsed from a string. The derived tier (`derived.rs`) compares each one against the Agent
  getter that computes the same value: `GetInt` on `data_plane.stop_timeout`, and `GetSizeInBytes` on
  a byte-size key. A leaf that matches as a string can still differ as the value ADP uses.
- Not compared: validation of the translated configuration. Validation rejects only a blank
  `api_key`, and most cases set none. A step whose translation succeeds is therefore applied, and its
  validation failure is recorded as a `system` line. Later steps describe the configuration
  agent-data-plane would hold if the key were set.

## 2. Leaf kinds

| Kind            | Rust type borrowed                  |
|-----------------|-------------------------------------|
| `Bool`          | `bool`                              |
| `Duration`      | `std::time::Duration`               |
| `F64`           | `f64`                               |
| `I64`           | `i64`                               |
| `JsonList`      | `&[serde_json::Value]`              |
| `OptionI64`     | `Option<i64>`                       |
| `OptionStr`     | `Option<&str>`                      |
| `Str`           | `&str`                              |
| `StringList`    | `&[String]`                         |
| `StringListMap` | `&HashMap<String, Vec<String>>`     |
| `StringMap`     | `&HashMap<String, String>`          |
| `StringMapList` | `&[HashMap<String, String>]`        |

## 3. Emulation

Each kind stands for the getter an Agent consumer of that type calls. A key may record several
getters (getter-map.md §4 rows 4, 6, 9, 12, and every §1.1 key, which records `Get` first); only the
emulated getter is compared, and each other getter must give `NotCompared`.

| Kind            | Emulated getter            | Why (getter-map.md §4)                                         |
|-----------------|----------------------------|----------------------------------------------------------------|
| `Bool`          | `GetBool`                  | row 1                                                          |
| `Duration`      | `GetDuration`              | row 8; for row 9 the `GetString` read is not compared          |
| `F64`           | `GetFloat64`               | row 5; for row 6 the `GetDuration` read is not compared        |
| `I64`           | `GetInt`, `GetInt64`       | rows 2 and 3; for row 4 the `GetDuration` read is not compared |
| `JsonList`      | `Get`                      | rows 14 and 15: `Get` is the only getter                       |
| `OptionI64`     | `GetInt`                   | row 2 and §1.1 `integer`: the typed read                       |
| `OptionStr`     | `GetString`                | row 7 and §1.1 `string`: the typed read                        |
| `Str`           | `GetString`                | row 7                                                          |
| `StringList`    | `GetStringSlice`           | row 11; for row 12 the `Get` read is not compared              |
| `StringListMap` | `GetStringMapStringSlice`  | row 18                                                         |
| `StringMap`     | `GetStringMapString`       | row 17                                                         |
| `StringMapList` | `Get`                      | row 15: `Get` is the only getter                               |

`I64` stands for two getters because both return the same Go integer, encoded the same way, and §1
gives a key at most one of them.

## 4. Rules

Each rule reads the result as getter-map.md §3 encodes it and compares exactly. A rule must not
round, trim, fold case, reorder a list, or coerce between integer and float.

| Kind            | Getter                    | Rule                                                                                    |
|-----------------|---------------------------|-----------------------------------------------------------------------------------------|
| `Bool`          | `GetBool`                 | equal booleans                                                                          |
| `Duration`      | `GetDuration`             | the leaf's whole nanoseconds equal the integer; a negative result never matches         |
| `F64`           | `GetFloat64`              | finite: equal bits, so `-0.0` differs from `0.0`; NaN matches NaN; each infinity itself |
| `I64`           | `GetInt`, `GetInt64`      | equal integers                                                                          |
| `JsonList`      | `Get`                     | a list of equal length, elements equal in order by the generic rule below; a top-level `null` matches `[]` (decision 6) |
| `OptionI64`     | `GetInt`                  | `Some(n)` matches `n`; `None` matches `0` (decision 5)                                  |
| `OptionStr`     | `GetString`               | `Some(s)` matches `s`; `None` matches `""` (decision 5)                                 |
| `Str`           | `GetString`               | equal strings, byte for byte                                                            |
| `StringList`    | `GetStringSlice`          | equal lists in order; `null` (nil slice) matches `[]` (decision 6)                      |
| `StringListMap` | `GetStringMapStringSlice` | equal key sets, each value an equal list; a `null` value matches `[]` (decision 6)      |
| `StringMap`     | `GetStringMapString`      | equal key sets and values; `null` (nil map) matches `{}` (decision 6)                   |
| `StringMapList` | `Get`                     | a list of equal length, each element a map of equal keys whose values are Go strings; a top-level `null` matches `[]` |

The generic rule, for a `serde_json::Value` against a `Get` value:

- `null`, booleans and strings match their own kind with an equal value.
- A JSON integer matches only a Go integer with the same value; a JSON float matches only a finite
  Go float with the same bits. So `1` and `1.0` differ at any depth.
- A non-finite Go float matches nothing, because a JSON value cannot hold one.
- Lists compare in order; maps compare as key sets, and key order never matters.

## 5. Verdicts

| Verdict       | Meaning                                                                                   |
|---------------|-------------------------------------------------------------------------------------------|
| `Match`       | The rule holds.                                                                           |
| `Differs`     | The rule fails. Both sides must be rendered exactly: every digit of a float, every element of a list, maps in key order. |
| `AdpRejects`  | `DatadogConfiguration` failed to deserialize, so there is no leaf. The replay produces it, not a rule. |
| `NotCompared` | No rule applies: the getter is not the emulated one, the getter is explicit-only (§6), or the result shape is not the getter's. |

`NotCompared` must carry its reason, and a replay must count every `NotCompared` by reason. It must
never be dropped or treated as `Match`. A pair with no rule must give `NotCompared`, never a guess.

## 6. Explicit-only reads

`ReadConfigSection` and `IsConfigured` (getter-map.md §2.1) must give `NotCompared`.

- `ReadConfigSection` returns only the leaves a user configured. agent-data-plane reads the OTLP
  receiver leaves one by one through typed readers that fill schema defaults
  (`datadog_translator.rs`), so no single leaf holds what the section map holds. Comparing the
  section needs the source tree's provenance, which is not part of a `LeafValue`.
- `IsConfigured` is a property of the key's sources, not of its value. agent-data-plane keeps it
  as provenance in its source tree, not in the typed leaf.

Both belong to a provenance tier, not to this one.

### 6.1 Recorded getters with no leaf rule

| Getter            | Where it is recorded                         | What compares it                                                    |
|-------------------|----------------------------------------------|---------------------------------------------------------------------|
| `GetSizeInBytes`  | byte-size keys such as `log_file_max_size`   | the derived tier, against the byte count ADP translates             |
| `GetFloat64Slice` | `histogram_percentiles`                      | nothing yet: the leaf is a string list that ADP parses later, and no rule emulates that parse |
| `GetStringMap`    | map keys, after `GetStringMapString`         | nothing: `StringMap` emulates `GetStringMapString` (decision 2)     |
| `ReadConfigSection`, `IsConfigured` | section and source probes  | nothing: explicit-only (§6)                                          |

A case that records only these getters has no compared verdict. Its `case` line in the known results
shows zero matches and zero differences, so a reviewer can see the gap.

## 7. Not observable from the stream

A section set to `null` in YAML, for example `otlp_config.receiver.protocols.grpc: null`, streams
nothing, because the stream sends leaves only (record.md §5.3). agent-data-plane therefore receives
no setting under that section, and its typed leaves under it hold their schema defaults, exactly as
if the section were absent. The Agent's `ReadConfigSection` keeps the declared section. The corpus
can't show this difference, so these rules can't catch it.

## 8. Decisions

| # | Choice point               | Options                                                  | Choice                          | Reason |
|---|----------------------------|----------------------------------------------------------|---------------------------------|--------|
| 1 | Rule granularity           | per key; per (kind, getter)                              | per (kind, getter)              | Fixes are made per type. |
| 2 | Several recorded getters   | compare all; compare the emulated one                    | emulated one, others `NotCompared` | A kind has one Rust type, which one consumer type reads. |
| 3 | `I64` getters              | `GetInt` only; `GetInt` and `GetInt64`                   | both                            | Same Go integer and encoding; otherwise row 3 keys are never compared. |
| 4 | Option kinds               | emulate `Get`; emulate the typed getter                  | typed getter                    | `Get` holds the stored value, which may be a string from an environment variable; the typed getter is what consumers read. |
| 5 | `None` against `0` or `""` | match; differ                                            | match                           | The typed getter cannot say "unset": Agent code sees `0` or `""` whether the key is unset or set to it, so no difference Agent code can observe is hidden. Whether ADP code treats `None` as the Agent treats the zero value is a question for the translation tier, not the leaf. |
| 6 | nil against empty list or map, at the top level of a result or of a map value | match; differ | match | Go code sees no difference through `len`, `range` or indexing. It can test `== nil`, but ADP's `Vec` and `HashMap` cannot hold nil, so no ADP value could match a nil result; whether a consumer branches on nil is a question for that consumer, not for the leaf. When ADP rejects a streamed `null`, the verdict is `AdpRejects`, so that failure stays visible. A `null` element inside a generic value stays strict. getter-map.md §3 keeps nil and empty distinct in the record so this rule is a choice made here, not in the corpus. |
| 7 | Float equality             | numeric `==`; bit equality                               | bit equality                    | Exact, and `-0.0` against `0.0` is a real difference. |
| 8 | NaN and infinities         | never match; match by name                               | match by name                   | The corpus encodes them without a payload. |
| 9 | Durations                  | seconds; nanoseconds                                     | whole nanoseconds               | `GetDuration` is nanoseconds; 1 ns off must differ. |
| 10 | Integer against float in generic values | numeric value; kind and value              | kind and value                  | getter-map.md §3 keeps them distinct at every depth. |
| 11 | Map key order             | significant; ignored                                     | ignored                         | Go maps are unordered; the corpus sorts keys. |
| 12 | List order                | significant; ignored                                     | significant                     | Slices are ordered in Go and in Rust. |
| 13 | Explicit-only reads       | compare against a leaf; `NotCompared`                    | `NotCompared`                   | Their results describe sources, not a leaf's value (§6). |

| 14 | Byte sizes                | compare the string leaf only; also compare the parsed size | both: the leaf here, the size in the derived tier | ADP parses the string itself, so equal strings can give different byte counts. |

No normalization is allowed beyond decisions 5, 6, 8 and 11.

## 9. Adding a leaf kind or a getter

1. A new leaf kind: add it to the generator (`lib/datadog-agent/config/build/witness_gen.rs`), which
   writes `LEAVES` and `LeafValue`, and to the tables in §2, §3 and §4.
2. A new rule: implement it in `compare.rs` for the (kind, getter) pair, with unit tests that
   cover a match, a difference, and a result shape the getter cannot return.
3. A new derived value: add a row to `DERIVATIONS` in `derived.rs`, or give the reason it is not
   replayed in `NOT_REPLAYED`.
4. Record the getter: add a case that names it (case.md), and regenerate the corpus.
5. Regenerate the known results (the command is in the file's header). Give each new divergence
   line a divergence type: an existing one, or a new `type` line.
