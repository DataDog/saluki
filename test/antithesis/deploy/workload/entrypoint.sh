#!/usr/bin/env bash
set -euo pipefail

# Workload client entrypoint.
#
# Gated on intake-healthy (compose `depends_on`). Emit `setup_complete`, then
# idle so Antithesis runs the test commands.

/opt/antithesis/setup-complete.sh
echo "setup_complete emitted; workload idle, awaiting Antithesis test commands."
exec tail -f /dev/null
