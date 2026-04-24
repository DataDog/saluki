#!/usr/bin/env bash
#
# Cleans up temporary caches (Apt, Cargo, etc) in a Docker builder.
#
# Designed to be called at the end of a RUN command.
#
set -euo pipefail
set -x

# Clean up APT-related files.
apt-get clean
rm -rf /var/lib/apt/lists/*

# Clean up Cargo-related files.
rm -rf /root/.cargo/registry
rm -rf /root/.rustup/toolchains/nightly-*/lib

# General cleanup of documentation and locale files and things we don't need
# on a build container.
rm -rf /usr/share/doc/* /usr/share/man/* /usr/share/locale/*
