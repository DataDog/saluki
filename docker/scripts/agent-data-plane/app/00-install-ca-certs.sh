#!/usr/bin/env sh
#
# Ensures CA certificates are present in the final image.
#
# We only install them if they're missing. In CI, the application base image already ships CA
# certificates and may run as a non-root user, so we skip the install (and avoid needing root just to
# run apt-get). For local builds the plain Ubuntu base lacks them but runs as root, so the install
# succeeds. Use the repository's available version because Ubuntu repositories do not retain old
# exact package versions indefinitely.

set -eu

if [ -d /usr/share/ca-certificates ]; then
    exit 0
fi

apt-get update
apt-get install --no-install-recommends -y ca-certificates
apt-get clean
rm -rf /var/lib/apt/lists
