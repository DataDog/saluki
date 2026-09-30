# Config recorder

The config recorder saves examples of how the Datadog Agent handles configuration. Agent Data
Plane (ADP) receives values from the Agent but reads them in Rust, where conversions can differ
from Go. Saved Agent results give compatibility tests a reference without running Go in Rust CI.

The recorder is a Go program built inside a Datadog Agent checkout. A **case** supplies inputs
such as environment variables, `datadog.yaml`, fleet policy, CLI overrides, and runtime updates.
The recorder runs the Agent's own configuration code and saves two views:

- The **config stream**: values and their sources sent to ADP, first as a snapshot and then as
  update events.
- **Getter results**: values returned by configuration methods such as `GetInt` and `GetStringMap`.
  These methods convert stored values to the types used by Agent components.

The saved collection of cases is the **corpus**, [`corpus.jsonl`](corpus.jsonl). It contains one
header line, then a case line and per-setting lines for each case. It is generated; do not edit
it by hand.

The [Rust reader](../config-corpus/README.md), `datadog-agent-config-corpus`, loads these records
for compatibility tests. Its unit tests validate the recordings' format, size, coverage, and
consistency with the current schema and recorder inputs. These checks run in CI without Go or
Docker; they do not compare ADP's behavior with the Agent's.

[`docs/comparison.md`](docs/comparison.md) fixes how agent-data-plane's typed values are compared with recorded getter results.

## Section reads

Some Agent code reads a setting through something other than a typed getter. A case names one of
these explicit-only reads (`docs/getter-map.md` §2.1) in a key's `getters` list; default getter selection
never picks them.

The Agent's OTLP pipeline does not read its receiver settings with getters. It reads the whole
`otlp_config.receiver` section with `configcheck.ReadConfigSection`, which keeps only the leaves
the user configured and the sections the user declared with a nil value, so schema defaults never
reach the collector. The function exists only under the Agent's `otlp` build tag, so
`regenerate.sh` vets, tests and builds the recorder with `-tags otlp`. That tag selects no file
under `pkg/config`, `comp/core/config` or `comp/core/configstream`, so it does not change how the
config is built or streamed.

`IsConfigured` reports whether the user set a key, not what its value is: the Agent uses it to
gate behavior such as the `dd_url`/`site` endpoints and the forwarder retry-queue sizes. A case
names `IsConfigured` in a key's `getters` list to record that answer directly, instead of Rust
re-deriving the rule from the streamed source and value.

## Generated cases

Besides the hand-written cases in `cases/`, `regenerate.sh` runs the recorder's `generate`
subcommand. It reads the merged Agent schema and saluki's `schema_overlay.yaml`, and writes the
`baseline`, `breadth`, `unsupported`, `excluded`, `unknown` and `depth` cases into
`target/config-recorder/generated-cases/` (emptied on every run, never checked in). `drive` then
records both directories.

- `baseline` is one case with no inputs, holding every modeled key.
- Other groups batch keys by top-level schema section. Top-level leaves (no dot) are batched by
  first character, as section `top-<c>`. A section with more than 40 keys is split by its second
  path component. A case name depends only on key paths, so an added or removed key usually
  changes only its own batch, except when it moves a section across the 40-key split threshold,
  which redistributes that section's other keys into new batch names too.
- Every value comes from a fixed rule on the key's schema type, default and format, so two runs
  write the same cases.
- `Case::check_origin` in the Rust reader removes generated batch and split suffixes from case
  names. Paired with a setting, checkpoint (snapshot or final read), and getter, it identifies a
  recorded read even when a schema change moves that setting into another batch. A unit test
  checks that these identities are unique.
