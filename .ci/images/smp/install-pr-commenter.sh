#!/usr/bin/env bash

# Make sure we have the dd-package helper installed.
command -v dd-package >/dev/null 2>&1 || { echo "DD Package helper (\`dd-package\`) must be installed and available on \$PATH" >&2; exit 1; }

cat << EOF
+----------------------------- NOTE TO MAINTAINERS -------------------------------+
|                                                                                 |
|  Ignore the \`bdist_wheel\` error raised when installing 'devtools/pr-commenter'  |
|  This error is known behavior, and you should see a successful install          |
|  beneath the error message in the GitLab CI logs                                |
|                                                                                 |
+---------------------------------------------------------------------------------+

EOF

# We need this specific version of `pr-commenter`, _and_ we also need to set the distribution to
# 20.04, even though we're actually running 20.10. ¯\_(ツ)_/¯
apt-get update
dd-package --bucket binaries.ddbuild.io --package devtools/pr-commenter --version "14692290-a6440fd1" --distribution "20.04"
