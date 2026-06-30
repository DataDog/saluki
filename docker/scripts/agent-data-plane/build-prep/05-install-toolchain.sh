#!/usr/bin/env sh
#
# Pre-installs the pinned Rust toolchain.
#
# The Dockerfile bind-mounts `rust-toolchain.toml` into /tmp for this step, so the layer's cache key
# is just that file's content -- code changes do not reinstall the toolchain. Running
# `rustup show active-toolchain` from a directory containing the manifest triggers the install.

set -eu

cd /tmp
rustup show active-toolchain
