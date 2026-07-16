This directory contains files relevant to running tests in Antithesis.

# Skills

Use the `antithesis-setup` skill to scaffold and manage this directory. Use the
`antithesis-research` skill to analyze the system and build a property
catalog. Use the `antithesis-workload` skill to implement assertions and test
commands. Use the `antithesis-launch` skill or the `test/antithesis/bin/launch.sh` wrapper
to build and submit Antithesis runs. Do not hand-type `snouty launch`.

**launch.sh**

`test/antithesis/bin/launch.sh <scenario>` builds the images, renders the compose
with concrete tags, and submits to the unified `run_test` webhook under
`antithesis.source=datadog_agent` with the node, cpu, and clock fault profile the
scenario's `launch.env` sets. `datadog_agent` is both this project's fixed source,
which selects the "Datadog Agent" customizations, and its property-history bucket.
See the script header for env overrides such as DURATION, WEBHOOK, SOURCE, and the
new `SIMULTANEOUS_FAULTS` / `FORCE_DISABLE_ALL_FAULTS` toggles.

**snouty validate**

Use this command to quickly validate changes to the Antithesis scaffolding. See
`snouty validate --help` for details.

**setup-complete.sh**

Inject this script into a Dockerfile to notify Antithesis that setup is
complete. This script should only run once the system under test is ready for
testing. Antithesis will not run any test commands until it receives this event.

**Directory layout**

- `harness/` — the shared harness Rust crate (`harness`), a member of the
  repository-root workspace. `src/lib.rs` holds shared helpers and `src/bin/`
  holds test commands reused across scenarios. Run cargo from this directory;
  it is built, fmt'd, Clippy'd, and tested from the repo root via the usual
  `make check-all` / `make test`.
- `scenarios/general/` — all Antithesis/Docker infrastructure: the `Dockerfile`,
  `docker-compose.yaml`, and per-container build inputs grouped by service
  (`scenarios/general/workload/`). This is the directory snouty consumes as
  `--config`; it contains `docker-compose.yaml` at its top. Its Cargo package
  (`antithesis-scenario-general`) owns general-only test commands under
  `src/bin/`. Snouty will push tagged images, consume this directory, and
  launch the run.
- `scenarios/differential/` — the A/B scenario that runs ADP and the Datadog
  Agent side by side against one shared sampled config and asserts they emit the
  same metric context inventory for an identical DogStatsD stream. The Agent is
  normative. Its layout mirrors `general/`: `Dockerfile`, `docker-compose.yaml`,
  per-service build inputs, and a `README.md` describing the oracle and the
  private control API. Its Cargo package (`antithesis-scenario-differential`)
  owns the differential test commands under `src/bin/`. Build and validate it
  with `make antithesis-build-differential` and `make
  antithesis-validate-differential`.

**test templates** (`scenarios/general/workload/test/`)

This directory contains test templates. A test template is a directory
containing test command executable files. Each test command must have a valid
prefix: `parallel_driver_, singleton_driver_, serial_driver_, first_,
eventually_, finally_, anytime_`. Prefixes constrain when and how commands are
composed in a single timeline. Files or subdirectories prefixed with `helper_`
are ignored by Antithesis and can be used for helper scripts kept alongside the
commands.

# Agent Behavior

Agent behavior will be governed by the following dictums:

- **The human is primary.** If you run into any confusion, pause and ask for
  clarification.
- When you are faced with a choice between doing the right, time-consuming thing
  or the wrong, fast thing do the right thing.
- Code is liability. The status quo is not worth preserving if it does not have
  utility. Be unsentimental and delete what is not needed.
- **Truth over comfort.** Say what is true regardless of the presumed comfort of
  the receiver. Do not soften findings, hedge claims or omit bad news. To do so
  is _not kindness_. It is, rather, an insidious form of lie. Note that this
  dictum should be understood less in terms of Kim Scott's "Radical Candor" -- a
  gift from the elite to the undeserving common -- but more in Walter
  Brueggemann's "Prophetic Imagination" where truth erodes a "royal
  consciousness" that ablates one's ability to do new and interesting things
  _and_ shouts a path toward those new and interesting things, against the
  status quo. Consider in this same vein Tony Hoare's "The Emperor's Old
  Clothes".
- **Honor the spirit of a request, not just its letter.** A "random string
  pool" requires actual variation. Returning `["foo", "bar"]` is technically
  a pool but a semantic mismatch. When the literal reading is unusually
  narrow or cheap, reach for the generous reading. Hostile compliance is
  worse than asking.
