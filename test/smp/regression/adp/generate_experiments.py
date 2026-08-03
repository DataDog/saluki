#!/usr/bin/env python3
"""
Generate SMP experiment configuration files from experiments.yaml.

This script reads the experiment definitions from experiments.yaml and generates one
target-config directory per suite (see SUITES): the full superset under full/ and the PR
gating subset under quality-gates/, each holding a copy of config.yaml and a cases/ tree.

Usage:
    python generate_experiments.py          # Generate experiment files
    python generate_experiments.py --check  # Verify files are up-to-date (for CI)
"""

import argparse
import copy
import os
import re
import shutil
import sys
import tempfile
from pathlib import Path

import yaml

SCRIPT_DIR = Path(__file__).parent
EXPERIMENTS_FILE = SCRIPT_DIR / "experiments.yaml"
CONFIG_FILE = SCRIPT_DIR / "config.yaml"

# Legacy single-suite output directory (used before the split into per-suite directories
# below). It is removed when regenerating and flagged by --check so it can't silently linger.
LEGACY_CASES_DIR = SCRIPT_DIR / "cases"

# Experiments are materialized into one or more "suites", each of which is a self-contained SMP
# target-config directory (a copied config.yaml plus a cases/ tree). A suite's predicate decides,
# from the raw experiment definition, whether the experiment belongs to that suite:
#   - "full": every experiment. This is the nightly / on-demand superset.
#   - "quality-gates": only experiments that declare `checks:`. This is the PR gating subset; an
#     experiment's bound *is* its gate, so the presence of `checks` is the classifier.
# A gating experiment is written, byte-for-byte identically, into both suites.
SUITES = {
    "full": lambda experiment: True,
    "quality-gates": lambda experiment: "checks" in experiment,
}

# Mapping from optimization goal to directory name suffix
GOAL_SUFFIXES = {
    "ingress_throughput": "throughput",
    "memory": "memory",
    "cpu": "cpu",
}

# Strings that YAML parsers interpret as booleans or null
YAML_BOOLEAN_LIKE = frozenset(
    ("true", "false", "yes", "no", "on", "off", "null", "~", "none")
)


def get_generator_type(generator_item: dict) -> str | None:
    """
    Get the type key from a generator item.

    Generator items are dicts with a single key indicating the type,
    for example, {"unix_datagram": {...}}, {"grpc": {...}}, {"http": {...}}
    """
    if isinstance(generator_item, dict) and len(generator_item) == 1:
        return next(iter(generator_item.keys()))
    return None


def merge_generator_lists(base_generators: list, overlay_generators: list) -> list:
    """
    Merge two generator lists by matching on generator type.

    If an overlay generator has the same type as a base generator, the configs
    are deep-merged. Otherwise, generators are appended.
    """
    if not base_generators:
        return copy.deepcopy(overlay_generators)
    # An explicit empty overlay list means "replace with no generators" (for example, no-traffic experiments).
    if overlay_generators is not None and len(overlay_generators) == 0:
        return []
    if not overlay_generators:
        return copy.deepcopy(base_generators)

    # Build a map of base generators by type
    result = []
    base_by_type = {}
    for i, gen in enumerate(base_generators):
        gen_type = get_generator_type(gen)
        if gen_type:
            base_by_type[gen_type] = i
        result.append(copy.deepcopy(gen))

    # Process overlay generators
    for overlay_gen in overlay_generators:
        overlay_type = get_generator_type(overlay_gen)
        if overlay_type and overlay_type in base_by_type:
            # Merge with existing generator of same type
            idx = base_by_type[overlay_type]
            result[idx] = {
                overlay_type: deep_merge(
                    result[idx][overlay_type], overlay_gen[overlay_type]
                )
            }
        else:
            # Append new generator
            result.append(copy.deepcopy(overlay_gen))

    return result


def deep_merge(base: dict, overlay: dict, path: tuple = ()) -> dict:
    """
    Recursively merge overlay into base.

    - For dicts: merge recursively
    - For lists: replace entirely (no merge), except for lading.generator
      which uses type-aware merging
    - None values in overlay remove keys from base
    """
    result = copy.deepcopy(base)

    for key, value in overlay.items():
        current_path = path + (key,)

        if value is None:
            # None removes the key
            result.pop(key, None)
        elif (
            key in result and isinstance(result[key], dict) and isinstance(value, dict)
        ):
            # Recursively merge dicts
            result[key] = deep_merge(result[key], value, current_path)
        elif (
            current_path == ("lading", "generator")
            and isinstance(result.get(key), list)
            and isinstance(value, list)
        ):
            # Special case: merge generator lists by type
            result[key] = merge_generator_lists(result[key], value)
        else:
            # Replace value (including lists)
            result[key] = copy.deepcopy(value)

    return result


