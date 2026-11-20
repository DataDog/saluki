#!/usr/bin/env bash
#
# Build, or reuse, the ADP image and the converged Agent image for one side of an SMP run.
#
# Usage: build-smp-images.sh <baseline|comparison>
#
# Writes `<ROLE>_ADP_SHA`, `<ROLE>_ADP_IMG`, and `<ROLE>_SMP_IMG` to `smp-vars.env`, for the
# downstream benchmark and binary size jobs.
#
# An `optimized-release` build takes about 15 minutes, and many runs rebuild an image that already
# exists. For example, every push to a PR rebuilds the same merge-base baseline. So the script tags
# each image with a key over the inputs to its build, and skips the build when the registry already
# has an image with that key:
#
# - ADP key: the ADP commit, the ADP build arguments, and the hash of this script. The commit fixes
#   the build context, which includes the Dockerfile and the Rust toolchain. The build arguments
#   include the build profile and target, and the base images resolved to digests, because their
#   tags move. The script hash covers the rest of the build recipe.
# - SMP key: the ADP key and the Agent image, which `.datadog-agent-version.json` pins by digest.
#
# Some inputs are outside the key, for example the packages that `apt-get install` gets during the
# build, and the BuildKit version. So the script reuses an image only while it is younger than
# `SMP_IMAGE_REUSE_MAX_AGE_DAYS`. Set `SMP_IMAGE_REUSE=false` to always build. If the script can't
# compute a key, it builds under a pipeline-scoped tag. If the registry lifecycle policy deletes a
# keyed image, the next run builds it again.
#
# Many pipelines can use one keyed image. So the image labels identify the ADP commit and the key,
# and not the pipeline that built the image.
#
# Expects the environment from `.setup-smp-env` in `.gitlab/benchmark.yml`, AWS credentials, and a
# Docker login to `${SMP_ECR_HOST}`.

set -euo pipefail

role="${1:?usage: build-smp-images.sh <baseline|comparison>}"
role_upper="$(echo "${role}" | tr '[:lower:]' '[:upper:]')"

reuse="${SMP_IMAGE_REUSE:-true}"
max_age_days="${SMP_IMAGE_REUSE_MAX_AGE_DAYS:-7}"
build_target="x86_64-unknown-linux-gnu"
image_repo="${SMP_ECR_HOST}/${SMP_TEAM_ID}-saluki"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
recipe_hash="$(sha256sum "${BASH_SOURCE[0]}" | cut -c1-64)"

log() {
    echo "[build-smp-images] $*" >&2
}

# Print the SHA-256 hash of stdin, as hex.
sha256() {
    sha256sum | cut -c1-64
}

# Print `<ref>@<digest>` for an image reference, with the digest that the tag points at now.
pin_digest() {
    local digest
    digest="$(docker buildx imagetools inspect "$1" --format '{{.Manifest.Digest}}')" || return 1
    [[ "${digest}" == sha256:* ]] || return 1
    echo "${1%@*}@${digest}"
}

# Succeed if the image exists and is younger than `max_age_days` days.
fresh_image_exists() {
    local image="$1" created age_days
    # A multi-platform index has one config for each platform, keyed by platform.
    created="$(docker buildx imagetools inspect "${image}" --format '{{json .Image}}' 2>/dev/null \
        | jq -er 'if has("created") then .created else ([.[].created] | max) end' 2>/dev/null)" || return 1
    age_days=$(( ( $(date -u +%s) - $(date -u -d "${created}" +%s) ) / 86400 ))
    if (( age_days >= max_age_days )); then
        log "${image} exists but is ${age_days} days old (limit ${max_age_days}); rebuilding."
        return 1
    fi
    log "${image} exists (built ${created}); reusing it."
}

adp_sha="$("${script_dir}/compute-smp-sha.sh" "${role}")"

# Both sides run the Agent from the pipeline commit's `.datadog-agent-version.json`, so the comparison
# measures only the ADP change. The baseline commit can predate the file, so the script reads it from
# the pipeline checkout.
agent_image="$(jq -er '.integration.linux.image' .datadog-agent-version.json)"

build_image="${ADP_PUBLIC_BASE_IMAGE}"
spdx_image="${SPDX_LICENSES_IMAGE}"
app_image="${GBI_BASE_IMAGE}"

if [[ "${reuse}" == "true" ]]; then
    if [[ "${agent_image}" != *@sha256:* ]]; then
        log "Agent image ${agent_image} isn't pinned by digest; not reusing images."
        reuse=false
    elif pinned_build="$(pin_digest "${build_image}")" \
        && pinned_spdx="$(pin_digest "${spdx_image}")" \
        && pinned_app="$(pin_digest "${app_image}")"; then
        # Build from the pinned references, so that the image matches its key if a tag moves during
        # the build.
        build_image="${pinned_build}"
        spdx_image="${pinned_spdx}"
        app_image="${pinned_app}"
    else
        log "Couldn't resolve the base images to digests; not reusing images."
        reuse=false
    fi
