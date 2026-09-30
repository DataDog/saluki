# Contract: corpus records

The corpus, `lib/datadog-agent/config-recorder/corpus.jsonl` in saluki, is the only interface
between the Go config recorder and the Rust replay tests. This contract fixes its line types,
fields, encodings and order, so each side can be built without the other.

Agent citations are at `281d921619d`. Value encodings for getter results are in `getter-map.md`
§3; this file references them.

## 1. File

- The corpus must be UTF-8 JSON Lines: one JSON object per line, each line ending in `\n`, no
  blank lines, no BOM.
- Every line must be canonical: no insignificant whitespace; object members sorted by key in
  byte order at every depth; strings escaped as Go `encoding/json` does with `SetEscapeHTML(false)`;
  numbers written exactly as the encoder for their field produced them (the line writer must
  embed them as raw JSON and must not re-render them).
- Exception: a streamed setting's `value` (§5.1) is raw protojson bytes and keeps protojson's
  own string escaping. The line writer must embed it as `json.RawMessage` and must not re-escape
  it. The two styles differ (`encoding/json` escapes U+2028 and U+2029; protojson does not), so
  readers must parse `value` as JSON and must not compare its bytes against other members.
- Every line has a `type` member: `header`, `case` or `key`.
- Order: the header line must be first. All other lines must be sorted by the tuple
  (`case`, rank, `key`), where rank is 0 for a case line and 1 for a key line, and strings
  compare in byte order. So each case line directly precedes its key lines.
- A field this contract marks optional must be omitted when it has no value, never written as
  `null`, unless the field's text says `null` is meaningful.
- Readers must reject a corpus whose header `format` they do not know, and must reject unknown
  members. The harness and the corpus change together; any format change bumps `format`.
- Size: the corpus must be at most 512,000 bytes. Fields are optional where marked so that empty
  values cost nothing.

## 2. Header line

| Member             | JSON type        | Meaning                                                              |
|--------------------|------------------|----------------------------------------------------------------------|
| `type`             | `"header"`       |                                                                      |
| `format`           | integer          | Contract version; this document is `1`.                              |
| `agent_commit`     | string           | Full 40-hex datadog-agent commit the harness built against.          |
| `goos`, `goarch`   | string           | `runtime.GOOS`, `runtime.GOARCH` of the harness processes.           |
| `go_version`       | string           | `runtime.Version()`, for example `go1.26.7`.                                |
| `container_image`  | string           | Digest-pinned reference of the image the harness processes ran in (the Go image; codegen runs in a separate pinned image). |
| `containerized`    | boolean          | `env.IsContainerized()` in the baseline process (§3.2).              |
| `features`         | array of string  | Sorted detected features in the baseline process (§6).               |
| `inputs_digest`    | string           | `sha256:` and 64 lowercase hex: the recorder inputs the corpus was made from (below). |

`agent_commit` must equal `lib/datadog-agent/config/schema/core/_version.txt`. The driver must take it
from the Agent checkout it built (`git -C <checkout> rev-parse HEAD`, passed by a required flag),
never from `_version.txt` or Go build info, so the pin check compares two independent facts.

`inputs_digest` ties the corpus to the recorder that made it, so an edit to a case or to the
harness without regeneration fails a check that needs no Go. The inputs are the regular files
under `lib/datadog-agent/config-recorder/` anywhere below `go/` and `cases/` (recursively), plus `agent_codegen.py` and
`regenerate.sh` (which names the images and package pins), skipping any path with a component
that starts with `.`. The listing has one line per file, sorted by relative path as bytes:
`<sha256 hex of the file>  <relative path>\n` (two spaces; the `sha256sum` format; paths use `/`
and are relative to `lib/datadog-agent/config-recorder/`). The digest is the SHA-256 of the
listing. `regenerate.sh` computes it and passes it to the driver by a required flag; the Rust test
(§9.5) recomputes it.

## 3. Case line

One per case.

