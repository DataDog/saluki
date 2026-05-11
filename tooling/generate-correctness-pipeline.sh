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

# Ask panoramic for the test list. jq yields one `<case>\t<runtime>\t<baseline-image>`
# line per test. Kind-runtime tests use .test-correctness-kind-definition (longer timeout);
# all cluster and image management is handled inside panoramic at runtime.
./target/release/panoramic list -d test/correctness --json \
    | jq -r 'to_entries[] | "\(.key)\t\(.value.runtime)\t\(.value.images.baseline)"' \
    | while IFS=$'\t' read -r case runtime baseline; do

        # Pick the right base mixin depending on runtime.
        if [[ "${runtime}" == "kubernetes_in_docker" ]]; then
            base_mixin=".test-correctness-kind-definition"
        else
            base_mixin=".test-correctness-definition"
        fi

        # saluki-images/* is a local-only placeholder; CI must inject the
        # registry image via the adp-baseline mixin. Real registry URLs
        # (DDOT) work as-is.
        if [[ "${baseline}" == saluki-images/* ]]; then
            extends="[${base_mixin}, .test-correctness-adp-baseline]"
        else
            extends="[${base_mixin}]"
        fi

        # GitLab job names cannot contain '/'. Matrix variant names use '/'
        # as a separator (e.g. dsd-origin-detection-matrix/legacy), so
        # replace it with '--' for the job name while keeping the original
        # name for CORRECTNESS_TEST_CASE, which panoramic receives as-is.
        job_name="${case//\//-}"

        cat <<EOF

test-correctness-${job_name}:
  extends: ${extends}
  variables:
    CORRECTNESS_TEST_CASE: ${case}
EOF
    done
