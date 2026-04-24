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

# First, install the default toolchain, which is provided via the `RUST_VERSION` environment variable.
#
# We pull in `clippy` to support tests, and we add a musl target to support statically linked builds.
rustup toolchain install --profile minimal --component clippy ${RUST_VERSION}
musl_target=$(rustc -vV | grep 'host:' | awk '{print $2}' | sed 's/linux-gnu/linux-musl/')
rustup target add --toolchain ${RUST_VERSION} ${musl_target}

# Install `rustfmt` via the nightly toolchain.
#
# We do manual cleanup afterwards to remove most of the actual toolchain bits, since all we care about
# is `rustfmt` itself.
rustup toolchain install --profile minimal --component rustfmt nightly
rm -rf /root/.rustup/toolchains/nightly-*/lib/rustlib/*/lib