| Member                  | JSON type        | Meaning                                                          |
|-------------------------|------------------|------------------------------------------------------------------|
| `type`                  | `"case"`         |                                                                  |
| `case`                  | string           | Case `name`.                                                     |
| `group`                 | string           | Case `group`.                                                    |
| `why`                   | array of string  | Case `why`, in file order (`[]` if none).                        |
| `inputs`                | object           | The case inputs (§3.1).                                          |
| `containerized`         | boolean          | Optional. Written only when it differs from the header; omitted on startup failure. |
| `features`              | array of string  | Optional. Written only when it differs from the header (§6); omitted on startup failure. |
| `origin`                | string           | First snapshot's `origin`. Omitted on startup failure.           |
| `startup_error`         | string           | Optional. `err.Error()` if construction failed (§4.1).           |
| `side_effects`          | array of object  | Optional; omitted when empty. Settings that differ from the baseline (§3.2). |
| `construction_warnings` | array of warning | Optional; omitted when empty. Warnings logged before the first snapshot, filtered (§7). |
| `updates`               | array of object  | Optional; omitted when the case has no updates. One per case update, in order (§4.2). |

### 3.1 `inputs`

`inputs` holds `env`, `yaml`, `fleet_policy`, `cli`, `updates` and `keys` exactly as the case
file gave them, re-encoded as JSON:

- `env`: object, name → string. `yaml`, `fleet_policy`: strings. Each is omitted when absent.
- `cli`: array of `{"key","value"}`; `updates`: array of `{"op","key","value","source"}` with `op`
  written only for `"unset"` (absent means `"set"`) and `value` omitted for `unset`. Omitted when
  empty.
- Typed values must be written in the JSON form of case.md §7: integers as JSON integers;
  floats in Go `encoding/json` form with `.0` appended when that form has no `.`, `e` or `E`.
  Decoding `inputs` by case.md §7 must give the Go values the harness used.
- `keys`: array; each element is `{"key": k}` or `{"getters": [...], "key": k}` (the override
  only when the case gave one). `keys` is omitted when the case has no startup failure, no key
  has a `getters` override, and the case's keys are in byte order: its key lines (§5), which are
  in byte order, then list exactly the case's keys. A reader must reconstruct it from them.
- `name`, `group`, `why` are not repeated inside `inputs`. `secrets` never appears (rejected).
- A case may give a key both a YAML input and a `set` update (case.md §3.1); `inputs` then holds
  both, the key line's `reads.snapshot` records the YAML path and `reads.final` the `Set` path.

### 3.2 `side_effects`

The baseline process is a case with no inputs (empty env, empty `datadog.yaml`), run first by the
driver. It writes no case line; the header's `containerized` and `features` come from it.

- Each case process must emit its full first snapshot to the driver out of band (a file or pipe
  the driver names), never into the corpus. The baseline process does the same.
- The driver must compute `side_effects`: every setting of the case's first snapshot whose
  streamed `source`, `unset_source` or `value` encoding (§5.1) differs byte-for-byte from the
  baseline's first snapshot, excluding keys listed in the case's `keys`. A setting in the
  baseline but not in the case's snapshot is included, written as `{"absent": true, "key": k}`
  with no other member.
- Each element is a streamed setting (§5.1) plus `key`. Elements must be sorted by `key` in byte
  order. Omitted on startup failure.
- Why: key lines hold only the listed keys, so replay would otherwise build partial snapshots and
  never see the Agent-derived keys a case changes (proxy settings, `procfs_path`, the data-plane
  fix-ups).
- A reader rebuilds a case's first snapshot as three layers, each overriding the one before:
  1. the `baseline-default` case's key-line snapshots;
  2. the case's `side_effects`, where an `absent` element removes the key;
  3. the case's own key-line snapshots, where `null` removes the key.

  This gives the modeled keys plus every key the case changed. Keys that are neither modeled nor
  changed are not in the corpus. `side_effects` is not filtered to modeled keys: it records what
  the Agent streamed.

### 3.3 Consistency rules

- A case line must have exactly one of `origin` and `startup_error`.
- On startup failure `features`, `containerized`, `side_effects` and `updates` must be omitted
  (no update ran; `inputs.updates` still lists the case's updates). The harness
  must not call `env.GetDetectedFeatures()` then: it panics when detection has not run
  (`pkg/config/env/environment_detection.go:47`).
- An update's `timed_out` may be `true` only when its `seq_delta > 0` (§4.2).
- Every event's `update`, written or reconstructed (§5.2), is the index of an update in the case
  (§4.2).

## 4. Run protocol and its outcomes

### 4.1 Startup

The harness must construct the config (case.md §4), then `Subscribe` to the `configstream`
component (`comp/core/configstream/impl/configstream.go:117`) and wait up to 10 s for the first
event, which must be a snapshot.

- If construction returns an error, the harness must write the case line with `startup_error`
  and no `origin`, write no key lines for that case, and exit zero. A startup error is an outcome.
- A timeout or a non-snapshot first event is a harness failure: exit non-zero, no records.
- Only an error from the config's own construction is a startup error. An `fx` wiring error (the
  graph fails `fx.ValidateApp` on the same options) is a harness failure, and so is a
  `startup_error` in the baseline process.
