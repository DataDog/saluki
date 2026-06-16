#!/usr/bin/env sh
#
# Harvests the per-dependency third-party license texts referenced by LICENSE-3rdparty.csv into an
# output directory as THIRD-PARTY-<spdx-id> files.
#
# Shared by the Agent Data Plane container build (docker/Dockerfile.agent-data-plane) and the host
# release tarball (ci/tooling/package-adp-tarball.sh) so both ship an identical set of license files.
#
# Usage: collect-third-party-licenses.sh <spdx-text-dir> <license-csv> <output-dir>
#   <spdx-text-dir>  directory of SPDX license texts (<spdx-id>.txt), e.g. the `text/` directory
#                    produced by fetch-spdx-licenses.sh
#   <license-csv>    path to LICENSE-3rdparty.csv
#   <output-dir>     directory to write THIRD-PARTY-<spdx-id> files into (created if absent)

set -eu

spdx_text_dir="${1:?usage: collect-third-party-licenses.sh <spdx-text-dir> <license-csv> <output-dir>}"
license_csv="${2:?usage: collect-third-party-licenses.sh <spdx-text-dir> <license-csv> <output-dir>}"
output_dir="${3:?usage: collect-third-party-licenses.sh <spdx-text-dir> <license-csv> <output-dir>}"

mkdir -p "${output_dir}"

# Walk the third column of LICENSE-3rdparty.csv (the SPDX expression for each dependency); split each
# row's expression on whitespace into individual SPDX identifiers; drop the join keywords (OR/AND/WITH)
# and tokens we don't have texts for (Custom, LLVM-exception); strip parens used for grouping
# multi-license expressions; dedupe; then copy each identifier's license text into the output
# directory as THIRD-PARTY-<spdx-id>.
tail -n +2 "${license_csv}" \
    | awk -F ',' '{print $3}' \
    | awk -F' ' '{for(i=1;i<=NF;i++) print $i}' \
    | grep -v -E "(OR|AND|WITH|Custom|LLVM-exception)" \
    | sed s/[\(\)]// \
    | sort \
    | uniq \
    | xargs -I {} cp "${spdx_text_dir}/{}.txt" "${output_dir}/THIRD-PARTY-{}"
