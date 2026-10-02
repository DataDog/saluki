# Datadog Agent configuration records

This Rust library reads saved examples of Datadog Agent configuration behavior.
It is test support, not part of the running Agent Data Plane (ADP).

ADP receives configuration from the Agent, but its Rust configuration types do not always read
values the way the Agent's Go code does. For example, an environment variable can reach ADP as
a string even when the Agent reads it as a map. Tests need the Agent's actual results, not our
assumptions about them.

## Where the records come from

The [config recorder](../config-recorder/README.md) is a Go program that runs the Agent's own
configuration code. Each **case** supplies inputs, such as YAML, environment variables, or
runtime updates, and names the settings to observe.

For each case, the recorder saves two views:

- The **config stream**: values and their sources that the Agent sends to ADP. A snapshot holds
  the starting values; later events report changes.
- **Getter results**: values returned by Agent methods such as `GetInt("dogstatsd_port")` or
  `GetStringMap("additional_endpoints")`. These methods convert stored values to Go types.

The **corpus** is the collection of recordings in
[`corpus.jsonl`](../config-recorder/corpus.jsonl). It is checked into this repository so Rust
tests can read it without running Go or Docker. Recording uses the Agent commit named in the
[vendored schema's version file](../config/schema/core/_version.txt).

## What this library does

`read(&bytes)` parses the file into a `Corpus`, containing the recorded inputs, stream values,
and getter results. It rejects malformed records and reports their line numbers. It
reconstructs fields omitted to save space.

The parser preserves distinctions that tests need: integers versus floats, Go `nil` versus
empty collections, and streamed values versus values returned by getters. It does not run the
Agent, imitate its conversions, or decide whether ADP agrees with it. Compatibility tests can use the parsed records
for that comparison.

The parser follows the [record format](../config-recorder/docs/record.md), rather than sharing
the Go writer's implementation. This avoids copying writer bugs into both sides.

## Checks and maintenance

The crate's unit tests check the parser with small examples and validate the checked-in corpus.
They catch stale recordings by comparing the recorded Agent commit, hashes of the recorder's
inputs, and covered settings with the current schema and ADP support inventory. They also
check the size limit and sources recorded for environment-only cases.

From the repository root, run:

```sh
cargo nextest run --lib -p datadog-agent-config-corpus
```

CI runs these checks with the other Rust unit tests. Neither Go nor Docker is needed.

After changing the Agent schema pin, recorder, or cases, regenerate the recordings:

```sh
make build-agent-config-corpus
```

Regeneration needs Docker and runs Go. Do not edit `corpus.jsonl` by hand. See the
[recorder instructions](../config-recorder/README.md#regenerating-the-corpus) for schema-update
steps, prerequisites, and caches.

## Source layout

- `src/lib.rs`: public types and the `read` entry point.
- `src/reader.rs`: record validation and reconstruction of omitted fields.
- `src/json.rs`: JSON parsing that preserves number text, string escaping, and member order
  for format checks.
- `src/getter.rs`: decoding of recorded Go return values into Rust types.
- `src/model.rs`: cases, inputs, stream settings, results, and errors.
- `src/lists.rs`: allowed source, getter, group, and warning-level names.
- `src/tests.rs`: parser tests with small inline recordings.
- `src/corpus_checks.rs`: checks against the checked-in recordings and schema.
