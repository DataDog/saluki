#!/usr/bin/env bash
# Runs as the bits user
set -o xtrace -o errexit -o nounset

# Install rustup
# The rust version is pinned in rust-toolchain.toml
# and will be installed by `rustup show` in postCreate
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- --no-modify-path --default-toolchain none -y

export PATH="$HOME/.cargo/bin:$PATH"


