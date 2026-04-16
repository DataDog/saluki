# GBI images stand for "golden base image"
# Documentation: https://datadoghq.atlassian.net/wiki/x/V4Quu
# Available images: https://github.com/DataDog/dd-source/tree/main/etc/images/base
FROM registry.ddbuild.io/images/base/gbi-ubuntu_2404:release AS base

USER root

# System-level build dependencies for ADP.
# Packages are listed alphabetically, one per line.
RUN apt-get update \
    && export DEBIAN_FRONTEND=noninteractive \
    && apt-get -y install --no-install-recommends \
        binutils \
        build-essential \
        ca-certificates \
        cmake \
        curl \
        g++ \
        gcc \
        git \
        jq \
        make \
        pkg-config \
        unzip \
    && rm -rf /var/lib/apt/lists/*

# Install protoc as in CI
COPY .ci/install-protoc.sh /tmp/install-protoc.sh
RUN chmod +x /tmp/install-protoc.sh && /tmp/install-protoc.sh && rm /tmp/install-protoc.sh
