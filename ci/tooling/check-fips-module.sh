#!/usr/bin/env bash
#
# Fails if the AWS-LC FIPS module used by FIPS builds is not a FIPS 140-3 certified module version.
#
# `aws-lc-fips-sys` bundles the AWS-LC FIPS module source, so we resolve the dependency graph with the `fips` feature
# enabled, find the `aws-lc-fips-sys` package it uses, and read the FIPS module version from the bundled headers. This
# mirrors how `aws-lc-fips-sys` identifies the module itself: `AWSLC_FIPS_VERSION_NUMBER` on newer branches, falling
# back to the major version of `AWSLC_VERSION_NUMBER_STRING` on older ones (e.g. FIPS 3.x -> 3).
#
# Only add a module version to CERTIFIED_FIPS_MODULE_VERSIONS once NIST has certified it; AWS-LC lists certified
# modules under "Validations" in https://github.com/aws/aws-lc/blob/main/crypto/fipsmodule/FIPS.md.

set -euo pipefail

CERTIFIED_FIPS_MODULE_VERSIONS=(3)

echo "[*] Checking that FIPS builds use a certified AWS-LC FIPS module..."

manifest_paths="$(cargo metadata --format-version 1 --locked --features fips | jq -r '
    . as $m
    | [.resolve.nodes[].id] as $resolved
    | $m.packages[]
    | select(.name == "aws-lc-fips-sys" and (.id as $id | $resolved | index($id)))
    | .manifest_path
')"

if [[ -z "${manifest_paths}" ]]; then
    echo "error: aws-lc-fips-sys not found in the FIPS dependency graph; update this check to match how FIPS builds" \
        "get their cryptographic module." >&2
    exit 1
fi

status=0
while IFS= read -r manifest_path; do
    base_h="$(dirname "${manifest_path}")/aws-lc/include/openssl/base.h"
    if [[ ! -f "${base_h}" ]]; then
        echo "error: could not find ${base_h}; update this check to match the aws-lc-fips-sys source layout." >&2
        exit 1
    fi

    module_version="$(sed -nE 's/^#define AWSLC_FIPS_VERSION_NUMBER[[:space:]]+([0-9]+).*/\1/p' "${base_h}")"
    if [[ -z "${module_version}" ]]; then
        module_version="$(sed -nE 's/^#define AWSLC_VERSION_NUMBER_STRING[[:space:]]+"([0-9]+)\..*/\1/p' "${base_h}")"
    fi
    if [[ -z "${module_version}" ]]; then
        echo "error: could not determine the FIPS module version from ${base_h}." >&2
        exit 1
    fi

    crate="$(basename "$(dirname "${manifest_path}")")"
    if [[ " ${CERTIFIED_FIPS_MODULE_VERSIONS[*]} " == *" ${module_version} "* ]]; then
        echo "${crate} bundles AWS-LC FIPS module ${module_version}, which is certified."
    else
        echo "error: ${crate} bundles AWS-LC FIPS module ${module_version}, which is not in the certified list" \
            "(${CERTIFIED_FIPS_MODULE_VERSIONS[*]}). See the aws-lc-rs pin in Cargo.toml." >&2
        status=1
    fi
done <<< "${manifest_paths}"

exit "${status}"
