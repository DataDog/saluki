#!/usr/bin/env bash
#
# Fails if the AWS-LC FIPS module used by FIPS builds is not a FIPS 140-3 certified module version.
#
# `aws-lc-fips-sys` bundles the AWS-LC FIPS module source, so we find the `aws-lc-fips-sys` package used by a FIPS build
# of `agent-data-plane` and read the FIPS module version from the bundled headers. This mirrors how `aws-lc-fips-sys`
# identifies the module itself: `AWSLC_FIPS_VERSION_NUMBER` on newer branches, falling back to the major version of
# `AWSLC_VERSION_NUMBER_STRING` on older ones (e.g. FIPS 3.x -> 3).
#
# Only add a module version to CERTIFIED_FIPS_MODULE_VERSIONS once NIST has certified it; AWS-LC lists certified
# modules under "Validations" in https://github.com/aws/aws-lc/blob/main/crypto/fipsmodule/FIPS.md.

set -euo pipefail

CERTIFIED_FIPS_MODULE_VERSIONS=(3)

echo "[*] Checking that FIPS builds use a certified AWS-LC FIPS module..."

# `cargo metadata` resolves the whole workspace and unifies features across every member, so it can't tell us what a
# single member's build uses: if any other member enabled `aws-lc-rs/fips`, `aws-lc-fips-sys` would look like part of
# `agent-data-plane` too, since it always depends on `aws-lc-rs`. `cargo tree` resolves features for only the selected
# package, the same way `cargo build --package` does, so we use it to find which `aws-lc-fips-sys` versions the FIPS
# build of `agent-data-plane` links, and only use `cargo metadata` to locate their source.
fips_sys_versions="$(cargo tree --locked --package agent-data-plane --features fips --edges normal --target all \
    --prefix none --format '{p}' | sed -nE 's/^aws-lc-fips-sys v([^ ]+).*/\1/p' | sort -u)"

if [[ -z "${fips_sys_versions}" ]]; then
    echo "error: aws-lc-fips-sys not found in the agent-data-plane FIPS dependency graph; update this check to match" \
        "how FIPS builds get their cryptographic module." >&2
    exit 1
fi

manifest_paths="$(cargo metadata --format-version 1 --locked --features agent-data-plane/fips \
    | jq -r --arg versions "${fips_sys_versions}" '
        ($versions | split("\n")) as $versions
        | .packages[]
        | select(.name == "aws-lc-fips-sys" and (.version as $v | $versions | index($v)))
        | .manifest_path
    ')"

if [[ -z "${manifest_paths}" ]]; then
    echo "error: could not locate the source of aws-lc-fips-sys ${fips_sys_versions//$'\n'/, } in cargo metadata." >&2
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
