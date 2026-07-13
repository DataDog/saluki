# aarch64 dynamically-linked glibc build.
#
# Built against the old glibc floor (glibc 2.23) provided by the Datadog Agent build image, which
# ships crosstool-NG toolchains under /opt/toolchains. We route this target's C/C++ compiler, archiver,
# and Rust linker to that toolchain so the C dependencies (AWS-LC, jemalloc, zstd) compile against, and
# the binary links against, its old-glibc sysroot -- which is what pins the glibc floor. AWS-LC's cmake
# build takes its C compiler from these cc-rs env vars, so it is covered too.
#
# The routing is guarded on /opt/toolchains being present, so builds outside the Agent image (the
# benchmark's plain-Ubuntu build, or a local host build) fall back to the native toolchain. The floor
# only matters for shipped artifacts, which are always built on the Agent image.
#
# Unlike aarch64 musl, this needs no -mno-outline-atomics: glibc provides __getauxval (since glibc 2.16),
# so outline-atomics runtime detection works and jemalloc compiles without it. -static-libgcc statically
# links libgcc so the binary carries no libgcc_s.so.1 runtime dependency, leaving glibc as the only
# dynamic dependency.

rustup target add aarch64-unknown-linux-gnu

if [ -d /opt/toolchains/aarch64 ]; then
    _ctng=/opt/toolchains/aarch64/bin/aarch64-unknown-linux-gnu
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="${_ctng}-gcc"
    export CC_aarch64_unknown_linux_gnu="${_ctng}-gcc"
    export CXX_aarch64_unknown_linux_gnu="${_ctng}-g++"
    export AR_aarch64_unknown_linux_gnu="${_ctng}-ar"
fi

TARGET_CARGO_ARGS="--target aarch64-unknown-linux-gnu"
TARGET_CFLAGS=""
TARGET_RUSTFLAGS="-C link-arg=-static-libgcc"
TARGET_OUTPUT_DIR="/adp/target/aarch64-unknown-linux-gnu/${BUILD_PROFILE}"
