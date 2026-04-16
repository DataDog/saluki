FROM ubuntu:24.04

ARG USERNAME=saluki
ARG USER_UID=1000
ARG USER_GID=${USER_UID}

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

# Create non-root user with sudo access.
# Ubuntu 24.04 ships with a "ubuntu" user at uid/gid 1000 — remove it first.
RUN userdel -r ubuntu 2>/dev/null || true && \
    groupadd --gid ${USER_GID} ${USERNAME} 2>/dev/null || true && \
    useradd --uid ${USER_UID} --gid ${USER_GID} -m ${USERNAME} && \
    echo "${USERNAME} ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/${USERNAME} && \
    chmod 0440 /etc/sudoers.d/${USERNAME}

# Switch to non-root user for rustup and cargo setup.
USER ${USERNAME}
WORKDIR /home/${USERNAME}

# Install rustup with the "stable" toolchain as default.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain stable
ENV PATH="/home/${USERNAME}/.cargo/bin:${PATH}"

# Pre-install all the relevant helper tools we need.
COPY Makefile /home/${USERNAME}/Makefile
RUN make cargo-preinstall
