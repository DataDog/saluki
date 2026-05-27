#!/usr/bin/env bash
# Fetches temporary AWS credentials based on the runner's IAM role and writes them to disk for shell usage.
#
# This allows us to generate classic `AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY` credential pairs for tooling that needs
# to access AWS resources (like `buildcache` accessing S3) from inside the build container. The credentials are written
# to a shell script such that they can then be `export`-ed into the build container via Docker's "build secrets"
# support, which exposes secrets as environment variables while ensuring the values never land in any image layer.

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
} > /tmp/temporary-aws-credentials.sh

echo "Wrote temporary AWS credentials to '/tmp/temporary-aws-credentials.sh'. ($(wc -l < /tmp/temporary-aws-credentials.sh) lines)"
