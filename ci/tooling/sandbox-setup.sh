#!/usr/bin/env bash
#
# Prepares a remote agent sandbox (e.g. Bits Code) to build and test this repository. Run it while the
# sandbox still has network access. Afterwards, `CARGO_NET_OFFLINE=true` builds and tests need no Git
# remotes and no rustup downloads, as long as `Cargo.lock` does not change.

set -euo pipefail

cd "$(dirname "$0")/../.."

# Keep the setup runner from timing out during long, quiet steps.
(while sleep 20; do echo '[setup] Still working...'; done) &
heartbeat_pid=$!
trap 'kill "$heartbeat_pid"' EXIT

echo '[setup] Installing the pinned Rust toolchain'
rustup toolchain install
rustup show active-toolchain

echo '[setup] Installing protoc, vale and kind'
bash ci/images/scripts/install-protoc.sh
bash ci/images/scripts/install-vale.sh
bash ci/images/scripts/install-kind.sh

echo '[setup] Installing the nightly toolchains, Miri and Cargo tools'
make ensure-rust-miri ensure-rust-aix-nightly cargo-preinstall

echo '[setup] Installing Python tooling'
make ensure-python-venv

echo '[setup] Fetching Cargo and documentation dependencies'
# Sandboxes often rewrite GitHub URLs to a Git proxy that rejects some of our Git dependencies. All of
# them are public, so fetch them directly. Cargo only honors `GIT_CONFIG_GLOBAL` when it fetches with
# the Git CLI.
GIT_CONFIG_GLOBAL=/dev/null CARGO_NET_GIT_FETCH_WITH_CLI=true cargo fetch --locked
bun install --frozen-lockfile
vale sync

echo '[setup] Ready'
