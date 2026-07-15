# x86_64 dynamically-linked glibc build.

rustup target add x86_64-unknown-linux-gnu

# When in "alternate build toolchain" mode for this architecture, update all the relevant
# environment variables that control which binary to use for CC/CXX/AR/etc so that we use
# the cross-compilation toolchain.
#
# This ensures that we use the right tool versions, glibc versions, and so on.
if [ -d /opt/toolchains/x86_64 ]; then
    _ctng=/opt/toolchains/x86_64/bin/x86_64-unknown-linux-gnu
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="${_ctng}-gcc"
    export CC_x86_64_unknown_linux_gnu="${_ctng}-gcc"
    export CXX_x86_64_unknown_linux_gnu="${_ctng}-g++"
    export AR_x86_64_unknown_linux_gnu="${_ctng}-ar"
    # https://github.com/DataDog/datadog-agent-buildimages/blob/6431ee46dba53a45dce472da57856e3b6bd2d494/docker-bake.hcl#L118
    TARGET_MAX_GLIBC="2.17"
fi

TARGET_CARGO_ARGS="--target x86_64-unknown-linux-gnu"
TARGET_CFLAGS=""
TARGET_RUSTFLAGS="-C link-arg=-static-libgcc"
TARGET_OUTPUT_DIR="/adp/target/x86_64-unknown-linux-gnu/${BUILD_PROFILE}"
