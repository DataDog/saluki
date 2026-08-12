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
import json
import os
import re
import shutil
import sys
import tempfile
from pathlib import Path

import jinja2
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

# Suffix marking a target file `source:` as a Jinja template rather than a file to copy verbatim.
JINJA_TEMPLATE_SUFFIX = ".j2"

# Loader used to check that rendered templates still parse. PyYAML's pure-Python parser dominates
# the runtime of this script when a template renders megabytes of configuration -- around 25
# seconds for a 12 MiB filterlist, against 4 with libyaml -- so use libyaml's parser wherever the
# runtime was built with it, and fall back to the pure-Python one where it wasn't.
SafeLoader = getattr(yaml, "CSafeLoader", yaml.SafeLoader)

# Keys of the resolved configuration that are handed to SMP with their own `{{ ... }}` templating
# intact, and so must not be rendered here. SMP substitutes the job ID, experiment name and time
# range into report links itself, long after we're done.
UNRENDERED_KEYS = frozenset(("report_links",))

# Maximum number of strings lading expands a pattern pool into. Beyond this it silently truncates
# (`.take(max_expansions)` in its `StringListPool`), so `lading_range` refuses to go over.
LADING_MAX_EXPANSIONS = 15_000

# A lading range pattern written out literally, for example `{{0-499}}`. Jinja shares lading's
# delimiters and would evaluate one of these as arithmetic before lading ever saw it, so we reject
# them and point at `lading_range` instead.
LITERAL_LADING_RANGE = re.compile(
    r"\{\{\s*(?:\d+\s*-\s*\d+|[A-Za-z]\s*-\s*[A-Za-z])\s*\}\}"
)

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


def range_bounds(first: int, count: int) -> tuple[int, int, int]:
    """Resolve a `first`/`count` range into its last value and the width lading renders it at.

    lading left-pads numeric range patterns to the width of the range's end, so every name it
    expands from one pattern is the same length. Anything we generate that has to line up with
    those names has to pad identically, which is why both helpers below share this.
    """
    if count < 1:
        raise ValueError(f"range count must be at least 1, got {count}")

    last = first + count - 1
    return first, last, len(str(last))


def lading_range(first: int, count: int) -> str:
    """Render a lading range pattern, for example, `lading_range(10000, 500)` -> `{{10000-10499}}`.

    Use this instead of writing the pattern out: Jinja and lading share `{{ ... }}` delimiters, so
    a literal pattern is evaluated as arithmetic during generation and never reaches lading.
    """
    first, last, _ = range_bounds(first, count)
    if count > LADING_MAX_EXPANSIONS:
        raise ValueError(
            f"lading_range({first}, {count}) exceeds lading's {LADING_MAX_EXPANSIONS} expansion "
            "limit, which it enforces by silently truncating the pool"
        )

    return f"{{{{{first}-{last}}}}}"


def lading_names(prefix: str, first: int, count: int) -> list[str]:
    """Expand a range into the list of names lading would generate for the matching pattern.

    This is the other half of `lading_range`: it produces the same strings, padded the same way,
    for the places we have to write them out in full rather than hand a pattern to lading. Slice
    the result to take a subset, so the padding still follows the full range's width.
    """
    first, last, width = range_bounds(first, count)
    return [f"{prefix}{value:0{width}d}" for value in range(first, last + 1)]


def build_jinja_env(base_path: Path) -> jinja2.Environment:
    """Build the Jinja environment that renders experiment values and target file templates."""
    env = jinja2.Environment(
        loader=jinja2.FileSystemLoader(base_path),
        undefined=jinja2.StrictUndefined,
        keep_trailing_newline=True,
        trim_blocks=True,
        lstrip_blocks=True,
    )
    env.globals["lading_range"] = lading_range
    env.globals["lading_names"] = lading_names

    return env


def render_value(value: str, env: jinja2.Environment, variables: dict) -> str:
    """Render one string through Jinja, with the experiment's variables in scope."""
    if LITERAL_LADING_RANGE.search(value):
        raise ValueError(
            f"{value!r} contains a literal lading range pattern. Jinja shares lading's "
            "`{{ ... }}` delimiters and evaluates it as arithmetic during generation, so lading "
            "never sees the pattern. Use `{{ lading_range(first, count) }}` instead."
        )

    return env.from_string(value).render(exp_vars=variables)


