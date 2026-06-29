#!/usr/bin/env sh
#
# Downloads the SPDX license-list-data archive and extracts it so the per-license texts are available
# under <output-dir>/text/<spdx-id>.txt.
#
# Shared by the Agent Data Plane container build (docker/Dockerfile.agent-data-plane) and the host
# release tarball (ci/tooling/package-adp-tarball.sh) so both source license texts from one place.
#
# Requires `curl` and `tar` on PATH (the caller is responsible for installing them). The version is
# read from SPDX_LICENSES_VERSION.
#
# Usage: SPDX_LICENSES_VERSION=3.28.0 fetch-spdx-licenses.sh <output-dir>

set -eu

: "${SPDX_LICENSES_VERSION:?SPDX_LICENSES_VERSION is required}"
output_dir="${1:?usage: fetch-spdx-licenses.sh <output-dir>}"

work_dir="$(mktemp -d)"
trap 'rm -rf "${work_dir}"' EXIT

curl -sSfL -o "${work_dir}/spdx.tar.gz" \
    "https://github.com/spdx/license-list-data/archive/refs/tags/v${SPDX_LICENSES_VERSION}.tar.gz"
tar -C "${work_dir}" -xzf "${work_dir}/spdx.tar.gz"

# The archive extracts to `license-list-data-<version>/` (which contains the `text/` directory).
# Move it to the requested output directory, replacing anything already there.
rm -rf "${output_dir}"
mkdir -p "$(dirname "${output_dir}")"
mv "${work_dir}/license-list-data-${SPDX_LICENSES_VERSION}" "${output_dir}"