def resolve_template_chain(
    templates: dict, template_name: str, seen: set = None
) -> dict:
    """Resolve a template and its inheritance chain."""
    if seen is None:
        seen = set()

    if template_name in seen:
        raise ValueError(f"Circular template inheritance detected: {template_name}")

    seen.add(template_name)

    if template_name not in templates:
        raise ValueError(f"Unknown template: {template_name}")

    template = templates[template_name]

    # If this template extends another, resolve that first
    if "extends" in template:
        parent_name = template["extends"]
        parent = resolve_template_chain(templates, parent_name, seen)
        # Remove extends from template before merging
        template_copy = {k: v for k, v in template.items() if k != "extends"}
        return deep_merge(parent, template_copy)

    return copy.deepcopy(template)


def resolve_experiment(experiment: dict, global_config: dict, templates: dict) -> dict:
    """
    Resolve an experiment's full configuration by applying inheritance.

    Order: global -> template (if extends) -> experiment
    """
    # Start with global config
    result = copy.deepcopy(global_config)

    # Apply template if specified
    if "extends" in experiment:
        template_name = experiment["extends"]
        template = resolve_template_chain(templates, template_name)
        result = deep_merge(result, template)

    # Apply experiment-specific config (excluding 'name', 'extends', and 'optimization_goals')
    experiment_config = {
        k: v
        for k, v in experiment.items()
        if k not in ("name", "extends", "optimization_goals")
    }
    result = deep_merge(result, experiment_config)

    return result


def expand_optimization_goals(experiment: dict) -> list[tuple[str, str]]:
    """
    Expand an experiment's optimization goals into (name, goal) pairs.

    If 'optimization_goals' (plural) is specified, generates multiple variants
    with suffixed names. Otherwise, uses the single 'optimization_goal'.

    Returns a list of (experiment_name, optimization_goal) tuples.
    """
    base_name = experiment["name"]

    # Check for plural 'optimization_goals' first
    if "optimization_goals" in experiment:
        goals = experiment["optimization_goals"]
        if not isinstance(goals, list) or not goals:
            raise ValueError(
                f"optimization_goals must be a non-empty list in experiment '{base_name}'"
            )

        expanded = []
        for goal in goals:
            suffix = GOAL_SUFFIXES.get(goal, goal)
            expanded.append((f"{base_name}_{suffix}", goal))
        return expanded

    # Fall back to singular 'optimization_goal'
    if "optimization_goal" in experiment:
        return [(base_name, experiment["optimization_goal"])]

    # No optimization goal specified - will inherit from template/global
    return [(base_name, None)]


def build_experiment_yaml(config: dict) -> dict:
    """Build the experiment.yaml content from resolved config."""
    # Copy target config but exclude 'files' which is only used for file generation
    target = {k: v for k, v in config["target"].items() if k != "files"}

    experiment = {
        "optimization_goal": config["optimization_goal"],
        "erratic": config.get("erratic", False),
        "target": target,
    }

    if "checks" in config:
        experiment["checks"] = config["checks"]

    experiment["report_links"] = config["report_links"]

    return experiment


def build_lading_yaml(config: dict) -> dict:
    """Build the lading.yaml content from resolved config."""
    lading_config = config.get("lading", {})
    return {
        k: v
        for k, v in lading_config.items()
        if k in ("generator", "blackhole", "target_metrics")
    }


class YamlDumper(yaml.SafeDumper):
    """Custom YAML dumper for consistent output formatting."""

    pass


def needs_double_quotes(data: str) -> bool:
    """
    Determine if a string value needs double quotes to avoid YAML ambiguity.

    Returns True for strings that would be parsed as non-strings without quotes:
    - Empty strings
    - Boolean-like values (true, false, yes, no, etc.)
    - Numeric-like values (integers, floats, hex, octal)
    - Strings starting with special YAML characters
    """
    if not data:
        return True  # Empty string needs quotes

    # Strings that look like YAML booleans or null
    if data.lower() in YAML_BOOLEAN_LIKE:
        return True

    # Strings that could be parsed as numbers (int or float)
    try:
        float(data)
        return True
    except ValueError:
        pass

    # Strings that could be parsed as octal/hex
    if re.match(r"^0[xXoO]?[0-9a-fA-F]+$", data):
        return True

    # Strings that start with special YAML characters
    if data[0] in (
        "!",
        "&",
        "*",
        "{",
        "}",
        "[",
        "]",
        "|",
        ">",
        "%",
        "@",
        "`",
        '"',
        "'",
    ):
        return True

    # Single special characters that have YAML meaning
    if data in (":", "-", "?"):
        return True

    return False


