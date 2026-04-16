#!/usr/bin/env bash
# Runs as the bits user. Installs Rust toolchain and cargo-binstall.
# Cargo helper tools are installed later via `make cargo-preinstall` in the
# postCreateCommand (once the repo is available).
set -o xtrace -o errexit -o nounset

# Install rustup
# The rust version is pinned in rust-toolchain.toml
# and will be installed by `rustup show` in postCreate
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- --no-modify-path --default-toolchain none -y

export PATH="$HOME/.cargo/bin:$PATH"


