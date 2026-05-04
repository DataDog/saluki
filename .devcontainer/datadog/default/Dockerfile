# GBI images stand for "golden base image"
# Documentation: https://datadoghq.atlassian.net/wiki/x/V4Quu
# Available images: https://github.com/DataDog/dd-source/tree/main/etc/images/base
FROM registry.ddbuild.io/images/base/gbi-ubuntu_2404:release AS base

USER root

# Avoid interactive prompts during package installation.
ENV DEBIAN_FRONTEND=noninteractive

RUN --mount=type=bind,source=.ci/configure-apt-mirror.sh,target=/tmp/configure-apt-mirror.sh \
    sh /tmp/configure-apt-mirror.sh

# Install system-level build dependencies.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        gcc \
        g++ \
        make \
        curl \
        unzip \
        git \
        ca-certificates \
        pkg-config \
        jq \
        binutils \
        sudo \
    && rm -rf /var/lib/apt/lists/*

# Install protoc using the repo's own CI script.
COPY .ci/install-protoc.sh /tmp/install-protoc.sh
RUN chmod +x /tmp/install-protoc.sh && /tmp/install-protoc.sh && rm /tmp/install-protoc.sh