def str_representer(dumper: yaml.Dumper, data: str) -> yaml.ScalarNode:
    """
    Represent strings with appropriate quoting style.

    - Multi-line strings use literal block style (|)
    - Strings needing quotes use double quotes
    - Unambiguous strings are left unquoted
    """
    if "\n" in data:
        return dumper.represent_scalar("tag:yaml.org,2002:str", data, style="|")

    style = '"' if needs_double_quotes(data) else None
    return dumper.represent_scalar("tag:yaml.org,2002:str", data, style=style)


def list_representer(dumper: yaml.Dumper, data: list) -> yaml.SequenceNode:
    """Use flow style for short lists of primitives (like seeds)."""
    # Use flow style for lists of numbers (like seed arrays)
    if data and all(isinstance(item, (int, float)) for item in data):
        return dumper.represent_sequence("tag:yaml.org,2002:seq", data, flow_style=True)
    return dumper.represent_sequence("tag:yaml.org,2002:seq", data, flow_style=False)


YamlDumper.add_representer(str, str_representer)
YamlDumper.add_representer(list, list_representer)


def dump_yaml(data: dict) -> str:
    """Dump dict to YAML string with consistent formatting."""
    return yaml.dump(
        data,
        Dumper=YamlDumper,
        default_flow_style=False,
        sort_keys=False,
        allow_unicode=True,
        width=120,
    )


def write_target_files(target_dir: Path, files_config: dict, base_path: Path) -> None:
    """
    Write target configuration files.

    Args:
        target_dir: Directory to write files to (for example, cases/exp/agent-data-plane/)
        files_config: Dict mapping filename to file spec (content or source)
        base_path: Base path for resolving relative source paths (directory containing experiments.yaml)
    """
    for filename, file_spec in files_config.items():
        file_path = target_dir / filename

        if "source" in file_spec:
            # Copy source file
            source_path = base_path / file_spec["source"]
            if not source_path.exists():
                raise ValueError(f"Source file not found: {source_path}")

            shutil.copy2(source_path, file_path)

        elif "content" in file_spec:
            # Write content directly
            content = file_spec["content"]
            if isinstance(content, str):
                # String content - write as-is, ensure trailing newline
                if not content.endswith("\n"):
                    content += "\n"
                file_path.write_text(content)
            else:
                # Dict/list content - serialize as YAML
                file_path.write_text(dump_yaml(content))
        else:
            raise ValueError(
                f"File spec for '{filename}' must have 'content' or 'source'"
            )


def write_experiment(
    name: str, config: dict, output_dir: Path, base_path: Path
) -> None:
    """Write the experiment files to the output directory."""
    experiment_dir = output_dir / name
    experiment_dir.mkdir(parents=True, exist_ok=True)

    # Write experiment.yaml
    experiment_yaml = build_experiment_yaml(config)
    (experiment_dir / "experiment.yaml").write_text(dump_yaml(experiment_yaml))

    # Write lading/lading.yaml
    lading_dir = experiment_dir / "lading"
    lading_dir.mkdir(exist_ok=True)
    lading_yaml = build_lading_yaml(config)
    (lading_dir / "lading.yaml").write_text(dump_yaml(lading_yaml))

    # Write target directory files (for example, agent-data-plane/)
    target_name = config["target"]["name"]
    target_dir = experiment_dir / target_name
    target_dir.mkdir(exist_ok=True)

    # Get files config, defaulting to empty.yaml with "{}" content
    files_config = config.get("target", {}).get(
        "files", {"empty.yaml": {"content": "{}"}}
    )
    write_target_files(target_dir, files_config, base_path)


def generate_experiments(
    config: dict, base_dir: Path, base_path: Path
) -> dict[str, list[str]]:
    """Generate all experiment files into per-suite directories under base_dir.

    For each suite, writes a cases/ tree plus a copy of config.yaml into base_dir/<suite>/, and
    returns a mapping of suite name to the list of experiment (variant) names written into it.
    base_path is the source root used to resolve `source:` files and the shared config.yaml.
    """
    global_config = config.get("global", {})
    templates = config.get("templates", {})
    experiments = config.get("experiments", [])

    generated = {suite: [] for suite in SUITES}

    for experiment in experiments:
        # Resolve the base experiment config (without optimization goal).
        resolved_base = resolve_experiment(experiment, global_config, templates)

        # Determine which suites this experiment belongs to.
        suites = [suite for suite, in_suite in SUITES.items() if in_suite(experiment)]

        # Expand optimization goals into variants, writing each into every matching suite.
        for name, goal in expand_optimization_goals(experiment):
            resolved = copy.deepcopy(resolved_base)
            if goal is not None:
                resolved["optimization_goal"] = goal
            for suite in suites:
                write_experiment(name, resolved, base_dir / suite / "cases", base_path)
                generated[suite].append(name)

    # Copy the shared SMP config.yaml into each suite's target-config directory.
    for suite in SUITES:
        suite_dir = base_dir / suite
        suite_dir.mkdir(parents=True, exist_ok=True)
        shutil.copy2(base_path / CONFIG_FILE.name, suite_dir / "config.yaml")

    return generated


