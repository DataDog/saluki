#!/usr/bin/env bash
# Emit a GitLab child-pipeline YAML for correctness tests on stdout.
#
# Run by .gitlab/e2e.yml's generate-correctness-pipeline job. Uses panoramic's
# `list --json` subcommand to enumerate test cases. A case names its runtime
# (kubernetes_in_docker when it runs pods, otherwise its subject OS) and no
# images: its images are in its plan, and PANORAMIC_SET in the base mixin
# replaces them.

set -euo pipefail

# Header: pull in the mixins, declare the lone stage.
cat <<'EOF'
include:
  - local: .gitlab/correctness-mixins.yml

stages:
  - test
EOF

# Ask panoramic for the test list. jq yields one `<case>\t<runtime>` line per test.
# Kind-runtime tests use .test-correctness-kind-definition (longer timeout); all
# cluster and image management is handled inside panoramic at runtime.
./target/release/panoramic list -d test/correctness/cases --json \
    | jq -r 'to_entries[] | "\(.key)\t\(.value.runtime)"' \
    | while IFS=$'\t' read -r case runtime; do

        case "${runtime}" in
            kubernetes_in_docker)
                base_mixin=".test-correctness-kind-definition" ;;
            docker | linux)
                base_mixin=".test-correctness-definition" ;;
            *)
                echo "case '${case}' has runtime '${runtime}', which no correctness job runs" >&2
                exit 1 ;;
        esac

        # GitLab job names cannot contain '/'. CORRECTNESS_TEST_CASE keeps the
        # case name as is, which panoramic receives as-is.
        job_name="${case//\//-}"

        cat <<EOF

test-correctness-${job_name}:
  extends: ${base_mixin}
  variables:
    CORRECTNESS_TEST_CASE: ${case}
EOF
    done
