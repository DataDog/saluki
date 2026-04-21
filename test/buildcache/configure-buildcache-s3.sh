#!/usr/bin/env bash
# Fetches AWS temporary credentials from the runner's attached IAM role via aws-cli and writes
# them to /tmp/buildcache-aws-env.sh as `export` lines. That file is fed to `docker buildx build`
# as a BuildKit secret (`--secret id=buildcache-aws-env,src=/tmp/buildcache-aws-env.sh`) so
# buildcache inside the build container can authenticate SigV4 requests against the S3 remote
# cache without those credentials ever landing in an image layer.

set -euo pipefail

command -v aws >/dev/null 2>&1 || { echo "aws-cli not found" >&2; exit 1; }

eval "$(aws configure export-credentials --format env)"
: "${AWS_ACCESS_KEY_ID:?export-credentials did not yield AWS_ACCESS_KEY_ID}"
: "${AWS_SECRET_ACCESS_KEY:?export-credentials did not yield AWS_SECRET_ACCESS_KEY}"

umask 077
{
    echo "export AWS_ACCESS_KEY_ID=${AWS_ACCESS_KEY_ID}"
    echo "export AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}"
    [[ -n "${AWS_SESSION_TOKEN:-}" ]] && echo "export AWS_SESSION_TOKEN=${AWS_SESSION_TOKEN}"
} > /tmp/buildcache-aws-env.sh

echo "Wrote /tmp/buildcache-aws-env.sh ($(wc -l < /tmp/buildcache-aws-env.sh) export lines)."
