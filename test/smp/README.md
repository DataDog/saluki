# SMP (Single Machine Performance) Experiments

This directory contains performance regression tests for ADP (Agent Data Plane) that run on the Single Machine Performance infrastructure.

## Overview

SMP tests measure ADP's performance characteristics under various workloads. Each experiment defines:

- **Target configuration**: How ADP is configured (environment variables, resource limits)
- **Load generation**: What traffic is sent to ADP (via [lading](https://github.com/DataDog/lading))
- **Optimization goal**: What metric to optimize for (`cpu`, `memory`, or `ingress_throughput`)
- **Quality checks**: Optional bounds on metrics (e.g., memory usage limits)

## Directory Structure

```
test/smp/regression/adp/
├── experiments.yaml          # Experiment definitions (source of truth)
├── generate_experiments.py   # Script to generate case directories
└── cases/                    # Generated experiment configurations
    └── <experiment_name>/
        ├── experiment.yaml
        ├── lading/
        │   └── lading.yaml
        └── agent-data-plane/
            └── ...
```

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

The `lading.generator` list uses type-aware merging. If both the template and experiment define a generator of the same type (e.g., `unix_datagram`), their configurations are merged:

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

## Regenerating Experiments

After modifying `experiments.yaml`, regenerate the case directories:

```bash
make generate-smp-experiments
```

To verify configurations are up-to-date (useful in CI):

```bash
make check-smp-experiments
```

## Running Experiments Locally

To run a specific experiment locally for profiling:

```bash
# First, start ADP with profiling enabled
make profile-run-adp

# Then, in another terminal, run the experiment's load generator
make profile-run-smp-experiment EXPERIMENT=dsd_uds_10mb_3k_contexts_throughput
```
