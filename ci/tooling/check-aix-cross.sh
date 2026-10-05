#!/usr/bin/env bash
#
# Fails if `agent-data-plane` doesn't compile for AIX, by type-checking it for `powerpc64-ibm-aix` from a non-AIX host.
#
# ADP is only built for AIX natively, using the IBM Open SDK for Rust (see `build-adp-aix.sh`), and that build happens
# downstream of us. This check catches Rust code, whether ours or a dependency's, that doesn't compile for AIX before it
# gets that far: unsupported platform APIs, missing `cfg` gates, and so on. It's a `cargo check`, so it never generates
# code or links, and can't catch problems that only show up at those stages.
#
# Checking for AIX from another host needs a few workarounds:
#
# - AIX is a tier 3 target with no prebuilt standard library, so we build it from source with `-Zbuild-std`. That needs a
#   nightly toolchain with the `rust-src` component, and a newer one than `RUST_NIGHTLY_VERSION`: neither it nor our
#   stable toolchain can build the AIX standard library from source.
# - `aws-lc-sys` and `zstd-sys` compile C code in their build scripts, which needs an AIX C toolchain and sysroot. We
#   override those build scripts for the AIX target instead, and provide what the Rust code needs from them in their
#   place. In this configuration both crates use their pregenerated bindings rather than running bindgen, so that's only
#   the `cfg` that `aws-lc-sys` uses to select its bindings and the `include` metadata that `aws-lc-rs` expects. The
#   libraries they declare don't need to exist, because `cargo check` never links.
# - We cap lints at warnings. This check is only about compile errors, and the newer nightly's lints (new deprecations,
#   for example) would otherwise trip `#![deny(warnings)]` in crates that are lint-free on our stable toolchain.

set -euo pipefail

AIX_TARGET="powerpc64-ibm-aix"
RUST_AIX_NIGHTLY_VERSION="${RUST_AIX_NIGHTLY_VERSION:?RUST_AIX_NIGHTLY_VERSION must be set to the nightly toolchain to use}"

echo "[*] Checking that agent-data-plane compiles for AIX (${AIX_TARGET})..."

# Build script overrides are keyed by the package's `links` value, which for `aws-lc-sys` includes its version, so we
# look up the current values rather than hardcoding them.
metadata="$(cargo metadata --format-version 1 --locked --filter-platform "${AIX_TARGET}")"

links_values() {
    jq -r --arg package "$1" '.packages[] | select(.name == $package) | .links // empty' <<< "${metadata}" | sort -u
}

config_args=(--config 'build.rustflags=["--cap-lints=warn"]')

for links in $(links_values aws-lc-sys); do
    # `universal` is the `cfg` that the `aws-lc-sys` build script emits for AIX.
    config_args+=(
        --config "target.${AIX_TARGET}.${links}.rustc-cfg=[\"universal\"]"
        --config "target.${AIX_TARGET}.${links}.include=\"\""
    )
done

for links in $(links_values zstd-sys); do
    config_args+=(--config "target.${AIX_TARGET}.${links}.rustc-link-lib=[\"zstd\"]")
done

# The profile matches what `build-adp-aix.sh` builds with, so code gated on `debug_assertions` is checked the same way.
cargo "+${RUST_AIX_NIGHTLY_VERSION}" check --locked \
    --target "${AIX_TARGET}" -Zbuild-std=std \
    --profile aix-optimized-release --bin agent-data-plane \
    "${config_args[@]}"
