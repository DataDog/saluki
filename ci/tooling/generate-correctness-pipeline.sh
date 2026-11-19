#!/usr/bin/env bash
# Emit a GitLab child-pipeline YAML for correctness tests on stdout.
#
# Run by .gitlab/e2e.yml's generate-correctness-pipeline job. Uses panoramic's
# `list --json` subcommand to enumerate test cases. A row has one of two shapes:
#
# - A legacy case names its runtime (docker or kubernetes_in_docker) and its
#   images. Those whose baseline is a saluki-images/ placeholder also extend
#   .test-correctness-adp-baseline so panoramic gets the CI-built image at
#   runtime.
# - A participant case names its subject OS as its runtime and no images: its
#   images are in its plan, and PANORAMIC_SET in the base mixin replaces them on
#   both sides.

set -euo pipefail

# Header: pull in the mixins, declare the lone stage.
cat <<'EOF'
include:
  - local: .gitlab/correctness-mixins.yml

stages:
  - test
EOF

# Ask panoramic for the test list. jq yields one `<case>\t<runtime>\t<baseline-image>`
# line per test, the baseline empty when the row names none. Kind-runtime tests use
# .test-correctness-kind-definition (longer timeout); all cluster and image management is
# handled inside panoramic at runtime.
./target/release/panoramic list -d test/correctness/cases --json \
    | jq -r 'to_entries[] | "\(.key)\t\(.value.runtime)\t\(.value.images.baseline // "")"' \
    | while IFS=$'\t' read -r case runtime baseline; do

        case "${runtime}" in
            kubernetes_in_docker)
                base_mixin=".test-correctness-kind-definition" ;;
            docker | linux)
                base_mixin=".test-correctness-definition" ;;
            *)
                echo "case '${case}' has runtime '${runtime}', which no correctness job runs" >&2
                exit 1 ;;
        esac

        # saluki-images/* is a local-only placeholder; CI must inject the
        # registry image via the adp-baseline mixin. Real registry URLs
        # (DDOT) work as-is. A participant case names no baseline.
        if [[ "${baseline}" == saluki-images/* ]]; then
            extends="[${base_mixin}, .test-correctness-adp-baseline]"
        else
            extends="[${base_mixin}]"
        fi

        # GitLab job names cannot contain '/'. Matrix variant names use '/'
        # as a separator (e.g. dsd-origin-detection-matrix/legacy), so
        # replace it with '-' for the job name while keeping the original
        # name for CORRECTNESS_TEST_CASE, which panoramic receives as-is.
        job_name="${case//\//-}"

        cat <<EOF

test-correctness-${job_name}:
  extends: ${extends}
  variables:
    CORRECTNESS_TEST_CASE: ${case}
EOF
    done