- `depth` records the input shapes that break readers: empty, null, wrong-shape and alternate
  spellings. For YAML and `set` inputs, modeled keys fall into classes by default-layer Go type,
  and for env inputs by Go type and schema `env_parser`. Each class contributes one
  representative setting, pinned by review in `go/gen/depth_reps.go` rather than picked by key
  order, so a schema bump that adds an earlier-sorting key moves no variant's rows; a pin that no
  longer matches the schema fails generation with what to update. There is one case per input
  shape, `depth-<source>-<shape>`, holding every class the shape applies to, plus
  `depth-env-secondary-name` for keys with more than one env name. Each key records its default
  getters plus `Get`. The generator writes every shape; a corpus test fails when the group's lines
  pass 80,000 bytes, and lists the bytes per shape so a person can cut shapes from case.md's table.

## Splitting batches

`drive` records a batch as it ran only when every key streams the source its inputs set. Otherwise
it splits the batch, its *root*, by the rules of case.md §3.1:

- When the root starts, each key that is not clean is *peeled* into a single-key part
  `<root>--<key>`, and the rest reruns under the root's name until it is clean or empty.
- When the root fails to start, `drive` halves it in key order to find the single keys that fail
  alone. It records each one's failed run as its part and reruns the rest. When no single key
  fails alone, every key becomes a part.

`baseline` and `behavior` cases, and single-key cases, are never split.

The drive work directory, `target/config-recorder/work/`, keeps what each process used and saw:

- `run/<root>/<i>-<name>/`: the `<i>`-th process of a root, counted from 1. It holds the case file
  `<name>.yaml`, the process's work directory `work/` and its result `result.gob`.
- `snapshots/<name>.jsonl`: the first snapshot of each recorded case, from the run that was
  recorded, and `_baseline.jsonl` for the baseline.
- `snapshots/discarded/<root>/<i>-<name>.jsonl`: the first snapshot of every other run.

## Regenerating the corpus

```sh
make build-agent-config-corpus
```

Regenerate after you bump `lib/datadog-agent/config/schema/core/_version.txt`, or after you change
the recorder (`go/`) or the cases. The run is deterministic: a second run gives a byte-identical
corpus.

Bumping the schema pin (`_version.txt`) also requires:

1. Re-checking the corpus reader's lists of sources, getters and groups
   (`lib/datadog-agent/config-corpus/src/lists.rs`) against `pkg/config/model/types.go` at the new
   pin.
2. Bumping `REVIEWED_AT_AGENT_COMMIT`, the constant at the top of that same file, to the new pin.
3. Regenerating the corpus and running the checks below; `corpus_pin_matches_vendored_schema`
   fails separately on a stale corpus and on a `REVIEWED_AT_AGENT_COMMIT` that still names the old
   pin, so a schema bump that forgets step 2 is caught even after regeneration.

## Adding a case

A hand-written case (group `behavior`) lives in [`cases/`](cases/) as `<name>.yaml`. Its required
fields, fixed by `docs/case.md` and enforced by `go/record/case.go:192-255,310-325`, are `name`
(matching the file stem), `group`, `why` (non-empty for `behavior`) and `keys` (non-empty). A
minimal example:

```yaml
# Shows: an env var whose value is a raw JSON-encoded map, read as a typed getter.
name: additional-endpoints-env
group: behavior
why: [env-map-raw-string]
env:
  DD_ADDITIONAL_ENDPOINTS: '{"https://x.test": ["k"]}'
keys: [additional_endpoints]
```

- Name the file `<name>.yaml`; `ParseCaseFile` rejects a file whose stem does not equal `name`.
- Give the file a top-of-file comment (as above) saying what the case shows, since the ids in
  `why` name entries in a catalog kept outside this repository.
- After adding or editing a case, run `make build-agent-config-corpus` to regenerate the corpus,
  then run the checks below.

The target runs [`regenerate.sh`](regenerate.sh), which:

1. reads the Agent commit from `_version.txt`;
2. fetches that commit into `target/config-recorder/datadog-agent`, or reuses the checkout when it
   is already there;
