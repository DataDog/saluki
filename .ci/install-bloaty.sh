#!/bin/bash
set -euo pipefail

# Build and install bloaty from source.
# Bloaty does not provide pre-built binaries.

BLOATY_VERSION="1.1"

# Install build dependencies.
apt-get update
apt-get install -y --no-install-recommends cmake g++ ninja-build

# Download and extract source.
curl -L -o /tmp/bloaty.tar.bz2 "https://github.com/google/bloaty/releases/download/v${BLOATY_VERSION}/bloaty-${BLOATY_VERSION}.tar.bz2"
tar -C /tmp -xf /tmp/bloaty.tar.bz2 --no-same-owner

# Build bloaty.
cd "/tmp/bloaty-${BLOATY_VERSION}"
cmake -B build -G Ninja -DCMAKE_BUILD_TYPE=Release
cmake --build build --target bloaty

# Install to /usr/local/bin.
cp build/bloaty /usr/local/bin/bloaty
chmod +x /usr/local/bin/bloaty

# Clean up build dependencies and source.
cd /
rm -rf "/tmp/bloaty-${BLOATY_VERSION}" /tmp/bloaty.tar.bz2
apt-get purge -y cmake g++ ninja-build
apt-get autoremove -y
apt-get clean
