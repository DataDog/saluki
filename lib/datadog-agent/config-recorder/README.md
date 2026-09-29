# Config recorder

The config recorder is a small Go program built inside a Datadog Agent checkout. For each case in
[`cases/`](cases/) it builds the Agent's configuration through the Agent's own code (environment,
`datadog.yaml`, fleet policy, CLI overrides, runtime updates), and records what the Agent streamed
and what its getters returned.

The output is [`corpus.jsonl`](corpus.jsonl): one header line, then a case line and key lines per
case. The Rust tests replay it to check that Saluki reads configuration the way the Agent does. It
is generated; do not edit it by hand.

## Generated cases

Besides the hand-written cases in `cases/`, `regenerate.sh` runs the recorder's `generate`
subcommand. It reads the merged Agent schema and saluki's `schema_overlay.yaml`, and writes the
`baseline`, `breadth`, `unsupported`, `excluded` and `unknown` cases into
`target/config-recorder/generated-cases/` (emptied on every run, never checked in). `drive` then
records both directories.

- `baseline` is one case with no inputs, holding every modeled key.
- Other groups batch keys by top-level schema section. Top-level leaves (no dot) are batched by
  first character, as section `top-<c>`. A section with more than 40 keys is split by its second
  path component. A case name depends only on key paths, so a new key changes only its own batch.
- Every value comes from a fixed rule on the key's schema type, default and format, so two runs
  write the same cases.

## Splitting batches

`drive` records a batch as it ran only when every key streams the source its inputs set. Otherwise
it splits the batch, its *root*, by the rules of case.md §3.1:

- When the root starts, each key that is not clean is *peeled* into a single-key part
  `<root>--<key>`, and the rest reruns under the root's name until it is clean or empty.
- When the root fails to start, `drive` halves it in key order to find the single keys that fail
  alone. It records each one's failed run as its part and reruns the rest. When no single key
  fails alone, every key becomes a part.

`baseline` and `behavior` cases, and single-key cases, are never split.

The drive workdir, `target/config-recorder/work/`, keeps what each process used and saw:

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
- The images are `linux/arm64`. On an amd64 host, install QEMU binfmt support first:

  ```sh
  docker run --privileged --rm tonistiigi/binfmt --install arm64
  ```

- Network access for a cold run:
  - `github.com`, to fetch the Agent commit (`AGENT_REPO_URL`, overridable);
  - the Go module proxy (`GOPROXY`; when set on the host, the script passes it through to the Go
    container, the same way it already does `AGENT_REPO_URL`);
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
