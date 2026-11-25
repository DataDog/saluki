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
#   the build context, which includes the Dockerfile and the Rust toolchain version. The build
#   arguments include the build profile and target, the compiler cache settings, and the images
#   that the build starts from, resolved to digests because their tags move. These images include
#   the slim SMP build image, which has the toolchain and comes from the pipeline commit (see
#   `.resolve-smp-build-image`). The script hash covers the rest of the build recipe. The key
#   doesn't include the credentials for the compiler cache: they change on each run, and they don't
#   change the output.
# - SMP key: the ADP commit, the ADP image digest, the Agent image, which
#   `.datadog-agent-version.json` pins by digest, and the hash of this script. The converged build
#   takes its Dockerfile and its files from the ADP commit.
#
# Some inputs are outside the key, for example the packages that `apt-get install` gets during the
# build, and the BuildKit version. So the script reuses an image only while it is younger than
# `SMP_IMAGE_REUSE_MAX_AGE_DAYS`. Set `SMP_IMAGE_REUSE=false` to always build. If the script can't
# compute a key, it builds under a pipeline-scoped tag. If the registry lifecycle policy deletes a
# keyed image, the next run builds it again.
#
# Caveats:
#
# - The downstream jobs pull the ADP image by digest. If another pipeline pushes a new build under
#   the same tag, the old digest becomes untagged, and a lifecycle rule for untagged images can
#   delete it before those jobs pull it.
# - Every pipeline that runs this script can push a keyed tag, which other pipelines then reuse.
#   This adds no new access: the same pipelines can already push any image to this repository.
#   But a bad image under a keyed tag, for example from a changed copy of this script that keeps
#   the key of `main`, affects every PR and nightly run that reuses it, until it is older than the
#   age limit. Set `SMP_IMAGE_REUSE=false` to stop reuse while you remove such an image.
#
# Many pipelines can use one keyed image. So the labels on a keyed image identify the ADP commit and
# the key, and not the pipeline that built the image. An image under a pipeline-scoped tag also gets
# labels for its branch, pipeline, and job.
#
# Expects the environment from `.setup-smp-env` and `.smp-adp-image-build` in `.gitlab/benchmark.yml`,
# AWS credentials, a Docker login to `${SMP_ECR_HOST}`, the `adp-builder` builder, and
# `SMP_BUILD_IMAGE` from `.resolve-smp-build-image`.

set -euo pipefail

role="${1:?usage: build-smp-images.sh <baseline|comparison>}"
role_upper="$(echo "${role}" | tr '[:lower:]' '[:upper:]')"

reuse="${SMP_IMAGE_REUSE:-true}"
max_age_days="${SMP_IMAGE_REUSE_MAX_AGE_DAYS:-7}"
image_repo="${SMP_ECR_HOST}/${SMP_TEAM_ID}-saluki"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"

log() {
    echo "[build-smp-images] $*" >&2
}

die() {
    log "error: $*"
    exit 1
}

[[ "${max_age_days}" =~ ^[0-9]+$ ]] \
    || die "SMP_IMAGE_REUSE_MAX_AGE_DAYS must be a number of days, not '${max_age_days}'."

# Print the SHA-256 hash of stdin, as hex.
sha256() {
    sha256sum | cut -c1-64
}

recipe_hash="$(sha256 < "${BASH_SOURCE[0]}")"

# Print `<ref>@<digest>` for an image reference, with the digest that the tag points at now.
pin_digest() {
    local digest
    digest="$(docker buildx imagetools inspect "$1" --format '{{.Manifest.Digest}}')" || return 1
    [[ "${digest}" == sha256:* ]] || return 1
    echo "${1%@*}@${digest}"
}

