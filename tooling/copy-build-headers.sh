#!/usr/bin/env sh
#
# Copies Linux headers from the host environment to the build target-specific include directory.
#
# This is necessary for providing the kernel headers to AWS-LC (and any other crate that requires them) when
# cross-compiling <arch>-unknown-linux-musl.

set -eux

# Take the BUILD_TARGET environment variable and map it to the shortened target triple used
# by `gcc` in terms of the specific include path we need to drop the headers in.
#
# We expect BUILD_TARGET to be set to the same target triple being passed to `rustc`.
case "${BUILD_TARGET}" in
    x86_64-unknown-linux-musl)
        GCC_TARGET_TRIPLE=x86_64-linux-gnu
        GCC_CROSS_TARGET_TRIPLE=x86_64-linux-musl
        ;;
    aarch64-unknown-linux-musl)
        GCC_TARGET_TRIPLE=aarch64-linux-gnu
        GCC_CROSS_TARGET_TRIPLE=aarch64-linux-musl
        ;;
    *)
        echo "Cross-compilation not required for '${BUILD_TARGET}'. No headers will be copied."
        exit 0
        ;;
esac

# Make sure the kernel headers are installed at their expected location.
if ! [ -d /usr/include/linux ] || ! [ -d /usr/include/asm-generic ] || ! [ -d /usr/include/${GCC_TARGET_TRIPLE} ]; then
    echo "Kernel headers not found!"
    exit 1
fi

# Copy over the kernel headers to the build target-specific include directory.
mkdir -p /usr/include/${GCC_CROSS_TARGET_TRIPLE}
cp -R /usr/include/linux /usr/include/${GCC_CROSS_TARGET_TRIPLE}/linux
cp -R /usr/include/asm-generic /usr/include/${GCC_CROSS_TARGET_TRIPLE}/asm-generic
cp -R /usr/include/${GCC_TARGET_TRIPLE}/asm /usr/include/${GCC_CROSS_TARGET_TRIPLE}/asm
