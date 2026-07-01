# ARMv7 GNU hard-float build.
#
# Datadog IoT Agent's ARMv7 packages target Debian armhf / RPM armv7hl, which maps to
# Rust's armv7-unknown-linux-gnueabihf target.

rustup target add armv7-unknown-linux-gnueabihf

if ! command -v arm-linux-gnueabihf-gcc >/dev/null 2>&1; then
    apt-get update
    apt-get install --no-install-recommends -y \
        gcc-arm-linux-gnueabihf \
        g++-arm-linux-gnueabihf \
        libc6-dev-armhf-cross
    apt-get clean
    rm -rf /var/lib/apt/lists/*
fi

export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER=arm-linux-gnueabihf-gcc
export CC_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-gcc
export CXX_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-g++
export AR_armv7_unknown_linux_gnueabihf=arm-linux-gnueabihf-ar

TARGET_CARGO_ARGS="--target armv7-unknown-linux-gnueabihf"
TARGET_OUTPUT_DIR="/adp/target/armv7-unknown-linux-gnueabihf/${BUILD_PROFILE}"