# Succeed if the image exists and is younger than `max_age_days` days. If the script can't read the
# age of the image, it builds the image again.
fresh_image_exists() {
    local image="$1" created created_epoch age_days
    # A multi-platform index has one config for each platform, keyed by platform.
    created="$(docker buildx imagetools inspect "${image}" --format '{{json .Image}}' 2>/dev/null \
        | jq -er 'if has("created") then .created else ([.[].created] | max) end' 2>/dev/null)" || return 1
    if ! created_epoch="$(date -u -d "${created}" +%s 2>/dev/null)"; then
        log "${image} exists, but its creation time '${created}' isn't a date; rebuilding."
        return 1
    fi
    age_days=$(( ( $(date -u +%s) - created_epoch ) / 86400 ))
    if (( age_days >= max_age_days )); then
        log "${image} exists but is ${age_days} days old (limit ${max_age_days}); rebuilding."
        return 1
    fi
    log "${image} exists (built ${created}); reusing it."
}

# A plain assignment, so that `set -e` stops the script if compute-smp-sha.sh fails. The commit goes
# into the image tags and keys, so the script also stops if it isn't a full commit SHA.
adp_sha="$("${script_dir}/compute-smp-sha.sh" "${role}")"
[[ "${adp_sha}" =~ ^[0-9a-f]{40}$ ]] || die "compute-smp-sha.sh gave '${adp_sha}', not a commit SHA."

# Both sides run the Agent from the pipeline commit's `.datadog-agent-version.json`, so the comparison
# measures only the ADP change. The baseline commit can predate the file, so the script reads it from
# the pipeline checkout.
agent_image="$(jq -er '.integration.linux.image' .datadog-agent-version.json)"

build_image="${SMP_BUILD_IMAGE}"
buildcache_image="${SALUKI_BUILDCACHE_CI_IMAGE}"
spdx_image="${SPDX_LICENSES_IMAGE}"
app_image="${GBI_BASE_IMAGE}"

if [[ "${reuse}" == "true" ]]; then
    if [[ "${agent_image}" != *@sha256:* ]]; then
        log "Agent image ${agent_image} isn't pinned by digest; not reusing images."
        reuse=false
    elif pinned_build="$(pin_digest "${build_image}")" \
        && pinned_buildcache="$(pin_digest "${buildcache_image}")" \
        && pinned_spdx="$(pin_digest "${spdx_image}")" \
        && pinned_app="$(pin_digest "${app_image}")"; then
        # Build from the pinned references, so that the image matches its key if a tag moves during
        # the build.
        build_image="${pinned_build}"
        buildcache_image="${pinned_buildcache}"
        spdx_image="${pinned_spdx}"
        app_image="${pinned_app}"
    else
        log "Couldn't resolve the base images to digests; not reusing images."
        reuse=false
    fi
fi

# The ADP key includes all of these arguments, so a change to them makes a new key.
#
# We don't pass `APP_GIT_HASH`. `saluki-metadata` embeds it, so a new value on each commit
# invalidates that crate and every crate that depends on it in the compiler cache, including
# `saluki-components`. The binary then reports an unknown Git hash. The image tag and the
# `git.commit` label still identify the built commit.
adp_build_args=(
    "BUILD_PROFILE=${SMP_BUILD_PROFILE}"
    "BUILD_TARGET=${BUILD_TARGET}"
    "BUILD_IMAGE=${build_image}"
    "BUILD_BASE=build-prepped"
    "BUILDCACHE_IMAGE=${buildcache_image}"
    "BUILDCACHE_REMOTE=s3://s3.${BUILDCACHE_S3_REGION}.amazonaws.com/${BUILDCACHE_S3_BUCKET}"
    "SPDX_LICENSES_IMAGE=${spdx_image}"
    "APP_IMAGE=${app_image}"
)

if [[ "${reuse}" == "true" ]]; then
    adp_key="$(printf '%s\n' "v1" "${adp_sha}" "${adp_build_args[@]}" "${recipe_hash}" | sha256 | cut -c1-40)"
    adp_img="${image_repo}:adp-reuse-${adp_key}"
else
    adp_key="none"
    adp_img="${ADP_IMG_BASE}-${adp_sha}"
fi

log "${role}: ADP commit ${adp_sha}"
for arg in "${adp_build_args[@]}"; do
    log "${role}: ${arg}"
done
log "${role}: Agent image ${agent_image}"
log "${role}: ADP image ${adp_img}"

