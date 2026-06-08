#!/usr/bin/env bash
# Assembles a release tarball for an already-built agent-data-plane binary.
#
# Produces an artifact whose layout matches the linux release tarball published by
# `.push-release-tarball-definition` in .gitlab/release.yml (which extracts it from the docker
# image built by `docker/Dockerfile.agent-data-plane`):
#
#   opt/datadog-agent/embedded/bin/agent-data-plane
#   opt/datadog/agent-data-plane/NOTICE
#   opt/datadog/agent-data-plane/LICENSE
#   opt/datadog/agent-data-plane/LICENSE-3rdparty.csv
#   opt/datadog/agent-data-plane/LICENSES/THIRD-PARTY-*
#
# Downstream consumers (the Datadog Agent omnibus build, see
# omnibus/config/software/datadog-agent-data-plane.rb in DataDog/datadog-agent) rely on this
# layout being identical across platforms.
#
# Inputs (all required, all read from environment so callers can pass them via Makefile or
# directly):
#
#   OUTPUT_DIR              Directory to place the final tarball in. Created if missing.
#   BUILD_PROFILE           Cargo profile the binary was built under. The binary is read from
#                           `target/${BUILD_PROFILE}/agent-data-plane` relative to CWD.
#   TARGET_OS               OS slug for the tarball filename (e.g. "darwin", "linux").
#   TARGET_ARCH             Arch slug for the tarball filename (e.g. "amd64", "arm64").
#   ADP_VERSION             ADP version for the tarball filename.
#   SPDX_LICENSES_VERSION   SPDX license-list-data tag (without the leading "v"), used to fetch
#                           the per-license THIRD-PARTY-* texts. Must match the version pinned
#                           in docker/Dockerfile.agent-data-plane so the linux and darwin
#                           tarballs ship the same set of license texts.
#
# Output: ${OUTPUT_DIR}/agent-data-plane-${ADP_VERSION}-${TARGET_OS}-${TARGET_ARCH}.tar.gz
#
# Must be invoked from the saluki repo root (the working directory is read as-is for the
# binary path and for NOTICE/LICENSE/LICENSE-3rdparty.csv).

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
for f in NOTICE LICENSE LICENSE-3rdparty.csv; do
  [[ -f "${f}" ]] || { echo "package-adp-tarball: missing '${f}' in CWD ($(pwd))" >&2; exit 1; }
done

tarball_name="agent-data-plane-${ADP_VERSION}-${TARGET_OS}-${TARGET_ARCH}.tar.gz"
echo "[*] Packaging ${tarball_name}"

# Stage everything under a fresh per-invocation directory so re-runs don't pick up stale files.
stage="$(mktemp -d)"
trap 'rm -rf "${stage}"' EXIT
mkdir -p "${stage}/tarball/opt/datadog-agent/embedded/bin"
mkdir -p "${stage}/tarball/opt/datadog/agent-data-plane/LICENSES"

cp "${binary}" "${stage}/tarball/opt/datadog-agent/embedded/bin/"
cp NOTICE LICENSE LICENSE-3rdparty.csv "${stage}/tarball/opt/datadog/agent-data-plane/"

# Replicate the `license-builder` stage from docker/Dockerfile.agent-data-plane on the host so
# the tarball ships the same per-license THIRD-PARTY-* files as the linux artifact.
echo "[*] Fetching SPDX license-list-data v${SPDX_LICENSES_VERSION}"
curl -sSfL -o "${stage}/spdx.tar.gz" \
  "https://github.com/spdx/license-list-data/archive/refs/tags/v${SPDX_LICENSES_VERSION}.tar.gz"
tar -C "${stage}" -xzf "${stage}/spdx.tar.gz"
spdx_text_dir="${stage}/license-list-data-${SPDX_LICENSES_VERSION}/text"

# The pipeline below mirrors the `license-builder` stage in docker/Dockerfile.agent-data-plane
# verbatim (single-occurrence sed paren strip and all) so the resulting THIRD-PARTY-* set is
# identical to the linux tarball. Each token in the third CSV column is one SPDX identifier
# (with OR/AND/WITH/parens noise we strip out).
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

# `sha256sum` is GNU coreutils (Linux); macOS uses BSD `shasum -a 256` to print the same digest
# format. Probe and fall back so this script works on both runner OSes.
echo "[*] Tarball ready"
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "${output_path}"
else
  shasum -a 256 "${output_path}"
fi
