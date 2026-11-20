#!/usr/bin/env bash
#
# Build, or reuse, the ADP image and the converged Agent image for one side of an SMP run.
#
# Usage: build-smp-images.sh <baseline|comparison>
#
# Writes `<ROLE>_ADP_SHA`, `<ROLE>_ADP_IMG`, and `<ROLE>_SMP_IMG` to `smp-vars.env`, for the
# downstream benchmark and binary size jobs.
#
# An `optimized-release` build takes about 15 minutes, and most runs rebuild an image that already
# exists: every push to a PR rebuilds the same merge-base baseline, and each nightly rebuilds the
# previous nightly's comparison as its baseline. So each image is tagged with a key over everything
# that goes into it, and a build is skipped when an image with that key is already in the registry:
#
# - ADP key: the ADP commit (which fixes the whole build context, including the Dockerfile and the
#   Rust toolchain), the build profile and target, the base images resolved to digests (their tags
#   float), and the hash of this script (which holds the rest of the build recipe).
# - SMP key: the ADP key and the Agent image, which `.datadog-agent-version.json` pins by digest.
#
# The build runs `apt-get install` against Ubuntu's archive, which the key can't capture, so an
# image is only reused while it is younger than `SMP_IMAGE_REUSE_MAX_AGE_DAYS`. Set
# `SMP_IMAGE_REUSE=false` to always build. Any failure to compute the key or inspect the registry
# falls back to building, under a pipeline-scoped tag.
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

sha256() {
    sha256sum | cut -c1-64
}

# Print `<ref>@<digest>` for an image reference, pinning a floating tag to what it points at now.
pin_digest() {
    local digest
    digest="$(docker buildx imagetools inspect "$1" --format '{{.Manifest.Digest}}')" || return 1
    [[ "${digest}" == sha256:* ]] || return 1
    echo "${1%@*}@${digest}"
}

# Succeed if the image exists and was built within the last `max_age_days` days.
fresh_image_exists() {
    local image="$1" created age_days
    # A multi-platform index reports one config per platform, keyed by platform.
    created="$(docker buildx imagetools inspect "${image}" --format '{{json .Image}}' 2>/dev/null \
        | jq -er 'if has("created") then .created else ([.[].created] | max) end' 2>/dev/null)" || return 1
    age_days=$(( ( $(date -u +%s) - $(date -u -d "${created}" +%s) ) / 86400 ))
    if (( age_days >= max_age_days )); then
        log "${image} exists but was built ${age_days} days ago (limit ${max_age_days}); rebuilding."
        return 1
    fi
    log "${image} exists (built ${created}); reusing it."
}

adp_sha="$("${script_dir}/compute-smp-sha.sh" "${role}")"

# Both sides run the Agent from the pipeline commit's `.datadog-agent-version.json`, so the comparison
# measures only the ADP change. The baseline commit may predate the file, so read it before checking
# anything else out.
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
        # Build from the pinned references, so the image matches its key even if a tag moves mid-build.
        build_image="${pinned_build}"
        spdx_image="${pinned_spdx}"
        app_image="${pinned_app}"
    else
        log "Couldn't resolve the base images to digests; not reusing images."
        reuse=false
    fi
fi

if [[ "${reuse}" == "true" ]]; then
    adp_key="$(printf '%s\n' "v1" "${adp_sha}" "${SMP_BUILD_PROFILE}" "${build_target}" \
        "${build_image}" "${spdx_image}" "${app_image}" "${recipe_hash}" | sha256 | cut -c1-40)"
    smp_key="$(printf '%s\n' "v1" "${adp_key}" "${agent_image}" | sha256 | cut -c1-40)"
    adp_img="${image_repo}:adp-reuse-${adp_key}"
    smp_img="${image_repo}:agent-adp-reuse-${smp_key}"
else
    adp_img="${ADP_IMG_BASE}-${adp_sha}"
    smp_img="${SMP_IMG_BASE}-${adp_sha}"
fi

log "${role}: ADP commit ${adp_sha}, profile ${SMP_BUILD_PROFILE}, target ${build_target}"
log "${role}: build image ${build_image}"
log "${role}: SPDX image ${spdx_image}"
log "${role}: app image ${app_image}"
log "${role}: Agent image ${agent_image}"
log "${role}: ADP image ${adp_img}"
log "${role}: SMP image ${smp_img}"

need_adp=true
need_smp=true
if [[ "${reuse}" == "true" ]]; then
    fresh_image_exists "${adp_img}" && need_adp=false
    # A converged image is only reused alongside the ADP image it was built from.
    if [[ "${need_adp}" == "false" ]] && fresh_image_exists "${smp_img}"; then
        need_smp=false
    fi
fi

if [[ "${need_adp}" == "true" || "${need_smp}" == "true" ]]; then
    # Build from a separate worktree of the ADP commit, so this script and the Agent version file stay
    # at the pipeline commit.
    src_dir="$(mktemp -d)/adp-src"
    git worktree add --detach "${src_dir}" "${adp_sha}" >&2
    docker buildx create --name adp-builder --driver docker-container --use >&2
fi

if [[ "${need_adp}" == "true" ]]; then
    log "Building ${adp_img}..."
    SECONDS=0
    docker buildx build \
        --file "${src_dir}/docker/Dockerfile.agent-data-plane" \
        --tag "${adp_img}" \
        --build-arg "BUILD_PROFILE=${SMP_BUILD_PROFILE}" \
        --build-arg "BUILD_TARGET=${build_target}" \
        --build-arg "BUILD_IMAGE=${build_image}" \
        --build-arg "SPDX_LICENSES_IMAGE=${spdx_image}" \
        --build-arg "APP_IMAGE=${app_image}" \
        --build-arg "APP_GIT_HASH=${adp_sha}" \
        --label "git.repository=${CI_PROJECT_NAME}" \
        --label "git.branch=${CI_COMMIT_REF_NAME}" \
        --label "git.commit=${CI_COMMIT_SHA}" \
        --label "ci.pipeline_id=${CI_PIPELINE_ID}" \
        --label "ci.job_id=${CI_JOB_ID}" \
        --push \
        "${src_dir}"
    log "Built ${adp_img} in ${SECONDS}s."
fi

# SMP fixes one target image per job, and the tag filtering experiments need the Core Agent in the
# picture: a `metric_tag_filterlist` is Agent configuration, and only reaches ADP over the config
# stream. So every experiment runs against the converged image; the ones that don't care about the
# Core Agent override the command to run ADP by itself.
if [[ "${need_smp}" == "true" ]]; then
    log "Building ${smp_img}..."
    SECONDS=0
    docker buildx build \
        --file "${src_dir}/docker/Dockerfile.datadog-agent" \
        --tag "${smp_img}" \
        --build-arg "DD_AGENT_IMAGE=${agent_image}" \
        --build-arg "ADP_IMAGE=${adp_img}" \
        --label "git.repository=${CI_PROJECT_NAME}" \
        --label "git.branch=${CI_COMMIT_REF_NAME}" \
        --label "git.commit=${CI_COMMIT_SHA}" \
        --label "ci.pipeline_id=${CI_PIPELINE_ID}" \
        --label "ci.job_id=${CI_JOB_ID}" \
        --push \
        "${src_dir}"
    log "Built ${smp_img} in ${SECONDS}s."
fi

{
    echo "${role_upper}_ADP_SHA=${adp_sha}"
    echo "${role_upper}_ADP_IMG=${adp_img}"
    echo "${role_upper}_SMP_IMG=${smp_img}"
} >> smp-vars.env