# Prepare a build, once. The build uses a separate worktree of the ADP commit, so that this script,
# the Agent version file, and the SMP build image stay at the pipeline commit.
src_dir=""
prepare_build() {
    [[ -z "${src_dir}" ]] || return 0
    src_dir="$(mktemp -d)/adp-src"
    git worktree add --detach "${src_dir}" "${adp_sha}" >&2
}

# Run `docker buildx build` and push the image. Arguments: the Dockerfile, the tag, the image key
# (`none` for a pipeline-scoped tag), any extra `docker buildx build` flags, `--`, and the build
# arguments.
build_and_push() {
    local dockerfile="$1" tag="$2" key="$3" arg label flags=()
    local labels=("git.repository=${CI_PROJECT_NAME}" "git.commit=${adp_sha}" "smp.image_key=${key}")
    shift 3
    while [[ "$1" != "--" ]]; do
        flags+=("$1")
        shift
    done
    shift
    if [[ "${key}" == "none" ]]; then
        labels+=("git.branch=${CI_COMMIT_REF_NAME}" "ci.pipeline_id=${CI_PIPELINE_ID}" "ci.job_id=${CI_JOB_ID}")
    fi
    for arg in "$@"; do
        flags+=(--build-arg "${arg}")
    done
    for label in "${labels[@]}"; do
        flags+=(--label "${label}")
    done
    prepare_build
    log "Building ${tag}..."
    SECONDS=0
    docker buildx build --file "${src_dir}/docker/${dockerfile}" --tag "${tag}" "${flags[@]}" --push "${src_dir}"
    log "Built ${tag} in ${SECONDS}s."
}

if [[ "${reuse}" != "true" ]] || ! fresh_image_exists "${adp_img}"; then
    # Temporary credentials for the shared compiler cache, from the IAM role of the runner and not
    # from the SMP profile. A build secret keeps them out of the image layers.
    "${script_dir}/get-temporary-aws-credentials.sh" >&2
    build_and_push Dockerfile.agent-data-plane "${adp_img}" "${adp_key}" \
        --secret id=aws-creds,src=/tmp/temporary-aws-credentials.sh -- "${adp_build_args[@]}"
fi

# The converged image refers to the ADP image by digest, and the SMP key includes that digest. So a
# rebuild of the ADP image under the same tag makes a new SMP key, and the downstream jobs get the
# same ADP build.
adp_ref="${adp_img}"
if [[ "${reuse}" == "true" ]]; then
    if adp_ref="$(pin_digest "${adp_img}")"; then
        smp_build_args=("DD_AGENT_IMAGE=${agent_image}" "ADP_IMAGE=${adp_ref}")
        smp_key="$(printf '%s\n' "v3" "${adp_sha}" "${smp_build_args[@]}" "${recipe_hash}" | sha256 | cut -c1-40)"
        smp_img="${image_repo}:agent-adp-reuse-${smp_key}"
    else
        log "Couldn't resolve ${adp_img} to a digest; not reusing the converged image."
        adp_ref="${adp_img}"
        reuse=false
    fi
fi
if [[ "${reuse}" != "true" ]]; then
    smp_build_args=("DD_AGENT_IMAGE=${agent_image}" "ADP_IMAGE=${adp_ref}")
    smp_key="none"
    smp_img="${SMP_IMG_BASE}-${adp_sha}"
fi
log "${role}: SMP image ${smp_img}, from ${adp_ref}"

# SMP runs one target image in each job, and the tag filtering experiments need the Core Agent: a
# `metric_tag_filterlist` is Agent configuration, and reaches ADP only over the config stream. So
# every experiment runs against the converged image. The experiments that don't need the Core Agent
# override the command to run ADP by itself.
if [[ "${reuse}" != "true" ]] || ! fresh_image_exists "${smp_img}"; then
    build_and_push Dockerfile.datadog-agent "${smp_img}" "${smp_key}" -- "${smp_build_args[@]}"
fi

{
    echo "${role_upper}_ADP_SHA=${adp_sha}"
    echo "${role_upper}_ADP_IMG=${adp_ref}"
    echo "${role_upper}_SMP_IMG=${smp_img}"
} >> smp-vars.env
