#!/usr/bin/env bash
# Uploads the ELF debug symbols of a released agent-data-plane build to Datadog so that native profiles can be
# symbolicated. Downloads the linux amd64 and arm64 release tarballs from binaries.ddbuild.io and uploads the
# binaries with `datadog-ci elf-symbols upload`, once to the default org (via `dd-auth`) and once to ddstaging.
#
# Usage: upload-adp-symbols.sh [VERSION]
#
# VERSION defaults to the latest GitHub release of DataDog/saluki (e.g. `1.7.2`).

set -euo pipefail

BINARIES_BASE_URL="https://binaries.ddbuild.io/saluki"
ARCHES=(amd64 arm64)
DD_AUTH_DOMAINS=("" "ddstaging.datadoghq.com")

for cmd in curl tar npx dd-auth; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "${cmd} not found" >&2; exit 1; }
done

if [[ $# -gt 1 ]]; then
    echo "Usage: $0 [VERSION]" >&2
    exit 1
fi

version="${1:-}"
if [[ -z "$version" ]]; then
    command -v gh >/dev/null 2>&1 || { echo "gh not found; pass VERSION explicitly" >&2; exit 1; }
    version="$(gh release view -R DataDog/saluki --json tagName -q .tagName)"
    echo "Using latest released version: ${version}"
fi
# Accept `v1.7.2` as well as `1.7.2`.
version="${version#v}"

workdir="$(mktemp -d -t adp-symbols-XXXXXX)"
trap 'rm -rf "$workdir"' EXIT

binaries=()
for arch in "${ARCHES[@]}"; do
    url="${BINARIES_BASE_URL}/agent-data-plane-${version}-linux-${arch}.tar.gz"
    dest="${workdir}/${version}-${arch}"
    mkdir -p "$dest"
    echo "Downloading ${url}..."
    curl -sSfL -o "${dest}.tar.gz" "$url"
    tar -C "$dest" -xzf "${dest}.tar.gz"
    binaries+=("${dest}/opt/datadog-agent/embedded/bin/agent-data-plane")
done

for domain in "${DD_AUTH_DOMAINS[@]}"; do
    dd_auth_args=()
    [[ -n "$domain" ]] && dd_auth_args=(--domain "$domain")
    echo "Uploading symbols for agent-data-plane ${version} to ${domain:-default org}..."
    dd-auth "${dd_auth_args[@]}" -- env DD_BETA_COMMANDS_ENABLED=1 \
        npx -y @datadog/datadog-ci@v5 elf-symbols upload --disable-git "${binaries[@]}"
done
