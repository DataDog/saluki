"""Runs the Agent's own config schema codegen (`schema_codegen`) in an Agent checkout.

Run from the root of the checkout. This is what `dda inv schema.codegen` does, without invoke's
task loader.

`tasks/__init__.py` imports the whole task tree, which pulls in many packages that codegen does not
need and that the Agent's dev requirements do not provide. So instead of importing `tasks` normally,
this registers the packages on the path to `tasks.schema.generate` as empty namespace packages that
point into the checkout. Python then loads only the modules codegen imports, and none of the
`__init__.py` files of those packages run.
"""

import os
import sys
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
from tasks.schema.generate import schema_codegen  # noqa: E402

schema_codegen(invoke.context.Context())
