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
    build-essential \
    ca-certificates \
    musl-dev \
    musl-tools \
    linux-libc-dev \
    make \
    cmake \
    gcc \
    g++ \
    perl \
    golang-go \
    protobuf-compiler \
    curl \
    unzip \
    rustup

rustup set profile minimal
