#!/usr/bin/env bash
#
# Post a condensed SMP benchmark report to Slack.
#
# Usage: report-smp-to-slack.sh <condensed-report.md>
#
# This is the nightly full-suite reporting hook. It currently STUBS the Slack post: it just
# echoes the report (and what it would send) to the job log. Sending internal Slack messages
# from public-repo CI needs a securely-provisioned webhook/token, which is not wired up yet —
# see the commented scaffold below for the intended shape once that exists.

set -euo pipefail

report_path="${1:?usage: report-smp-to-slack.sh <condensed-report.md>}"

if [[ ! -f "${report_path}" ]]; then
    echo "report-smp-to-slack: report file not found: ${report_path}" >&2
    exit 1
fi

echo "===================== SMP full-suite report (Slack stub) ====================="
cat "${report_path}"
echo "=============================================================================="
echo "[stub] would POST the above report to Slack."

# TODO: once a Slack webhook/token is securely available in CI (e.g. an SSM param such as
# ci.saluki.smp-slack-webhook fetched into $SLACK_WEBHOOK_URL), replace the stub above with a
# real post, e.g.:
#
#   payload=$(jq -Rs '{text: .}' < "${report_path}")
#   curl --fail --silent --show-error \
#       -X POST -H 'Content-type: application/json' \
#       --data "${payload}" \
#       "${SLACK_WEBHOOK_URL}"
