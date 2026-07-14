# aarch64 dynamically-linked glibc build.

rustup target add aarch64-unknown-linux-gnu

# When in "alternate build toolchain" mode for this architecture, update all the relevant
# environment variables that control which binary to use for CC/CXX/AR/etc so that we use
# the cross-compilation toolchain.
#
# This ensures that we use the right tool versions, glibc versions, and so on.
if [ -d /opt/toolchains/aarch64 ]; then
    _ctng=/opt/toolchains/aarch64/bin/aarch64-unknown-linux-gnu
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER="${_ctng}-gcc"
    export CC_aarch64_unknown_linux_gnu="${_ctng}-gcc"
    export CXX_aarch64_unknown_linux_gnu="${_ctng}-g++"
    export AR_aarch64_unknown_linux_gnu="${_ctng}-ar"
    TARGET_MAX_GLIBC="2.17"
fi

TARGET_CARGO_ARGS="--target aarch64-unknown-linux-gnu"
TARGET_CFLAGS=""
TARGET_RUSTFLAGS="-C link-arg=-static-libgcc"
TARGET_OUTPUT_DIR="/adp/target/aarch64-unknown-linux-gnu/${BUILD_PROFILE}"