fi

# The ADP key includes all of these arguments, so a change to them makes a new key.
adp_build_args=(
    "BUILD_PROFILE=${SMP_BUILD_PROFILE}"
    "BUILD_TARGET=${build_target}"
    "BUILD_IMAGE=${build_image}"
    "SPDX_LICENSES_IMAGE=${spdx_image}"
    "APP_IMAGE=${app_image}"
    "APP_GIT_HASH=${adp_sha}"
)
smp_build_args=(
    "DD_AGENT_IMAGE=${agent_image}"
)

if [[ "${reuse}" == "true" ]]; then
    adp_key="$(printf '%s\n' "v1" "${adp_sha}" "${adp_build_args[@]}" "${recipe_hash}" | sha256 | cut -c1-40)"
    smp_key="$(printf '%s\n' "v1" "${adp_key}" "${smp_build_args[@]}" | sha256 | cut -c1-40)"
    adp_img="${image_repo}:adp-reuse-${adp_key}"
    smp_img="${image_repo}:agent-adp-reuse-${smp_key}"
else
    adp_key="none"
    smp_key="none"
    adp_img="${ADP_IMG_BASE}-${adp_sha}"
    smp_img="${SMP_IMG_BASE}-${adp_sha}"
fi
smp_build_args+=("ADP_IMAGE=${adp_img}")

log "${role}: ADP commit ${adp_sha}"
for arg in "${adp_build_args[@]}" "${smp_build_args[@]}"; do
    log "${role}: ${arg}"
done
log "${role}: ADP image ${adp_img}"
log "${role}: SMP image ${smp_img}"

need_adp=true
need_smp=true
if [[ "${reuse}" == "true" ]]; then
    fresh_image_exists "${adp_img}" && need_adp=false
    # Reuse a converged image only together with the ADP image that it contains.
    if [[ "${need_adp}" == "false" ]] && fresh_image_exists "${smp_img}"; then
        need_smp=false
    fi
fi

if [[ "${need_adp}" == "true" || "${need_smp}" == "true" ]]; then
    # Build from a separate worktree of the ADP commit, so that this script and the Agent version file
    # stay at the pipeline commit.
    src_dir="$(mktemp -d)/adp-src"
    git worktree add --detach "${src_dir}" "${adp_sha}" >&2
    docker buildx create --name adp-builder --driver docker-container --use >&2
fi

if [[ "${need_adp}" == "true" ]]; then
    log "Building ${adp_img}..."
    SECONDS=0
    adp_flags=()
    for arg in "${adp_build_args[@]}"; do
        adp_flags+=(--build-arg "${arg}")
    done
    for label in "git.repository=${CI_PROJECT_NAME}" "git.commit=${adp_sha}" "smp.image_key=${adp_key}"; do
        adp_flags+=(--label "${label}")
    done
    docker buildx build \
        --file "${src_dir}/docker/Dockerfile.agent-data-plane" \
        --tag "${adp_img}" \
        "${adp_flags[@]}" \
        --push \
        "${src_dir}"
    log "Built ${adp_img} in ${SECONDS}s."
fi

# SMP runs one target image in each job, and the tag filtering experiments need the Core Agent: a
# `metric_tag_filterlist` is Agent configuration, and reaches ADP only over the config stream. So
# every experiment runs against the converged image. The experiments that don't need the Core Agent
# override the command to run ADP by itself.
if [[ "${need_smp}" == "true" ]]; then
    log "Building ${smp_img}..."
    SECONDS=0
    smp_flags=()
    for arg in "${smp_build_args[@]}"; do
        smp_flags+=(--build-arg "${arg}")
    done
    for label in "git.repository=${CI_PROJECT_NAME}" "git.commit=${adp_sha}" "smp.image_key=${smp_key}"; do
        smp_flags+=(--label "${label}")
    done
    docker buildx build \
        --file "${src_dir}/docker/Dockerfile.datadog-agent" \
        --tag "${smp_img}" \
        "${smp_flags[@]}" \
        --push \
        "${src_dir}"
    log "Built ${smp_img} in ${SECONDS}s."
fi

{
    echo "${role_upper}_ADP_SHA=${adp_sha}"
    echo "${role_upper}_ADP_IMG=${adp_img}"
    echo "${role_upper}_SMP_IMG=${smp_img}"
} >> smp-vars.env
