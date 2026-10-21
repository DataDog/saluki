# v1.0 traces golden fixtures

These fixtures are one-off copies of the golden value test data recorded by the Go trace-agent harness. Don't edit
them: to refresh them, re-copy the files from the source and update the commit below.

## Provenance

- Repository: `DataDog/datadog-agent`
- Branch: `andrew.glaude/testDataGenerator`
- Commit: `813ec3ae95abec7ff21af48c02bbeb585515b945` (the fixtures were uncommitted working-tree files on top of this
  commit when they were copied)
- Source paths:
  - `pkg/trace/agent/testdata/cases/*.json` → `cases/`
  - `pkg/trace/agent/testdata/golden/*.json` → `golden/`
- Harness: `pkg/trace/agent/golden_v1_test.go`

The fuzz corpus (`testdata/fuzz/`) and `obfuscation_test_spans.jsonl` are not copied.

## Case spec format

Each `cases/<name>.json` file describes one scenario, and `cases/defaults.json` holds the payload defaults. Empty
`lang`, `lang_version`, `tracer_version`, `app_version`, and `env` fields fall back to the defaults.

- `name`, `comment`: the case name and a description of the pipeline branches it covers.
- `headers`: request headers that override the defaults.
- `config.install_signature`: agent install signature configuration.
- `payload`: the tracer payload in logical form (plain strings, no string table), with `chunks[].spans[]`.
- Span `start` is either a Go duration string, used as an offset from the test clock (for example, `"-2s"`), or a
  JSON integer of absolute Unix nanoseconds.
- Span `duration` is either a Go duration string (for example, `"80ms"`) or a JSON integer of nanoseconds.
- Attribute values are strings (string `AnyValue`) or numbers (double `AnyValue`).

## Golden outputs

- `golden/v1_<name>.json`: the stats payloads flushed by the concentrator, as protojson. Bucket starts are replaced by
  `"<bucket>"`.
- `golden/v1_<name>_wire.json`: the HTTP requests sent to `/api/v0.2/traces` and `/api/v0.2/stats`, with their pinned
  headers and decoded payloads. Trace string tables are sorted, and span starts are replaced by `"<start:N>"`, where
  `N` is the offset from the payload build clock rounded to 100ms.

The Go harness ran the agent with its default configuration, the hostname `agent-host`, and the API key `test`.