- Before subscribing the harness must call no getter, since a getter read on an unknown key adds
  it to the key set (`getter-unknown-key-joins-keyset`).

### 4.2 Updates

`base` is the first snapshot's `sequence_id`. For each update *i* the harness must read
`before = cfg.GetSequenceID()` (`pkg/config/model/types.go:165`), apply it (case.md §5), and read
`after = cfg.GetSequenceID()`.

- If `after == before`, the harness must not wait and moves to update *i+1*.
- Otherwise it must receive events until one with `sequence_id - base >= after - base` has
  arrived or 5 s pass.

Events are attributed by sequence range, not by arrival time:

- An update event whose `seq` (§5.2) is in (`before - base`, `after - base`] belongs to update
  *i*.
- Nothing else may notify. The harness must check that update 0's `before` equals `base`, and
  that after the `final` reads `GetSequenceID()` equals the last update's `after` (or `base` in a
  case with no updates). Either check failing is a harness failure. At the pin only `Set`,
  `DirectBulkSet` and `UnsetForSource` move the sequence ID (`nodetreemodel/config.go:302,392,502`).
- An event whose `seq` is in no update's range, or an update event whose `key` is on no key
  line, is a harness failure (exit non-zero, no records).
- A re-sync `ConfigSnapshot` after the first snapshot is a harness failure in format 1. It needs a
  sequence gap: an update that fails to encode (no case.md §7 value does) or a full subscriber
  channel (one update at a time never fills it) (`configstream.go:145-175`). Re-syncs belong to
  the protocol tier.

Each element of the case line's `updates`, one per case update in order (element *i* is update
*i*; the number of events attributed to *i* is the count of key-line events with `update` *i*):

| Member      | JSON type        | Meaning                                                           |
|-------------|------------------|-------------------------------------------------------------------|
| `seq_delta` | integer          | `after - before`: notifications the config issued.                |
| `timed_out` | boolean          | Optional; `true` if the wait ended at 5 s. Never with `seq_delta` 0. |
| `warnings`  | array of warning | Optional; omitted when empty. Warnings logged during the `Set`/`UnsetForSource` call (§7). |


## 5. Key line

One per entry of the case's `keys`, unless startup failed.

| Member     | JSON type                  | Meaning                                                   |
|------------|----------------------------|-----------------------------------------------------------|
| `type`     | `"key"`                    |                                                           |
| `case`     | string                     | Case name.                                                |
| `key`      | string                     | The key, as written in `keys`.                            |
| `snapshot` | streamed setting or `null` | The first snapshot's setting whose `key` equals this key; `null` means absent from the snapshot. |
| `events`   | array of event             | Optional; omitted when empty. Later stream events carrying this key, in arrival order. |
| `reads`    | object                     | `{"snapshot": read}` plus `"final": read` if the case has updates (§5.3). |

### 5.1 Streamed setting

The encoding of one `pb.ConfigSetting` (`pkg/proto/datadog/model/v1/model.proto:153-161`):

| Member         | JSON type  | Meaning                                                            |
|----------------|------------|--------------------------------------------------------------------|
| `source`       | string     | `source` field, written even when `""`.                            |
| `unset_source` | string     | Optional; omitted when `""`.                                       |
| `value`        | JSON value | Optional; omitted when the proto `value` field is unset.           |

- `value` must be `protojson.Marshal` of the `google.protobuf.Value`, passed through
  `json.Compact`, embedded as raw bytes, including protojson's own string escaping (§1). protojson
  sorts `Struct` keys; its spacing is not stable, which `json.Compact` removes
  (observed during implementation). The canonical-line rules for the rest of the line must not
  re-escape these bytes, and readers must parse `value` as JSON, not compare bytes across the two
  escaping styles.
- Per streamed value, the harness must check `protojson.Unmarshal` of that output back into a
  `Value` gives bytes equal to the original under `proto.MarshalOptions{Deterministic: true}`. A
  mismatch is a harness failure (exit non-zero).
- The `key` field is not repeated. Snapshot matching is by exact string equality of `key`.

### 5.2 Event

A streamed setting (§5.1) plus:

