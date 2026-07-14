#!/usr/bin/env bash
#
# Installs rustup and the various toolchains required for tests and builds.
#
set -euo pipefail
set -x

# Install rustup itself, but without any toolchains.
curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain none

# Update our path so we can use Cargo commands.
export PATH="/root/.cargo/bin:${PATH}"

# Install the default toolchain, which is provided via the `RUST_VERSION` environment variable. The
# host gnu target it installs is the one we build ADP for; the old-glibc floor comes from the build
# image's crosstool-NG toolchain at link/compile time, not from a separate Rust target.
rustup toolchain install --profile minimal --component clippy,rustfmt ${RUST_VERSION}

# Install `rustfmt` via the nightly toolchain.
#
# We do manual cleanup afterwards to remove most of the actual toolchain bits, since all we care about
# is `rustfmt` itself.
rustup toolchain install --profile minimal --component rustfmt nightly
rm -rf /root/.rustup/toolchains/nightly-*/lib/rustlib/*/lib
