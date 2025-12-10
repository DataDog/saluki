#!/usr/bin/env bash
#
# Installs pr-commenter for CI builds.
# Uses dd-package to install from internal Datadog repositories.
#
set -euo pipefail
set -x

readonly VERSION="14692290-a6440fd1"
readonly DISTRIBUTION="20.04"

# Verify dd-package is available.
check_dependencies() {
    if ! command -v dd-package >/dev/null 2>&1; then
        echo "ERROR: dd-package must be installed and available on \$PATH" >&2
        exit 1
    fi
}

show_maintainer_note() {
    cat << 'EOF'
+----------------------------- NOTE TO MAINTAINERS -------------------------------+
|                                                                                 |
|  Ignore the `bdist_wheel` error raised when installing 'devtools/pr-commenter'  |
|  This error is known behavior, and you should see a successful install          |
|  beneath the error message in the GitLab CI logs                                |
|                                                                                 |
+---------------------------------------------------------------------------------+
EOF
}

install_pr_commenter() {
    apt-get update
    dd-package \
        --bucket binaries-ddbuild-io-prod \
        --package devtools/pr-commenter \
        --version "${VERSION}" \
        --distribution "${DISTRIBUTION}"
    apt-get clean
}

check_dependencies
show_maintainer_note
install_pr_commenter
