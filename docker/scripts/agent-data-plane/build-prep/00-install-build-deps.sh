#!/usr/bin/env sh
#
# Installs the system build toolchain for compiling Agent Data Plane.
#
# This includes the kernel headers AWS-LC needs (linux-libc-dev), the Go toolchain required to build
# AWS-LC in FIPS mode (golang-go), and protoc. Finally, it switches rustup to the minimal profile so
# toolchain installs stay lean.
#
# This path is used by local `make build-adp-image*` builds and the benchmark job, which build on a
# plain Ubuntu base for the native host/gnu target. Shipped artifacts are built on the Datadog Agent
# build image instead (see ci/images/definitions/build/Dockerfile), which provides the old-glibc
# crosstool-NG toolchain; nothing musl-specific is needed here.

set -eu

apt-get update
apt-get install --no-install-recommends -y \
    build-essential \
    ca-certificates \
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
