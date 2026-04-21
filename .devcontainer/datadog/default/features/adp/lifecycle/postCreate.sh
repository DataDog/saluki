#!/usr/bin/env bash
# Runs as bits after the workspace is created and the repo is cloned.
set -o xtrace -o errexit -o nounset -o pipefail

export PATH="$HOME/.cargo/bin:$PATH"

cd "$HOME/dd/saluki"

# Install the exact Rust toolchain pinned in rust-toolchain.toml.
rustup toolchain install

# Install all cargo helper tools
make check-rust-build-tools
make cargo-preinstall