3. copies `go/` into the checkout as `cmd/config-recorder/`;
4. runs the Agent's schema codegen and core schema merge in a pinned Python image;
5. runs `gofmt`, `go vet`, `go test`, `go build` and the recorder in a pinned Go image;
6. replaces `corpus.jsonl` only if every step succeeded.

### Host requirements

- Docker, git, make and bash. Go and Python are not needed on the host.
- The containers run on `linux/arm64` whatever the host, so there is one canonical corpus until an
  amd64 run proves the records byte-identical. To choose another platform, set
  `CONFIG_RECORDER_PLATFORM` (for example `linux/amd64`); it is passed to every `docker run` as
  `--platform`. Both images are pinned to multi-arch indexes. On an amd64 host, install QEMU `binfmt`
  support first:

  ```sh
  docker run --privileged --rm tonistiigi/binfmt --install arm64
  ```

- Network access for a cold run:
  - `github.com`, to fetch the Agent commit on the host, outside any container
    (`AGENT_REPO_URL` can override it; `regenerate.sh` never passes it into a container);
  - the Go module proxy (`GOPROXY`; when set on the host, the script passes it into the Go
    container with `-e GOPROXY`);
  - PyPI, for the Python packages `uv` installs;
  - the image registries: Docker Hub (the Go image) and `ghcr.io` (the Python image).

### Caches

- `target/config-recorder/`: the Agent checkout and the merged schema.
- Docker volumes `saluki-config-recorder-gomod` (Go modules), `saluki-config-recorder-gobuild` (Go
  build cache) and `saluki-config-recorder-uv` (Python packages).

Run one regeneration at a time: concurrent runs share the checkout and the volumes.

To start clean, remove them:

```sh
rm -rf target/config-recorder
docker volume rm saluki-config-recorder-gomod saluki-config-recorder-gobuild saluki-config-recorder-uv
```

## Checks

The `datadog-agent-config-corpus` crate checks the committed corpus with no Go and no Docker, as
unit tests of that crate; CI runs them, along with every other Rust unit test, through `make test`.
When a check fails because the corpus is stale, regenerate it with `make build-agent-config-corpus`.

- `corpus_lines_read_strictly`: an independent strict reader of the whole file. It catches a corpus
  that breaks the record format: encoding, line order, canonical JSON, unknown or `null` members,
  bad values, and the consistency rules between lines.
- `corpus_within_size_cap`: the file is over 512,000 bytes.
- `depth_group_within_budget`: the `depth` group's corpus lines are over their 80,000-byte budget;
  see `docs/case.md` §3.2.1 for which variants to cut.
- `corpus_pin_matches_vendored_schema`: the header's `agent_commit` is not `_version.txt`, or
  `REVIEWED_AT_AGENT_COMMIT` (`lib/datadog-agent/config-corpus/src/lists.rs`) is not `_version.txt`.
- `corpus_inputs_digest_is_current`: `go/`, `cases/`, `agent_codegen.py` or `regenerate.sh` changed
  since the corpus was recorded.
- `corpus_groups_match_overlay`: the keys of the generated case groups no longer match the vendored
  schema and `schema_overlay.yaml`, for example after a key's `support` changed.
- `corpus_env_cases_stream_env_source`: a key in an env-only `breadth` or `unsupported` case did
  not stream the source `environment-variable`, so its env name is not the Agent's.
- `vendored_schema_has_no_unrecorded_shapes`: the vendored schema grew a shape (a deprecated key
  name, a duration-tagged integer or number) that no case covers yet; add the case the failure
  names, in `cases/`, before regenerating.

```sh
cargo nextest run --lib --bins -p datadog-agent-config-corpus
```

`regenerate.sh` also stops when `lib/datadog-agent/config/schema/core/` (without `_version.txt`)
differs from the pinned checkout's `pkg/config/schema/yaml/`. To record anyway, set
`CONFIG_RECORDER_ALLOW_SCHEMA_DIFF=1`; it then warns and carries on. A comparison that cannot run
(a missing directory, an unreadable file) always stops it.
