"""Runs the Agent's own config schema codegen (`schema_codegen`) in an Agent checkout, then writes
the compressed schemas the Agent embeds to validate its config.

Run from the root of the checkout. This is what `dda inv schema.codegen` and `dda inv
schema.compress` do, without invoke's task loader.

`tasks/__init__.py` imports the whole task tree, which pulls in many packages that codegen does not
need and that the Agent's dev requirements do not provide. So instead of importing `tasks` normally,
this registers the packages on the path to `tasks.schema.generate` as empty namespace packages that
point into the checkout. Python then loads only the modules codegen imports, and none of the
`__init__.py` files of those packages run.
"""

import os
import sys
import tempfile
import types

ROOT = os.getcwd()
PACKAGES = [
    "tasks",
    "tasks.schema",
    "tasks.libs",
    "tasks.libs.build",
    "tasks.libs.common",
    "tasks.libs.types",
]

for name in PACKAGES:
    module = types.ModuleType(name)
    module.__path__ = [os.path.join(ROOT, *name.split("."))]
    sys.modules[name] = module

import invoke.context  # noqa: E402
import zstandard  # noqa: E402
from tasks.schema.generate import (  # noqa: E402
    COMPRESS_DIR,
    CORE_SCHEMA_MAIN_FILE,
    SYSTEM_PROBE_SCHEMA_MAIN_FILE,
    schema_codegen,
)
from tasks.schema.produce_byproduct import produce_byproduct  # noqa: E402


def compress_schemas():
    """Writes `pkg/config/schema/compressed/*.yaml.zstd` as `_compress_no_bazel` in
    `tasks/schema/generate.py` does, with the `zstandard` package in place of the `zstd` binary.

    Without these files the Agent's schema component has no schema, logs an error and skips
    validation, so `schema.enforce` would revert nothing.
    """
    compressed_dir = os.path.join(COMPRESS_DIR, "compressed")
    os.makedirs(compressed_dir, exist_ok=True)
    # `zstd --no-check -5`: no frame checksum, level 5 (the Agent's _ZSTD_ARGS).
    compressor = zstandard.ZstdCompressor(level=5, write_checksum=False)
    for name, top_schema in (
        ("core_schema", CORE_SCHEMA_MAIN_FILE),
        ("system-probe_schema", SYSTEM_PROBE_SCHEMA_MAIN_FILE),
    ):
        embedded_fd, embedded_path = tempfile.mkstemp(suffix=".yaml")
        os.close(embedded_fd)
        try:
            produce_byproduct("embedded", top_schema, embedded_path)
            with open(embedded_path, "rb") as f:
                data = f.read()
        finally:
            os.remove(embedded_path)
        if not data:
            raise SystemExit(f"the embedded {name} schema is empty")
        with open(os.path.join(compressed_dir, f"{name}.yaml.zstd"), "wb") as f:
            f.write(compressor.compress(data))


schema_codegen(invoke.context.Context())
compress_schemas()
