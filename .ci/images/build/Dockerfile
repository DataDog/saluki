FROM ubuntu:14.04 AS bolt-builder

ARG DEBIAN_FRONTEND=noninteractive
ENV TZ=UTC

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential software-properties-common curl ca-certificates git gnupg2 unzip ninja-build python3 libjemalloc-dev python3-psutil libssl-dev
RUN add-apt-repository ppa:ubuntu-toolchain-r/test -y && apt-get update
RUN apt-get install -y --no-install-recommends gcc-9 g++-9 && \
    update-alternatives --install /usr/bin/gcc gcc /usr/bin/gcc-9 60 --slave /usr/bin/g++ g++ /usr/bin/g++-9 && \
    update-alternatives --install /usr/bin/ld ld /usr/bin/ld.gold 20
RUN apt-get remove cmake

RUN mkdir /tmp/cmake && \
    cd /tmp/cmake && \
    curl -L -O https://github.com/Kitware/CMake/releases/download/v3.29.6/cmake-3.29.6.tar.gz && \
    tar -xzvf cmake-3.29.6.tar.gz
RUN cd /tmp/cmake/cmake-3.29.6 && \
    ./bootstrap && \
    make -j16 && \
    sudo make install

WORKDIR /home/bolt

RUN git clone --depth 1 --branch llvmorg-18.1.7 https://github.com/llvm/llvm-project

RUN mkdir build && \
    cd build && \
    cmake -G Ninja ../llvm-project/llvm \
      -DLLVM_ENABLE_PROJECTS="bolt;clang;lld" \
      -DLLVM_TARGETS_TO_BUILD="X86;AArch64" \
      -DCMAKE_BUILD_TYPE=Release \
      -DLLVM_ENABLE_ASSERTIONS=ON \
      -DCMAKE_EXE_LINKER_FLAGS="-Wl,--push-state -Wl,-whole-archive -ljemalloc_pic -Wl,--pop-state -lpthread -lstdc++ -lm -ldl" \
      -DCMAKE_INSTALL_PREFIX=/home/bolt/install && \
    ninja check-bolt && \
    ninja install-llvm-bolt install-perf2bolt install-merge-fdata \
      install-llvm-boltdiff install-bolt_rt

FROM ubuntu:14.04

ARG RUST_VERSION=1.79.0

# Install basic utilities and an updated compiler/binutils toolchain, which is necessary for compiling.
RUN apt-get update && \
    apt-get install -y --no-install-recommends build-essential software-properties-common curl ca-certificates git gnupg2 cmake3 unzip && \
    add-apt-repository ppa:ubuntu-toolchain-r/test -y && \
    apt-get update && \
    apt-get install -y --no-install-recommends gcc-8 g++-8 binutils-2.26 && \
    update-alternatives --install /usr/bin/gcc gcc /usr/bin/gcc-8 60 --slave /usr/bin/g++ g++ /usr/bin/g++-8 && \
    rm -rf /var/lib/apt/lists/*
ENV PATH="/usr/lib/binutils-2.26/bin:${PATH}"

# Install Protocol Buffers compiler by hand, since Ubuntu 14.04 does not have a recent enough version.
COPY .ci/install-protoc.sh /
RUN chmod +x /install-protoc.sh && /install-protoc.sh

# Install awscli by hand, for similar reasons to the Protocol Buffers compiler.
COPY .ci/install-awscli.sh /
RUN chmod +x /install-awscli.sh && /install-awscli.sh

# Install Rust and common Cargo tooling that we depend on.
RUN curl https://sh.rustup.rs -sSf | sh -s -- -y --default-toolchain ${RUST_VERSION}
ENV PATH="/root/.cargo/bin:${PATH}"
RUN rustup component add clippy && \
    rustup component add llvm-tools && \
    cargo install cargo-nextest --version 0.9.68 && \
    cargo install cargo-hack --version 0.6.27 && \
    cargo install cargo-deny --version 0.14.20 && \
    cargo install dd-rust-license-tool --version 1.0.3

# Install BOLT, used for post-link optimization of the ADP binary.
COPY --from=bolt-builder /home/bolt/install /usr/local

COPY .ci/images/build/entrypoint.sh /
RUN chmod +x /entrypoint.sh
ENTRYPOINT [ "/entrypoint.sh" ]
