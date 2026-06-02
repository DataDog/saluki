This directory contains files relevant to running tests in Antithesis.

# Skills

Use the `antithesis-setup` skill to scaffold and manage this directory. Use the
`antithesis-research` skill to analyze the system and build a property
catalog. Use the `antithesis-workload` skill to implement assertions and test
commands. Use the `antithesis-launch` skill to build, validate, and submit
Antithesis runs — do not run `snouty launch` directly.

**snouty launch**

Use `snouty launch --json --webhook basic_test --config test/antithesis/deploy`
to start an Antithesis run. Always run `compose build` first to ensure images
are up to date.

**snouty validate**

Use this command to quickly validate changes to the Antithesis scaffolding. See
`snouty validate --help` for details.

**setup-complete.sh**

Inject this script into a Dockerfile to notify Antithesis that setup is
complete. This script should only run once the system under test is ready for
testing. Antithesis will not run any test commands until it receives this event.

**Directory layout**

- `harness/` — the harness Rust crate (`harness`), a member of the
  repository-root workspace. `src/lib.rs` holds shared helpers; each
  `src/bin/*.rs` is an Antithesis test command named after its file. Run cargo
  from this directory; it is built, fmt'd, Clippy'd, and tested from the repo
  root via the usual `make check-all` / `make test`.
- `deploy/` — all Antithesis/Docker infrastructure: the `Dockerfile`,
  `docker-compose.yaml`, and per-container build inputs grouped by service
  (`deploy/adp/`, `deploy/workload/`). This is the directory snouty consumes as
  `--config`; it contains `docker-compose.yaml` at its top. Snouty will push
  tagged images, consume this directory, and launch the run.

**scratchbook**

This directory is the Antithesis scratchbook for the codebase. It contains
documents such as system analysis, property catalogs, topology plans,
per-property evidence files (in `scratchbook/properties/`), property
relationship maps, and other persistent integration notes. Keep it up to date as
Antithesis-related decisions change.

**test templates** (`deploy/workload/test/`)

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