def load_config(config_path: Path) -> dict:
    """Load and parse the experiments.yaml file."""
    with open(config_path) as f:
        return yaml.safe_load(f)


def compare_experiment_files(generated_dir: Path, existing_dir: Path) -> list[str]:
    """Compare generated experiment files against existing ones.

    Returns a list of difference descriptions, empty if files match.
    """
    differences = []

    for file_path in generated_dir.rglob("*"):
        if not file_path.is_file():
            continue

        rel_path = file_path.relative_to(generated_dir)
        existing_file = existing_dir / rel_path

        if not existing_file.exists():
            differences.append(f"Missing file: {existing_file}")
        elif existing_file.read_text() != file_path.read_text():
            differences.append(f"Content differs: {existing_file}")

    return differences


def check_experiments(config: dict) -> bool:
    """Check if generated experiments match existing files.

    Returns True if all files are up-to-date, False otherwise.
    """
    with tempfile.TemporaryDirectory() as tmpdir:
        tmp_path = Path(tmpdir)
        generated = generate_experiments(config, tmp_path, SCRIPT_DIR)

        differences = []
        total = 0
        for suite, names in generated.items():
            total += len(names)
            suite_existing = SCRIPT_DIR / suite
            suite_tmp = tmp_path / suite

            if not suite_existing.exists():
                differences.append(f"Missing directory: {suite_existing}")
                continue

            # Compare every generated file under the suite (config.yaml + the cases/ tree).
            differences.extend(compare_experiment_files(suite_tmp, suite_existing))

            # Flag any case directories on disk that the config no longer generates.
            existing_cases = suite_existing / "cases"
            if existing_cases.exists():
                existing_dirs = {d.name for d in existing_cases.iterdir() if d.is_dir()}
                for extra in existing_dirs - set(names):
                    differences.append(
                        f"Extra directory not in config: {existing_cases / extra}"
                    )

        # The pre-split cases/ directory must not linger after regeneration.
        if LEGACY_CASES_DIR.exists():
            differences.append(f"Legacy directory should be removed: {LEGACY_CASES_DIR}")

        if differences:
            print("SMP experiment files are out of date:", file=sys.stderr)
            for diff in differences:
                print(f"  - {diff}", file=sys.stderr)
            print(
                "\nRun 'make generate-smp-experiments' to regenerate.",
                file=sys.stderr,
            )
            return False

        print(
            f"All {total} experiment configurations across {len(generated)} suites "
            "are up-to-date."
        )
        return True


def main():
    parser = argparse.ArgumentParser(
        description="Generate SMP experiment configuration files from experiments.yaml"
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="Check if generated files match existing files (for CI)",
    )
    args = parser.parse_args()

    if not EXPERIMENTS_FILE.exists():
        print(f"Error: {EXPERIMENTS_FILE} not found", file=sys.stderr)
        sys.exit(1)

    if not CONFIG_FILE.exists():
        print(f"Error: {CONFIG_FILE} not found", file=sys.stderr)
        sys.exit(1)

    config = load_config(EXPERIMENTS_FILE)

    if args.check:
        if not check_experiments(config):
            sys.exit(1)
    else:
        # Clear existing suite directories (and the pre-split cases/ dir) and regenerate.
        for suite in SUITES:
            suite_dir = SCRIPT_DIR / suite
            if suite_dir.exists():
                shutil.rmtree(suite_dir)
        if LEGACY_CASES_DIR.exists():
            shutil.rmtree(LEGACY_CASES_DIR)

        generated = generate_experiments(config, SCRIPT_DIR, SCRIPT_DIR)
        total = sum(len(names) for names in generated.values())
        print(f"Generated {total} experiment configurations:")
        for suite in sorted(generated):
            print(f"  {suite}/ ({len(generated[suite])}):")
            for name in sorted(generated[suite]):
                print(f"    - {name}")


if __name__ == "__main__":
    main()
