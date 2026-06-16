#!/usr/bin/env sh
#
# Assembles the third-party license texts into the mirrored output tree at
# /rootfs/opt/datadog/agent-data-plane/LICENSES.
#
# Fetches the SPDX license archive and harvests the per-dependency THIRD-PARTY-* texts referenced by
# LICENSE-3rdparty.csv, using the same shared ci/tooling scripts the host release tarball uses, so the image and tarball
# ship an identical license set.

set -eu

licenses_root="/rootfs/opt/datadog/agent-data-plane"

# Ensure curl and CA certificates are available. This is a near no-op on the CI build image (which
# already ships them) and installs them on a plain Ubuntu base for local builds.
apt-get update
apt-get install --no-install-recommends -y ca-certificates curl

# Fetch the SPDX license texts, then harvest the identifiers our dependencies use into the mirror.
sh /tooling/fetch-spdx-licenses.sh /tmp/spdx
sh /tooling/collect-third-party-licenses.sh \
    /tmp/spdx/text \
    "${licenses_root}/LICENSE-3rdparty.csv" \
    "${licenses_root}/LICENSES"
