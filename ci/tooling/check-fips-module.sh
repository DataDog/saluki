#!/usr/bin/env bash
#
# Fails if FIPS builds use an AWS-LC FIPS branch that has no FIPS 140-3 certified release, or an AWS-LC version that is
# older than the certified release on that branch.
#
# `aws-lc-fips-sys` bundles the AWS-LC FIPS module source, so we find the `aws-lc-fips-sys` package used by a FIPS build
# of `agent-data-plane` and read two values from the bundled headers:
#
# - The FIPS module number. This mirrors how `aws-lc-fips-sys` identifies the module itself: `AWSLC_FIPS_VERSION_NUMBER`
#   on newer branches. The FIPS 3 and FIPS 4 branches don't define it, so there the major version of
#   `AWSLC_VERSION_NUMBER_STRING` is the module number (for example, 4.2.0 -> 4). Other major versions must define it.
# - The full AWS-LC version, from `AWSLC_VERSION_NUMBER_STRING`.
#
# The module number identifies the FIPS branch, and so the certificate that can apply. Each certificate names one AWS-LC
# version on that branch. `aws-lc-fips-sys` can bundle a later version from the same branch, which the certificate
# doesn't list. This check doesn't decide whether such a version is covered: it only makes sure that FIPS builds stay on a
# branch with a certified release, and never go below that release. An aws-lc-rs maintainer suggests that later versions
# keep the status (https://github.com/aws/aws-lc-rs/issues/1166), but AWS hasn't confirmed this
# (https://github.com/aws/aws-lc/issues/3830). The AWS-LC major version and the module number can diverge in future
# releases, so don't infer one from the other.
#
# Add a module to `certified_min_version` only after NIST certifies a release on its branch. Use the AWS-LC version that
# the certificate names as the minimum version.

set -euo pipefail

# Prints the AWS-LC version that the certificate names for the FIPS module number `$1`. Prints nothing if no release on
# that branch is certified.
certified_min_version() {
    case "$1" in
        # https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5298
        # https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5314
        3) echo "3.1.0" ;;
        # https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5595
        # https://csrc.nist.gov/projects/cryptographic-module-validation-program/certificate/5596
        4) echo "4.0.0" ;;
        *) ;;
    esac
}

# Returns success if version `$1` is equal to or greater than version `$2`.
version_at_least() {
    [[ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -n 1)" == "$2" ]]
}

echo "[*] Checking that FIPS builds use an AWS-LC FIPS branch with a certified release..."

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

    aws_lc_version="$(sed -nE 's/^#define AWSLC_VERSION_NUMBER_STRING[[:space:]]+"([0-9]+(\.[0-9]+)*)".*/\1/p' "${base_h}")"
    if [[ -z "${aws_lc_version}" ]]; then
        echo "error: could not determine the AWS-LC version from ${base_h}." >&2
        exit 1
    fi

    module_version="$(sed -nE 's/^#define AWSLC_FIPS_VERSION_NUMBER[[:space:]]+([0-9]+).*/\1/p' "${base_h}")"
    if [[ -z "${module_version}" ]]; then
        # The FIPS 3 and FIPS 4 branches predate `AWSLC_FIPS_VERSION_NUMBER` (aws/aws-lc#3211). On these branches the
        # major version is the module number. For any other major version, a missing define means that the header
        # layout changed, so fail instead of guessing the module.
        module_version="${aws_lc_version%%.*}"
        if [[ "${module_version}" != "3" && "${module_version}" != "4" ]]; then
            echo "error: ${base_h} doesn't define AWSLC_FIPS_VERSION_NUMBER, and AWS-LC ${aws_lc_version} is not" \
                "on the FIPS 3 or FIPS 4 branch; update this check to match how aws-lc-fips-sys identifies the" \
                "module." >&2
            exit 1
        fi
    fi

    crate="$(basename "$(dirname "${manifest_path}")")"
    min_version="$(certified_min_version "${module_version}")"
    if [[ -z "${min_version}" ]]; then
        echo "error: ${crate} bundles AWS-LC ${aws_lc_version} (FIPS module ${module_version}), and no release of FIPS" \
            "module ${module_version} is certified. See the aws-lc-rs comment in Cargo.toml." >&2
        status=1
    elif ! version_at_least "${aws_lc_version}" "${min_version}"; then
        echo "error: ${crate} bundles AWS-LC ${aws_lc_version} (FIPS module ${module_version}), which is older than" \
            "the certified release ${min_version}." >&2
        status=1
    else
        echo "${crate} bundles AWS-LC ${aws_lc_version} (FIPS module ${module_version}). The certified release on" \
            "this branch is ${min_version}."
    fi
done <<< "${manifest_paths}"

exit "${status}"
