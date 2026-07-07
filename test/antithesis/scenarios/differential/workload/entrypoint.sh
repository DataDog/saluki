#!/usr/bin/env bash
set -euo pipefail

# Differential workload client entrypoint.
#
# Gated on intake-healthy by compose `depends_on`. Emit `setup_complete`, then
# idle so Antithesis runs the test commands.

/opt/antithesis/setup-complete.sh
echo "setup_complete emitted; differential workload idle, awaiting Antithesis test commands."
exec tail -f /dev/null
