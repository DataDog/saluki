#!/usr/bin/env bash
# Runs as the bits user. Installs Rust toolchain and cargo-binstall.
# Cargo helper tools are installed later via `make cargo-preinstall` in the
# postCreateCommand (once the repo is available).
set -o xtrace -o errexit -o nounset

# Install rustup with the stable toolchain.
# The exact version (+ clippy/rustfmt) is pinned in rust-toolchain.toml and
# will be installed automatically on first cargo invocation in the workspace.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- --no-modify-path --default-toolchain stable -y

export PATH="$HOME/.cargo/bin:$PATH"

# Install cargo-binstall by downloading the pre-built binary directly.
# This avoids compilation issues with pinned source versions vs changing Rust
# toolchains. Version should be kept in sync with CARGO_TOOL_VERSION_cargo-binstall
# in the Makefile.
BINSTALL_VERSION="1.17.7"
BINSTALL_ARCH="$(uname -m)"
BINSTALL_TARBALL="cargo-binstall-${BINSTALL_ARCH}-unknown-linux-musl.tgz"
curl -L --proto '=https' --tlsv1.2 -sSf \
    "https://github.com/cargo-bins/cargo-binstall/releases/download/v${BINSTALL_VERSION}/${BINSTALL_TARBALL}" \
    | tar -xz -C "$HOME/.cargo/bin"