| Member   | JSON type                 | Meaning                                                           |
|----------|---------------------------|-------------------------------------------------------------------|
| `seq`    | integer                   | Event `sequence_id - before` of its update (§4.2): the first event of an update has `seq` 1. |
| `update` | integer                   | Optional. Update index the event is attributed to (§4.2).         |

- `update` is omitted when exactly one of the case's updates has `key` equal to the key line's
  `key`, and every event on the line is attributed to that update. A reader then attributes the
  line's events to that update.
- Why relative `seq` and the omission: an absolute `seq` shifts every later event in a batch
  when a schema bump inserts a key, which turns one new key into dozens of changed lines.

- Every event is a `ConfigUpdate` (a re-sync is a harness failure, §4.2). It is listed on the key
  line whose `key` equals `setting.key`.

### 5.3 Read

The getter reads at a checkpoint. `snapshot` is taken after the first snapshot is received and
before any update; `final` after the last update's wait ends.

| Member    | JSON type        | Meaning                                                           |
|-----------|------------------|-------------------------------------------------------------------|
| `getters` | array of result  | One per getter in the key's list (`getter-map.md`), in list order.|
| `go_type` | string           | `fmt.Sprintf("%T", cfg.Get(key))`, for example `string`, `map[string]interface {}`, `<nil>`. |
| `source`  | string           | `cfg.GetSource(key).String()` (`nodetreemodel/getter.go:321`). Optional; see below. |

`source` must be omitted when it equals the corresponding streamed source, which the stream takes
from `GetSource` (`configstream.go:329`):

- `reads.snapshot.source` when it equals the key line's `snapshot.source`;
- `reads.final.source` when it equals the `source` of the key line's last event, or of
  `snapshot` if the key line has no events.

A reader must reconstruct an omitted `source` by the same rule. When the compared streamed
setting is `null` (key absent from the snapshot), `source` must be written.

A key that names a section (an inner node, such as `otlp_config.receiver`) has no streamed
setting: the stream sends leaves only. So its key line's `snapshot` is `null`, and its read
`source` is what `GetSource` returns for an inner node (`unknown` at the pin). The section's leaves
appear in the case's `side_effects` (§3.2) when they differ from the baseline.

A result is `{"getter": name, "result": <getter-map.md §3 encoding>, "warnings": [warning]}`;
`warnings` is omitted when empty.

Call order at a checkpoint must be: for each key in `keys` order, each getter in list order, then
`Get`, then `GetSource`. Only the getter calls' warnings are recorded; the Agent warns once per
unknown key (`nodetreemodel/config.go:637-645`), so the first getter call carries it.

## 6. Features

`features` must be the keys of `env.GetDetectedFeatures()` (`pkg/config/env/environment_detection.go:47`),
as their `Feature` strings (`environment_container_features.go:11-45`), sorted, read after the
first snapshot. On startup failure the harness must not call it (§3.3).

## 7. Warnings

- A warning is `{"level": L, "message": M}`: `L` is the slog level name the recorder received
  (`WARN` or `ERROR`), `M` the record's message. Records below `WARN` must not be recorded.
- Capture must flush the Agent logger (`pkglog.Flush()`) before reading, as the S1b stub does.
- The Agent logs some warnings during package initialization, before the recorder installs its
  logger (the config package's `init()` builds the schema and runs env transforms,
  `pkg/config/setup/config.go:84-87`, `nodetreemodel/config.go:736`). The Agent's logger buffers
  them and delivers them when the logger is set up. They must be kept, in delivery order, as
  construction warnings (then filtered and sorted like the rest). The synchronous-delivery check
  must count only the records that arrive after its own probe call; they must be exactly the
  probe. The buffer prefixes each message with the caller's `<file>:<line> ` (`runtime.Caller`,
  `pkg/util/log/log.go:119-131`), which names the build path, not behavior; the recorder strips
  that prefix, and only from records delivered at setup. The init-time copy and the construction
  copy of the same warning are then identical; both are kept, since the Agent logs both (its
  package `init()` and `NewAgentParams` each build the config, `override-funcs-registered-twice`).
- Getter-call warnings must be filtered to those mentioning the key. `construction_warnings` and
  update warnings must be filtered to those mentioning any key in `keys`, any update or `cli` key,
  or any `env` name. "Mentions" means the string occurs with no `[A-Za-z0-9_.]` character
  immediately before or after it. Keys match lowercased; env names as given.
- Getter-call and update warnings keep emission order. `construction_warnings` are sorted by
  `message`, then `level`, in byte order, duplicates kept: the Agent emits some of them while
  iterating Go maps (unknown YAML keys, env bindings), so their order is not stable.
