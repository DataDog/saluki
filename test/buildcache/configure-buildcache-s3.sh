#!/usr/bin/env bash
# Configures buildcache to use S3 as a remote for validation purposes:
#   - Exports AWS temporary credentials from the runner's IAM role (via aws-cli).
#   - Writes ~/.buildcache/config.json pointing at the configured S3 bucket.
#   - Persists the AWS_* env vars to /tmp/buildcache-aws-env.sh so subsequent CI
#     script lines (each of which runs in a fresh shell) can source them.
#
# Required env:
#   BUILDCACHE_S3_BUCKET   bucket name, e.g. dd-saluki-build-cache-us1-ddbuild-io
#   BUILDCACHE_S3_REGION   AWS region, e.g. us-east-1

set -euo pipefail

command -v aws        >/dev/null 2>&1 || { echo "aws-cli not found"        >&2; exit 1; }
command -v buildcache >/dev/null 2>&1 || { echo "buildcache not found"     >&2; exit 1; }
: "${BUILDCACHE_S3_BUCKET:?must be set}"
: "${BUILDCACHE_S3_REGION:?must be set}"

# Grab temp creds from the runner's IAM role.
eval "$(aws configure export-credentials --format env)"
: "${AWS_ACCESS_KEY_ID:?export-credentials did not yield AWS_ACCESS_KEY_ID}"
: "${AWS_SECRET_ACCESS_KEY:?export-credentials did not yield AWS_SECRET_ACCESS_KEY}"

# Persist for later job lines; GitLab runs each script: line in its own shell.
umask 077
{
    echo "export AWS_ACCESS_KEY_ID=${AWS_ACCESS_KEY_ID}"
    echo "export AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY}"
    [[ -n "${AWS_SESSION_TOKEN:-}" ]] && echo "export AWS_SESSION_TOKEN=${AWS_SESSION_TOKEN}"
    echo "export AWS_DEFAULT_REGION=${BUILDCACHE_S3_REGION}"
} > /tmp/buildcache-aws-env.sh

# Write the buildcache config. We deliberately do NOT put s3_access/s3_secret in
# this file so that buildcache reads creds from the AWS_* env vars at call time
# (which is the only way to flow AWS_SESSION_TOKEN through for temporary creds).
mkdir -p "${HOME}/.buildcache"
cat > "${HOME}/.buildcache/config.json" <<EOF
{
  "remote": "s3://s3.${BUILDCACHE_S3_REGION}.amazonaws.com/${BUILDCACHE_S3_BUCKET}",
  "debug": 2
}
EOF

echo "Wrote ${HOME}/.buildcache/config.json:"
cat "${HOME}/.buildcache/config.json"
