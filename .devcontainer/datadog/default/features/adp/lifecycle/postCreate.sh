#!/usr/bin/env bash
# Runs as bits after the workspace is created and the repo is cloned.
set -o xtrace -o errexit -o nounset -o pipefail

export PATH="$HOME/.cargo/bin:$PATH"

cd ~/dd/saluki

# Install the exact Rust toolchain pinned in rust-toolchain.toml.
rustup show

# Install all cargo helper tools (versions defined in Makefile).
make cargo-preinstall