- Warnings depend on the process, not only on the key. Some Agent warnings are logged once per
  process, and filtering keeps only those that mention the case's own names. So a key moved into
  a bisected part (case.md §3.1) may record different warnings than it would in its batch. A
  reader must not assume that a key's warnings are a function of the key and its input alone.

## 8. Example

The two S1/S1b outputs, as their own cases (lines wrapped here for reading; in the corpus each is
one line). Header values other than the commit and Go version are placeholders. Fields that were not
captured in S1/S1b (`features`, `construction_warnings`, `side_effects`, the
`additional_endpoints` getter's warnings) are shown as empty, which means omitted.

```json
{"agent_commit":"281d921619d52ce7b99aef40607285992c9c2e89","container_image":"golang@sha256:e30143be198ab04cf7ba25fba83ab3a692ca584c994aad0bf131fa0eb32dd8c1","containerized":false,"features":[],"format":1,"go_version":"go1.26.7","goarch":"arm64","goos":"linux","inputs_digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","type":"header"}
{"case":"additional-endpoints-env","group":"behavior",
 "inputs":{"env":{"DD_ADDITIONAL_ENDPOINTS":"{\"https://x.test\": [\"k\"]}"}},
 "origin":"datadog.yaml","type":"case","why":["env-map-raw-string"]}
{"case":"additional-endpoints-env","key":"additional_endpoints",
 "reads":{"snapshot":{"getters":[{"getter":"GetStringMapStringSlice","result":{"https://x.test":["k"]}}],"go_type":"string"}},
 "snapshot":{"source":"environment-variable","value":"{\"https://x.test\": [\"k\"]}"},"type":"key"}
{"case":"logs-enabled-yes-yaml","group":"behavior",
 "inputs":{"yaml":"logs_enabled: \"yes\"\n"},
 "origin":"datadog.yaml","type":"case","why":["getter-bool-string-strict-parsebool","yaml-type-mismatch-scalar-leaf-keeps-raw"]}
{"case":"logs-enabled-yes-yaml","key":"logs_enabled",
 "reads":{"snapshot":{"getters":[{"getter":"GetBool","result":false,"warnings":[{"level":"WARN","message":"failed to get configuration value for key \"logs_enabled\": strconv.ParseBool: parsing \"yes\": invalid syntax"}]}],"go_type":"string"}},
 "snapshot":{"source":"file","value":"yes"},"type":"key"}
```

Provenance: streamed values and sources are the S1 and S1b output lines. Both reads omit
`source` (§5.3): `GetSource` for `additional_endpoints` is S1's, `environment-variable`, equal
to the snapshot's; for `logs_enabled` it is the stream's source, which the stream takes from
`GetSource` (`configstream.go:329`). `go_type` `string` follows from the streamed
string value, which `sanitizeValue` would not produce from a map (`configstream.go:396-409`).

## 8a. Readings settled during implementation

1. Warning filter (§7): the key is lowercased before searching; the message is matched as-is,
   case-sensitively. Env names are matched as given.
2. (Retired with `update: null`: every event now belongs to an update, §4.2.)
3. What a single-case process hands the driver is an internal result file, never corpus lines;
   only the driver writes corpus lines (§3.2).

## 9. Decisions

1. `containerized` and `features`: the header holds the baseline process's values (§3.2); a
   case line writes them only when they differ, and never on startup failure.
2. Read checkpoints stay at two (`snapshot`, `final`). A protocol case that needs a typed read
   after an intermediate update is split into several cases.
3. Warning determinism: a two-run byte-identity check decides. If warning text is not
   stable, the harness records the warning with the unstable part replaced only if the Agent
   provides a stable form; otherwise the case is reported for review, not normalized
   silently.
4. The 5 s wait is accepted; it costs seconds only for updates that issued a notification whose
   event is dropped, since an update with `after == before` is not waited on (§4.2).
5. The size cap and the pin check are a Rust test in the `datadog-agent-config` crate (it owns
   `_version.txt`), so ordinary CI checks them without Go. The same test recomputes
   `inputs_digest` (§2) and fails when it differs, naming `make build-agent-config-corpus`.
   It also checks the corpus against the overlay (case.md §3.2): the keys of the `breadth` and
   `unsupported` case lines equal the modeled and unsupported schema keys, and every `excluded`
   key is still excluded. The overlay is not a digest input, so an overlay edit that changes no
   key's group needs no regeneration.
