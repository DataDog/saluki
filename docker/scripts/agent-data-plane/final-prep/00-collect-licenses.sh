#!/usr/bin/env sh
#
# Assembles the third-party license texts into the mirrored output tree at
# /rootfs/opt/datadog/agent-data-plane/LICENSES.
#
# Harvests the per-dependency THIRD-PARTY-* texts referenced by LICENSE-3rdparty.csv, using the same
# shared ci/tooling scripts the host release tarball uses, so the image and tarball ship an identical
# license set.
#
# SPDX license texts come from a prebuilt source image when one is mounted (CI passes
# SPDX_LICENSES_IMAGE -- the Datadog spdx-license-list-data image -- so the build doesn't reach out to
# GitHub every time); otherwise they're fetched from GitHub on demand (local and clean builds).

set -eu

licenses_root="/rootfs/opt/datadog/agent-data-plane"

# The spdx-license-list-data image carries the GitHub release tarball's contents under /spdx, so once
# its root is mounted at /mnt/spdx the license texts live at /mnt/spdx/spdx/text. The default
# `scratch` source leaves /mnt/spdx empty, which falls through to fetching from GitHub.
if [ -d /mnt/spdx/spdx/text ]; then
    spdx_text_dir=/mnt/spdx/spdx/text
elif [ -n "$(ls -A /mnt/spdx 2>/dev/null || true)" ]; then
    # An image was mounted but the texts aren't where we expect -- fail loudly rather than silently
    # falling back to a network fetch (which would defeat the point of providing a prebuilt source).
    echo "ERROR: an SPDX source image is mounted at /mnt/spdx but /mnt/spdx/spdx/text is missing." >&2
    echo "       Expected the release-tarball contents under /spdx. Contents of /mnt/spdx:" >&2
    ls -la /mnt/spdx >&2 || true
    exit 1
else
    # No prebuilt source (scratch default): fetch the SPDX archive from GitHub. Ensure curl and CA
    # certificates are available first (a near no-op on the CI build image, an install on plain Ubuntu).
    apt-get update
    apt-get install --no-install-recommends -y ca-certificates=20260601~26.04.1 curl=8.18.0-1ubuntu2.2
    sh /tooling/fetch-spdx-licenses.sh /tmp/spdx
    spdx_text_dir=/tmp/spdx/text
fi

# Harvest the identifiers our dependencies use into the mirror. The CSV is bind-mounted at
# /tmp/LICENSE-3rdparty.csv for this step (the Dockerfile COPYs it into the image separately,
# afterwards, so the running image still carries it under /opt).
sh /tooling/collect-third-party-licenses.sh \
    "${spdx_text_dir}" \
    /tmp/LICENSE-3rdparty.csv \
    "${licenses_root}/LICENSES"
