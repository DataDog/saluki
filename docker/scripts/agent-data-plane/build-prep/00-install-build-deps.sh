#!/usr/bin/env sh
#
# Installs the system build toolchain for compiling Agent Data Plane.
#
# This includes musl cross-compilation support (musl-dev/musl-tools), the kernel headers AWS-LC needs
# (linux-libc-dev), the Go toolchain required to build AWS-LC in FIPS mode (golang-go), and protoc.
# Finally, it switches rustup to the minimal profile so toolchain installs stay lean.

set -eu

apt-get update
apt-get install --no-install-recommends -y \
    build-essential=12.12ubuntu2.26.04.1 \
    ca-certificates=20260601~26.04.1 \
    musl-dev=1.2.5-3build1 \
    musl-tools=1.2.5-3build1 \
    linux-libc-dev=7.0.0-27.27 \
    make=4.4.1-3 \
    cmake=4.2.3-2ubuntu2 \
    gcc=4:15.2.0-5ubuntu1 \
    g++=4:15.2.0-5ubuntu1 \
    perl=5.40.1-7ubuntu0.1 \
    golang-go=2:1.26~1 \
    protobuf-compiler=3.21.12-15ubuntu1 \
    curl=8.18.0-1ubuntu2.2 \
    unzip=6.0-29ubuntu1 \
    rustup=1.27.1-8

rustup set profile minimal
