#!/usr/bin/env bash
# Emit a GitLab child-pipeline YAML for correctness tests on stdout.
#
# Run by .gitlab/e2e.yml's generate-correctness-pipeline job. Uses panoramic's
# `list --json` subcommand to enumerate test cases and their baseline images.
# Tests whose baseline is a saluki-images/ placeholder also extend
# .test-correctness-adp-baseline so panoramic gets the CI-built image at runtime.

set -euo pipefail

# Header: pull in the mixins, declare the lone stage.
cat <<'EOF'
include:
  - local: .gitlab/correctness-mixins.yml

stages:
  - test
EOF

# Ask panoramic for the test list. jq yields one `<case>\t<baseline-image>`
# line per test.
./target/release/panoramic list -d test/correctness --json \
    | jq -r 'to_entries[] | "\(.key)\t\(.value.images.baseline)"' \
    | while IFS=$'\t' read -r case baseline; do

        # saluki-images/* is a local-only placeholder; CI must inject the
        # registry image via the adp-baseline mixin. Real registry URLs
        # (DDOT) work as-is.
        if [[ "${baseline}" == saluki-images/* ]]; then
            extends='[.test-correctness-definition, .test-correctness-adp-baseline]'
        else
            extends='[.test-correctness-definition]'
        fi

        cat <<EOF

test-correctness-${case}:
  extends: ${extends}
  variables:
    CORRECTNESS_TEST_CASE: ${case}
EOF
    done
