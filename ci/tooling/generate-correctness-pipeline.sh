#!/usr/bin/env bash
# Emit a GitLab child-pipeline YAML for correctness tests on stdout.
#
# Run by .gitlab/e2e.yml's generate-correctness-pipeline job.
# Reads runnable rows from `panoramic list --subject-os linux --json`, using their
# layout realizations to choose the job template.

set -euo pipefail

# Header: pull in the mixins, declare the lone stage.
cat <<'EOF'
include:
  - local: .gitlab/correctness-mixins.yml

stages:
  - test
EOF

# jq yields one `<case>\t<pod>` line per runnable case.
# Unknown statuses and null layouts on runnable rows fail jq and this script.
./target/release/panoramic list -d test/correctness/cases --subject-os linux --json \
    | jq -r 'to_entries[] | select(if .value.status == "skipped" then false elif .value.status == "runnable" then true else error("unknown status for \(.key)") end) | "\(.key)\t\(any(.value.layout[]; .realization == "kubernetes-pod"))"' \
    | while IFS=$'\t' read -r case pod; do

        # Pod cases use a longer timeout; panoramic manages the cluster and its images.
        if [[ "${pod}" == "true" ]]; then
            base_mixin=".test-correctness-kind-definition"
        else
            base_mixin=".test-correctness-definition"
        fi

        cat <<EOF

test-correctness-${case}:
  extends: [${base_mixin}]
  variables:
    CORRECTNESS_TEST_CASE: ${case}
EOF
    done
