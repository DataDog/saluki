#!/usr/bin/env bash
# Assembles a release tarball for an already-built agent-data-plane binary, matching the layout
# the linux release tarball publishes (see `.push-release-tarball-definition` in
# .gitlab/release.yml) so downstream consumers (the Datadog Agent omnibus build) can use the
# same software definition for both platforms:
#
#   opt/datadog-agent/embedded/bin/agent-data-plane
#   opt/datadog/agent-data-plane/{NOTICE,LICENSE,LICENSE-3rdparty.csv}
#   opt/datadog/agent-data-plane/LICENSES/THIRD-PARTY-*
#
# All inputs are read from the environment (see the `:?` checks below). Driven by
# `make package-adp-host`; can also be invoked directly from the saluki repo root.

set -euo pipefail

: "${OUTPUT_DIR:?OUTPUT_DIR is required}"
: "${BUILD_PROFILE:?BUILD_PROFILE is required}"
: "${TARGET_OS:?TARGET_OS is required}"
: "${TARGET_ARCH:?TARGET_ARCH is required}"
: "${ADP_VERSION:?ADP_VERSION is required}"
: "${SPDX_LICENSES_VERSION:?SPDX_LICENSES_VERSION is required}"

binary="target/${BUILD_PROFILE}/agent-data-plane"
if [[ ! -x "${binary}" ]]; then
  echo "package-adp-tarball: expected binary at '${binary}' (run 'make build-adp-host' first)" >&2
  exit 1
fi

tarball_name="agent-data-plane-${ADP_VERSION}-${TARGET_OS}-${TARGET_ARCH}.tar.gz"
echo "[*] Packaging ${tarball_name}"

stage="$(mktemp -d)"
trap 'rm -rf "${stage}"' EXIT
mkdir -p "${stage}/tarball/opt/datadog-agent/embedded/bin"
mkdir -p "${stage}/tarball/opt/datadog/agent-data-plane/LICENSES"

cp "${binary}" "${stage}/tarball/opt/datadog-agent/embedded/bin/"
cp NOTICE LICENSE LICENSE-3rdparty.csv "${stage}/tarball/opt/datadog/agent-data-plane/"

# Replicate the `license-builder` stage from docker/Dockerfile.agent-data-plane on the host so
# the tarball ships the same per-license THIRD-PARTY-* files as the linux artifact. The awk
# pipeline (single-occurrence sed paren strip and all) mirrors the Dockerfile verbatim.
echo "[*] Fetching SPDX license-list-data v${SPDX_LICENSES_VERSION}"
curl -sSfL -o "${stage}/spdx.tar.gz" \
  "https://github.com/spdx/license-list-data/archive/refs/tags/v${SPDX_LICENSES_VERSION}.tar.gz"
tar -C "${stage}" -xzf "${stage}/spdx.tar.gz"
spdx_text_dir="${stage}/license-list-data-${SPDX_LICENSES_VERSION}/text"

# Walk the third column of LICENSE-3rdparty.csv (the SPDX expression for each dependency); split
# each row's expression on whitespace into individual SPDX identifiers; drop the join keywords
# (OR/AND/WITH) and tokens we don't have texts for (Custom, LLVM-exception); strip parens used
# for grouping multi-license expressions; dedupe; then copy each remaining identifier's
# license text from the SPDX archive into the tarball as `THIRD-PARTY-<spdx-id>`.
echo "[*] Harvesting THIRD-PARTY-* license texts"
tail -n +2 LICENSE-3rdparty.csv \
  | awk -F ',' '{print $3}' \
  | awk -F' ' '{for(i=1;i<=NF;i++) print $i}' \
  | grep -v -E "(OR|AND|WITH|Custom|LLVM-exception)" \
  | sed s/[\(\)]// \
  | sort \
  | uniq \
  | xargs -I {} cp "${spdx_text_dir}/{}.txt" \
      "${stage}/tarball/opt/datadog/agent-data-plane/LICENSES/THIRD-PARTY-{}"

mkdir -p "${OUTPUT_DIR}"
output_path="${OUTPUT_DIR}/${tarball_name}"
tar -C "${stage}/tarball" -czf "${output_path}" .

echo "[*] Tarball ready"
# `sha256sum` (GNU coreutils, on linux) and `shasum -a 256` (BSD, on macOS) print the same digest.
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${output_path}"
else
  shasum -a 256 "${output_path}"
fi
