# SMP (Single Machine Performance) Experiments

This directory contains performance regression tests for ADP (Agent Data Plane) that run on the Single Machine Performance infrastructure.

## Overview

SMP tests measure ADP's performance characteristics under various workloads. Each experiment defines:

- **Target configuration**: How ADP is configured (environment variables, resource limits)
- **Load generation**: What traffic is sent to ADP (via [lading](https://github.com/DataDog/lading))
- **Optimization goal**: What metric to optimize for (`cpu`, `memory`, or `ingress_throughput`)
- **Quality checks**: Optional bounds on metrics (for example, memory usage limits)

## The target image

SMP fixes one target image per job, so every experiment runs against the same one: the converged
Datadog Agent image (`docker/Dockerfile.datadog-agent`), built in CI from that side's ADP build.
An experiment picks how much of it to use through `target.command`:

- Most experiments run ADP by itself, in standalone mode, by pointing the command straight at the
  ADP binary. The Core Agent never starts.
- The tag filtering experiments run the image's own entrypoint, so the Core Agent comes up and
  supervises ADP. They need it: a `metric_tag_filterlist` is Core Agent configuration, and reaches
  ADP over the config stream.

Profiling is SMP's to arrange: the SMP experiment runner loads `ddprof` into the target with `LD_PRELOAD` on the
replicas named by `ddprof_replicas` in `config.yaml`.

## Directory Structure

```
test/smp/regression/adp/
├── experiments.yaml          # Experiment definitions (source of truth)
├── config.yaml               # Shared SMP config (copied into each suite below)
├── generate_experiments.py   # Script to generate the per-suite case directories
├── shared/                   # Files pulled into experiments at generation time: copied verbatim
│                             # (cert.pem) or rendered if they are Jinja templates (*.j2)
├── quality-gates/            # PR gating suite (experiments with `checks:`)
│   ├── config.yaml
│   └── cases/
│       └── <experiment_name>/
│           ├── experiment.yaml
│           ├── lading/lading.yaml
│           └── agent-data-plane/...
└── full/                     # Nightly / on-demand suite (all experiments; a superset)
    ├── config.yaml
    └── cases/
        └── <experiment_name>/...
```

> [!NOTE]
> `quality-gates/` and `full/` are generated; never edit them by hand. Edit `experiments.yaml`
> and run `make generate-smp-experiments`.

## Suites

Experiments are generated into two suites, each a self-contained SMP target-config directory:

- **`quality-gates/`** — the PR gate. Contains only experiments that declare `checks:` (those
  whose bounds define whether a PR is suitable to merge). CI runs this suite on every PR and
  fails the pipeline if a bound is breached.
- **`full/`** — the superset of *all* experiments (including the quality gates). CI runs this
  suite nightly on `main` (summarizing to Slack) for long-term trend analysis, and on-demand as
  a manual job on a PR (reporting to the PR). The full suite never gates a PR.

An experiment's membership is derived automatically: it joins `quality-gates/` if and only if it
declares `checks:`. There is no separate flag to maintain — the bound *is* the gate. Define each
experiment once in `experiments.yaml`; the generator writes the gating experiments, identically,
into both suite directories.

## Defining Experiments

All experiments are defined in `experiments.yaml`. The file has three sections:

### Global Configuration

Settings inherited by all experiments:

```yaml
global:
  erratic: false
  target:
    name: agent-data-plane
    cpu_allotment: 4
    memory_allotment: 2GiB
    # ... shared environment variables, profiling settings
  report_links:
    # ... dashboard links
  lading:
    blackhole:
      # ... default sink configuration
    target_metrics:
      # ... metrics scraping configuration
```

### Templates

Reusable partial configurations that experiments can extend:

```yaml
templates:
  dsd_base:
    target:
      environment:
        DD_DATA_PLANE_DOGSTATSD_ENABLED: "true"
        # ... DogStatsD-specific settings
    lading:
      generator:
        - unix_datagram:
            # ... base generator config

  otlp_base:
    target:
      environment:
        DD_DATA_PLANE_OTLP_ENABLED: "true"
        # ... OTLP-specific settings
```

### Experiments

Individual experiment definitions:

```yaml
experiments:
  - name: my_experiment
    extends: dsd_base                    # Inherit from template
    optimization_goal: memory            # Single goal
    # or
    optimization_goals: [cpu, memory, ingress_throughput]  # Multiple goals
    
    target:
      environment:
        # Experiment-specific overrides
    
    checks:                              # Optional quality gates
      - name: memory_usage
        bounds:
          series: total_rss_bytes
          upper_bound: "100.0 MiB"
    
    lading:
      generator:
        - unix_datagram:
            bytes_per_second: "10 MiB"   # Merged with template's generator
```

## Configuration Inheritance

Configuration is merged in order: `global` → `template` → `experiment`

- **Dictionaries** are deep-merged (experiment values override inherited values)
- **Lists** are replaced entirely, except for `lading.generator` which merges by generator type
- **`null` values** remove inherited keys

### Generator Merging

The `lading.generator` list uses type-aware merging. If both the template and experiment define a generator of the same type (for example, `unix_datagram`), their configurations are merged:

```yaml
# Template defines full generator config
templates:
  dsd_base:
    lading:
      generator:
        - unix_datagram:
            seed: [2, 3, 5, ...]
            path: /tmp/adp-dogstatsd-dgram.sock
            variant:
              dogstatsd:
                # ... full variant config
            maximum_prebuild_cache_size_bytes: "500 Mb"

# Experiment only overrides what differs
experiments:
  - name: my_experiment
    extends: dsd_base
    lading:
      generator:
        - unix_datagram:
            bytes_per_second: "10 MiB"   # Merged into template config
```

## Optimization Goals

Use `optimization_goal` (singular) for a single goal, or `optimization_goals` (plural) to generate multiple experiment variants:

```yaml
# Single goal - generates: my_experiment/
- name: my_experiment
  optimization_goal: memory

# Multiple goals - generates: my_experiment_cpu/, my_experiment_memory/, my_experiment_throughput/
- name: my_experiment
  optimization_goals: [cpu, memory, ingress_throughput]
```

## Custom Target Files

Experiments can specify custom files to place in the target directory (named after `target.name`, for example, `agent-data-plane/`). Files can be specified with inline content or copied from shared files.

### Inline Content

Write content directly to a file:

```yaml
target:
  files:
    empty.yaml:
      content: "{}"
    
    # YAML content is automatically serialized
    config.yaml:
      content:
        some_key: some_value
        nested:
          key: value
```

### Copying Shared Files

Copy files from a source path relative to `experiments.yaml`:

```yaml
target:
  files:
    cert.pem:
      source: shared/cert.pem    # Relative to experiments.yaml
```

A source ending in `.j2` is rendered as a Jinja template rather than copied. See
[Variables and Templating](#variables-and-templating).

### Default Behavior

If no `files` are specified, a default `empty.yaml` with `{}` content is created. Files are inherited from global/templates and merged with experiment-specific files (experiment values take precedence on conflict).

## Variables and Templating

Some experiments need structure that is impractical to write out by hand — a
`metric_tag_filterlist` with thousands of entries, say. Rather than special-casing that in the
generator, an experiment can declare `variables:` and use Jinja to build what it needs.

`variables:` is inherited and merged like everything else (`global` → `template` → `experiment`),
so a template can hold the expressions while each experiment supplies the numbers. It is an input
to generation, not part of the output. Templates see it as `exp_vars`:

```yaml
templates:
  my_base:
    variables:
      metric_prefix: example.metric.
      metric_first: 10000
      metric_count: 0            # overridden per experiment

experiments:
  - name: my_experiment
    extends: my_base
    variables:
      metric_count: 500
```

Templating applies in two places, both fed by that one set of variables:

- **Any string in the experiment configuration.** Values are rendered after inheritance is
  resolved, so `"{{ exp_vars.metric_count }}"` works in an environment variable, a lading setting,
  or inline file content. An unknown variable is an error, not an empty string.
- **Target files whose `source:` ends in `.j2`.** The file is rendered as a Jinja template instead
  of being copied, which is where loops belong. The result is checked for valid YAML if the target
  filename is a YAML file.

`report_links` is left alone: SMP does its own `{{ job_id }}` substitution in those.

### Helpers

Two functions are available in every template:

| Helper | Purpose |
| --- | --- |
| `lading_range(first, count)` | Renders a lading range pattern, for example `lading_range(10000, 500)` → `{{10000-10499}}`. |
| `lading_names(prefix, first, count)` | Expands the same range into the list of names lading produces, padding included. |

Use `lading_range` rather than writing a pattern out. Jinja and lading share `{{ ... }}`
delimiters, so a literal `{{0-499}}` would be evaluated as arithmetic during generation and never
reach lading. Generation rejects literal patterns and points here.

`lading_names` is the other half: it produces the same strings for the places that need them
written out in full, so both sides of an experiment agree. Because lading pads numeric ranges to
the width of the range's *end*, take subsets by slicing the full range
(`lading_names(prefix, 0, pool)[:subset]`) rather than by generating a shorter one, which would
pad differently.

### Tag filtering, as an example

The tag filtering experiments use all of this. `variables:` fixes the corpora and the filterlist
size, the lading generator draws from patterns built with `lading_range`, and
`shared/tagfilter-datadog.yaml.j2` loops over `lading_names` to build the filterlist the Core
Agent streams to ADP. One set of numbers drives both, so the filterlist can't end up naming
metrics the generator never sends — which is a silent failure, since the experiment still runs
green while measuring nothing.

> [!NOTE]
> The rendered `datadog.yaml` files are large — the 10,000-entry variants are ~12 MiB each. That
> is the configuration under test, and it compresses to well under a megabyte in git, but it is
> worth knowing before adding another size variant.

## Regenerating Experiments

After modifying `experiments.yaml`, regenerate the case directories:

```bash
make generate-smp-experiments
```

To verify configurations are up-to-date (useful in CI):

```bash
make check-smp-experiments
```

Both targets run the generator with the local virtualenv at `.venv` when there is one, and fall
back to `python3` otherwise, as CI does — its build image pre-installs the same pinned
`requirements.txt`. Set the virtualenv up, or update it after that file changes, with
`make ensure-python-venv`.

## Running Experiments Locally

To run a specific experiment locally for profiling:

```bash
# First, start ADP with profiling enabled
make profile-run-adp

# Then, in another terminal, run the experiment's load generator
make profile-run-smp-experiment EXPERIMENT=dsd_uds_10mb_3k_contexts_throughput
```

> [!NOTE]
> This drives an ADP process you started yourself, so it only covers the experiments that run ADP
> standalone. Running a tag filtering experiment locally means running the converged image
> (`make build-datadog-agent-image-release`) with that case's `datadog.yaml`, and pointing lading
> at it by hand.