def render_config(config: dict, env: jinja2.Environment, variables: dict) -> dict:
    """Render every Jinja expression in a resolved experiment configuration.

    Strings anywhere in the configuration may reference `exp_vars` and the helpers above, which is
    what keeps a template's expressions and an experiment's numbers in one place. Keys in
    UNRENDERED_KEYS are passed through untouched.
    """

    def render(value):
        if isinstance(value, dict):
            return {key: render(item) for key, item in value.items()}
        if isinstance(value, list):
            return [render(item) for item in value]
        if isinstance(value, str) and "{{" in value:
            return render_value(value, env, variables)
        return value

    return {
        key: value if key in UNRENDERED_KEYS else render(value)
        for key, value in config.items()
    }


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


def render_target_template(
    source: str,
    filename: str,
    env: jinja2.Environment,
    variables: dict,
    cache: dict,
) -> str:
    """Render a target file from a Jinja template, and check that it still parses as YAML.

    A template is how a file gets structure that would be unreadable written out by hand, such as
    a `metric_tag_filterlist` with thousands of entries. Rendering it here rather than at run time
    means a broken template fails generation instead of an SMP job twenty minutes in.

    Experiments routinely share a rendering -- an idle variant and a traffic variant of the same
    scenario differ in their load generation, not in their target configuration -- and both
    rendering and parsing megabytes of YAML is the bulk of this script's runtime, so results are
    cached against the template and the variables that produced them.
    """
    key = (source, json.dumps(variables, sort_keys=True, default=str))
    if key in cache:
        return cache[key]

    rendered = env.get_template(source).render(exp_vars=variables)

    if filename.endswith((".yaml", ".yml")):
        try:
            # Scan the event stream rather than loading the document: the check is for a template
            # that renders malformed YAML, and every syntax error surfaces during parsing, without
            # paying to build Python objects out of megabytes of configuration. The one thing this
            # gives up is resolving aliases, which these templates deliberately never emit.
            for _ in yaml.parse(rendered, Loader=SafeLoader):
                pass
        except yaml.YAMLError as error:
            raise ValueError(
                f"Template '{source}' rendered invalid YAML for '{filename}': {error}"
            ) from error

    cache[key] = rendered
    return rendered


def write_target_files(
    target_dir: Path,
    files_config: dict,
    base_path: Path,
    env: jinja2.Environment,
    variables: dict,
    render_cache: dict,
) -> None:
    """
    Write target configuration files.

    Args:
        target_dir: Directory to write files to (for example, cases/exp/agent-data-plane/)
        files_config: Dict mapping filename to file spec (content or source)
        base_path: Base path for resolving relative source paths (directory containing experiments.yaml)
        env: Jinja environment used to render `.j2` sources
        variables: The experiment's `variables`, exposed to templates as `exp_vars`
        render_cache: Renderings shared across experiments, keyed by template and variables
    """
    for filename, file_spec in files_config.items():
        file_path = target_dir / filename

        if "source" in file_spec:
            source = file_spec["source"]
            source_path = base_path / source
            if not source_path.exists():
                raise ValueError(f"Source file not found: {source_path}")

            if source_path.suffix == JINJA_TEMPLATE_SUFFIX:
                # Render the template rather than copying it verbatim.
                file_path.write_text(
                    render_target_template(
                        source, filename, env, variables, render_cache
                    )
                )
            else:
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
    name: str,
    config: dict,
    output_dir: Path,
    base_path: Path,
    env: jinja2.Environment,
    variables: dict,
    render_cache: dict,
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
    write_target_files(
        target_dir, files_config, base_path, env, variables, render_cache
    )


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

    env = build_jinja_env(base_path)
    render_cache = {}
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

            # `variables` drives both the Jinja expressions in the configuration itself and the
            # target file templates, so the two can't disagree about, say, how many metrics an
            # experiment generates. It is an input to generation, not part of the output.
            variables = resolved.pop("variables", {})
            resolved = render_config(resolved, env, variables)

            for suite in suites:
                write_experiment(
                    name,
                    resolved,
                    base_dir / suite / "cases",
                    base_path,
                    env,
                    variables,
                    render_cache,
                )
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
